// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::path::PathBuf;

use anki::collection::CollectionBuilder;
use anki::sync::collection::normal::SyncActionRequired;
use anki::sync::http_client::HttpSyncClient;
use anki::sync::login::sync_login;
use anki::sync::media::syncer::MediaSyncer;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use reqwest::Client;

/// Disposable Rust-only sync client. Never use the sync server's data
/// directory.
#[derive(Parser)]
struct Args {
    #[arg(long)]
    collection: PathBuf,
    #[arg(long)]
    endpoint: Option<String>,
    /// Restore a verified export into a new disposable collection and report
    /// metadata only.
    #[arg(long)]
    inspect_export: Option<PathBuf>,
    #[arg(long)]
    inspect_collection: bool,
    #[arg(long)]
    inspect_notetype: Option<i64>,
    /// Only permitted for a collection path which does not yet exist.
    #[arg(long)]
    bootstrap: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(id) = args.inspect_notetype {
        if !args.collection.exists() {
            bail!("inspection requires an existing disposable restored collection");
        }
        let mut col = CollectionBuilder::new(&args.collection).build()?;
        let nt = col
            .get_notetype(anki::notetype::NotetypeId(id))?
            .context("note type missing")?;
        println!(
            "{}",
            serde_json::json!({"id":id,"name":nt.name,"fields":nt.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),"templates":nt.templates.iter().map(|t| serde_json::json!({"name":t.name,"front":t.config.q_format,"back":t.config.a_format})).collect::<Vec<_>>(),"css":nt.config.css})
        );
        col.close(None)?;
        return Ok(());
    }
    if args.inspect_collection {
        if !args.collection.exists() {
            bail!("inspection requires an existing disposable restored collection");
        }
        let mut builder = CollectionBuilder::new(&args.collection);
        builder.with_desktop_media_paths().set_check_integrity(true);
        let col = builder.build()?;
        let mut counts = serde_json::Map::new();
        for table in ["notes", "cards", "revlog"] {
            let count: i64 = col.storage.db().query_row(
                &format!("select count(*) from {table}"),
                [],
                |row| row.get(0),
            )?;
            counts.insert(table.into(), count.into());
        }
        println!("{}", serde_json::json!({"integrity":"ok", "counts":counts}));
        col.close(None)?;
        return Ok(());
    }
    if let Some(export) = args.inspect_export {
        if args.collection.exists() {
            bail!("export inspection refuses to replace an existing collection");
        }
        std::fs::create_dir_all(
            args.collection
                .parent()
                .context("collection parent missing")?,
        )?;
        let mut builder = CollectionBuilder::new(&args.collection);
        builder.with_desktop_media_paths();
        let col = builder.build()?;
        let progress = col.new_progress_handler();
        col.close(None)?;
        anki::import_export::package::import_colpkg(
            &export.to_string_lossy(),
            &args.collection.to_string_lossy(),
            &args.collection.with_extension("media"),
            &args.collection.with_extension("media.db2"),
            progress,
        )?;
        let mut col = builder.build()?;
        let mut counts = serde_json::Map::new();
        for table in ["notes", "cards", "revlog"] {
            let count: i64 = col.storage.db().query_row(
                &format!("select count(*) from {table}"),
                [],
                |row| row.get(0),
            )?;
            counts.insert(table.into(), count.into());
        }
        let schemas: Vec<_> = col
            .get_all_notetypes()?
            .iter()
            .map(|nt| {
                serde_json::json!({"id":nt.id.0, "name":nt.name,
                "fields":nt.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),
                "templates":nt.templates.iter().map(|t| &t.name).collect::<Vec<_>>()})
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"counts":counts,"notetypes":schemas})
        );
        col.close(None)?;
        return Ok(());
    }
    let endpoint = args
        .endpoint
        .context("--endpoint is required for synchronization")?;
    if args.bootstrap && args.collection.exists() {
        bail!("bootstrap refuses to replace an existing collection");
    }
    if !args.bootstrap && !args.collection.exists() {
        bail!("new clients require an explicit download-only bootstrap");
    }
    let username = std::env::var("ANKI_SYNC_USERNAME").context("ANKI_SYNC_USERNAME missing")?;
    let password_file =
        std::env::var("ANKI_SYNC_PASSWORD_FILE").context("ANKI_SYNC_PASSWORD_FILE missing")?;
    let password = std::fs::read_to_string(password_file)?;
    let client = Client::builder().build()?;
    let mut auth = sync_login(
        username,
        password.trim_end().to_owned(),
        Some(endpoint.clone()),
        client.clone(),
    )
    .await?;
    // Upstream login returns the host key without retaining the custom endpoint.
    auth.endpoint = Some(endpoint.parse()?);
    let mut builder = CollectionBuilder::new(&args.collection);
    builder.with_desktop_media_paths().set_check_integrity(true);
    let mut col = builder.build()?;
    if args.bootstrap {
        col.full_download(auth.clone(), client.clone()).await?;
        col = builder.build()?;
    }
    let result = col.normal_sync(auth.clone(), client.clone()).await?;
    if !matches!(result.required, SyncActionRequired::NoChanges) {
        bail!(
            "explicit maintenance required: {:?}; no automatic full upload/download",
            result.required
        );
    }
    MediaSyncer::new(
        col.media()?,
        col.new_progress_handler(),
        HttpSyncClient::new(auth, client),
    )?
    .sync(None)
    .await?;
    let notes = col.search_notes_unordered("")?;
    println!(
        "{}",
        serde_json::json!({"status": "synced_successfully", "notes": notes.len()})
    );
    col.close(None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use anki::decks::DeckId;
    use anki::search::SortMode;
    use anki::sync::http_server::default_ip_header;
    use anki::sync::http_server::SimpleServer;
    use anki::sync::http_server::SyncServerConfig;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn native_sync_preserves_note_identity_after_edit_and_restart() -> Result<()> {
        let data = tempdir()?;
        // This binary has one integration test, so the server's environment is
        // isolated.
        std::env::set_var("SYNC_USER1", "fixture:fixture-password");
        let (address, server) = SimpleServer::make_server(SyncServerConfig {
            host: "127.0.0.1".parse()?,
            port: 0,
            base_folder: data.path().join("server"),
            ip_header: default_ip_header(),
        })
        .await
        .unwrap();
        let server_task = tokio::spawn(server);
        let endpoint =
            std::env::var("ANKI_TEST_ENDPOINT").unwrap_or_else(|_| format!("http://{address}/"));
        let password = match std::env::var("ANKI_TEST_ENDPOINT") {
            Ok(_) => std::fs::read_to_string(std::env::var("ANKI_TEST_PASSWORD_FILE")?)?,
            Err(_) => "fixture-password".to_owned(),
        };
        let client = Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let mut auth = sync_login(
            "fixture",
            password.trim_end(),
            Some(endpoint.clone()),
            client.clone(),
        )
        .await
        .context("fixture native login")?;
        auth.endpoint = Some(endpoint.parse()?);
        let master_path = data.path().join("master.anki2");
        let mut master_builder = CollectionBuilder::new(&master_path);
        master_builder.with_desktop_media_paths();
        let mut master = master_builder.build()?;
        let nt = master
            .get_notetype_by_name("Basic")?
            .context("stock Basic missing")?;
        let mut note = nt.new_note();
        note.set_field(0, "original quotation")?;
        note.set_field(1, "original answer")?;
        master.add_note(&mut note, DeckId(1))?;
        let note_id = note.id;
        let original_cards = master.search_cards("", SortMode::NoOrder)?;
        // A fixed review record is fixture setup only, never an MCP operation.
        master.storage.db().execute(
            "insert into revlog values (1234567890000, ?1, -1, 3, 10, 5, 2500, 1234, 1)",
            [original_cards[0].0],
        )?;
        let media_content = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        std::fs::write(
            master_path.with_extension("media").join("fixture.svg"),
            media_content,
        )?;
        MediaSyncer::new(
            master.media()?,
            master.new_progress_handler(),
            HttpSyncClient::new(auth.clone(), client.clone()),
        )?
        .sync(None)
        .await
        .context("fixture media upload")?;
        // Only the disposable authoritative fixture seeds this empty server.
        master
            .full_upload(auth.clone(), client.clone())
            .await
            .context("fixture full upload")?;
        let sidecar_path = data.path().join("sidecar.anki2");
        let mut sidecar_builder = CollectionBuilder::new(&sidecar_path);
        sidecar_builder.with_desktop_media_paths();
        sidecar_builder
            .build()?
            .full_download(auth.clone(), client.clone())
            .await
            .context("sidecar full download")?;
        let mut sidecar = sidecar_builder.build()?;
        MediaSyncer::new(
            sidecar.media()?,
            sidecar.new_progress_handler(),
            HttpSyncClient::new(auth.clone(), client.clone()),
        )?
        .sync(None)
        .await
        .context("sidecar media download")?;
        assert_eq!(
            std::fs::read(sidecar_path.with_extension("media").join("fixture.svg"))?,
            media_content,
        );
        let mut downloaded = sidecar
            .storage
            .get_note(note_id)?
            .context("downloaded note missing")?;
        assert_eq!(downloaded.fields(), note.fields());
        downloaded.set_field(1, "updated answer")?;
        sidecar.update_note(&mut downloaded)?;
        sidecar.close(None)?;
        // Restart before remote sync: the mutation must persist locally.
        let mut sidecar = sidecar_builder.build()?;
        assert_eq!(
            sidecar.storage.get_note(note_id)?.unwrap().fields()[1],
            "updated answer"
        );
        let result = sidecar
            .normal_sync(auth.clone(), client.clone())
            .await
            .context("sidecar incremental upload")?;
        assert_eq!(result.required, SyncActionRequired::NoChanges);
        sidecar.close(None)?;
        let mut master = master_builder.build()?;
        let result = master
            .normal_sync(auth, client)
            .await
            .context("master incremental download")?;
        assert_eq!(result.required, SyncActionRequired::NoChanges);
        let received = master
            .storage
            .get_note(note_id)?
            .context("note missing after sync")?;
        assert_eq!(received.fields(), &["original quotation", "updated answer"]);
        assert_eq!(master.search_cards("", SortMode::NoOrder)?, original_cards);
        let review: (i64, i64, i64, i64, i64, i64) = master.storage.db().query_row(
            "select cid, ease, ivl, lastIvl, factor, time from revlog where id=1234567890000",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        assert_eq!(review, (original_cards[0].0, 3, 10, 5, 2500, 1234));
        master.close(None)?;
        server_task.abort();
        Ok(())
    }
}
