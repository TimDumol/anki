// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::path::Path;

use anki::collection::Collection;
use anki::collection::CollectionBuilder;
use anki::notetype::NotetypeId;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub(crate) fn history(col: &Collection) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    for sql in [
        "select id,nid,did,ord,type,queue,due,ivl,factor,reps,lapses,left,odue,odid,flags,data from cards order by id",
        "select * from revlog order by id",
    ] {
        let mut statement = col.storage.db().prepare(sql)?;
        let count = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..count {
                hash.update(format!("{:?}\0", row.get_ref(column)?).as_bytes());
            }
        }
    }
    Ok(hash.finalize().to_hex().to_string())
}

pub(crate) fn original_notes(col: &Collection) -> Result<Vec<anki::notes::Note>> {
    col.storage
        .get_all_note_ids()?
        .into_iter()
        .map(|id| col.storage.get_note(id)?.context("note missing"))
        .collect()
}

pub(crate) fn verify(
    col: &Collection,
    notes: &[anki::notes::Note],
    before_history: &str,
) -> Result<()> {
    ensure!(
        history(col)? == before_history,
        "card IDs, scheduling or reviews changed"
    );
    ensure!(
        col.storage.get_all_note_ids()?.len() == notes.len(),
        "note count changed"
    );
    for before in notes {
        let after = col.storage.get_note(before.id)?.context("note lost")?;
        ensure!(
            after.guid == before.guid
                && after.notetype_id == before.notetype_id
                && after.tags == before.tags
                && after.fields().get(..before.fields().len()) == Some(before.fields().as_slice()),
            "original note content changed"
        );
    }
    Ok(())
}

/// Only the host maintenance coordinator invokes this command, with public sync
/// blocked, the MCP daemon stopped and both volumes already backed up.
pub async fn run() -> Result<()> {
    let path = std::path::PathBuf::from(std::env::var("ANKI_COLLECTION")?);
    let mut request = load(&path)?.context("no maintenance request")?;
    ensure!(
        request.state == "requested",
        "ambiguous migration must be reconciled by operator"
    );
    let backup = std::env::var("ANKI_MAINTENANCE_BACKUP")?;
    ensure!(
        backup.starts_with("/opt/anki/rollback/migration-"),
        "consistent host backup required"
    );
    request.backup = Some(backup);
    save(&path, &request)?;
    let result = perform(&path, &mut request).await;
    if result.is_err() {
        request.state = if request.state == "requested" {
            "not_applied"
        } else {
            "reconciliation_required"
        }
        .into();
        save(&path, &request)?;
    }
    result
}

async fn perform(path: &Path, request: &mut Request) -> Result<()> {
    use anki::sync::collection::normal::SyncActionRequired;
    use anki::sync::http_client::HttpSyncClient;
    use anki::sync::login::sync_login;
    use anki::sync::media::syncer::MediaSyncer;

    let journal = crate::writes::Journal::open(&path.with_extension("mcp-journal.sqlite"))?;
    ensure!(journal.pending()? == 0, "resolve ordinary operations first");
    let endpoint = std::env::var("ANKI_SYNC_ENDPOINT")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let password = std::fs::read_to_string(std::env::var("ANKI_SYNC_PASSWORD_FILE")?)?;
    let mut auth = sync_login(
        std::env::var("ANKI_SYNC_USERNAME")?,
        password.trim_end().to_string(),
        Some(endpoint.clone()),
        client.clone(),
    )
    .await?;
    auth.endpoint = Some(endpoint.parse()?);
    let mut col = CollectionBuilder::new(path)
        .with_desktop_media_paths()
        .set_check_integrity(true)
        .build()?;
    let output = col.normal_sync(auth.clone(), client.clone()).await?;
    ensure!(
        output.required == SyncActionRequired::NoChanges,
        "resolve existing full sync before migration"
    );
    MediaSyncer::new(
        col.media()?,
        col.new_progress_handler(),
        HttpSyncClient::new(auth.clone(), client.clone()),
    )?
    .sync(None)
    .await?;
    let preview = preview(
        &mut col,
        request.notetype_id,
        request.template_update.as_ref(),
    )?;
    ensure!(
        preview["expected_revision"] == request.expected_revision
            && preview["card_generation_changed_in_clone"] == false,
        "stale or unsafe migration"
    );
    let notes = original_notes(&col)?;
    let before_history = history(&col)?;
    let nt = col
        .get_notetype(NotetypeId(request.notetype_id))?
        .context("note type missing")?;
    let mut after = match &request.template_update {
        Some(input) => crate::templates::updated(&nt, input)?,
        None => crate::contexts::migrated(&nt)?,
    };
    // Persist the irreversible phase before changing the collection. A crash here
    // requires operator reconciliation, never an automatic second full upload.
    request.preview = preview;
    request.state = "prepared".into();
    save(path, request)?;
    col.update_notetype(&mut after, false)?;
    verify(&col, &notes, &before_history)?;
    request.state = "local_applied".into();
    save(path, request)?;
    col.full_upload(auth.clone(), client.clone()).await?;
    request.state = "uploaded_verifying".into();
    save(path, request)?;
    // Verify what the native server actually stored, using a fresh download.
    let temp = tempfile::tempdir_in(path.parent().context("collection parent")?)?;
    let verify_path = temp.path().join("server.anki2");
    CollectionBuilder::new(&verify_path)
        .build()?
        .full_download(auth, client)
        .await?;
    let mut downloaded = CollectionBuilder::new(&verify_path)
        .set_check_integrity(true)
        .build()?;
    verify(&downloaded, &notes, &before_history)?;
    let server_type = downloaded
        .get_notetype(NotetypeId(request.notetype_id))?
        .context("server note type missing")?;
    ensure!(
        server_type
            .fields
            .iter()
            .map(|f| &f.name)
            .eq(after.fields.iter().map(|f| &f.name))
            && server_type.templates.len() == after.templates.len()
            && server_type.config.css == after.config.css
            && server_type
                .templates
                .iter()
                .zip(&after.templates)
                .all(|(a, b)| a.config.q_format == b.config.q_format
                    && a.config.a_format == b.config.a_format),
        "server template mismatch"
    );
    downloaded.close(None)?;
    request.state = "synced_successfully".into();
    save(path, request)?;
    println!("{}", serde_json::to_value(request)?);
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub subject: String,
    pub idempotency_key: String,
    pub notetype_id: i64,
    pub expected_revision: String,
    pub state: String,
    pub preview: Value,
    pub backup: Option<String>,
    #[serde(default)]
    pub template_update: Option<crate::templates::Update>,
}

pub fn load(collection: &Path) -> Result<Option<Request>> {
    let path = collection.with_extension("maintenance.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    ensure!(bytes.len() <= 512 * 1024, "maintenance manifest too large");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub fn blocked(collection: &Path) -> Result<bool> {
    Ok(load(collection)?
        .is_some_and(|r| !matches!(r.state.as_str(), "synced_successfully" | "not_applied")))
}

pub fn save(collection: &Path, request: &Request) -> Result<()> {
    use std::io::Write;
    let parent = collection.parent().unwrap();
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec(request)?)?;
    file.as_file().sync_all()?;
    file.persist(collection.with_extension("maintenance.json"))?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn archive_path(collection: &Path, subject: &str, key: &str) -> std::path::PathBuf {
    collection.with_extension(format!(
        "maintenance-{}.json",
        blake3::hash(serde_json::json!([subject, key]).to_string().as_bytes()).to_hex()
    ))
}

pub fn get(collection: &Path, subject: &str, key: &str) -> Result<Option<Request>> {
    if let Some(request) = load(collection)? {
        if request.subject == subject && request.idempotency_key == key {
            return Ok(Some(request));
        }
    }
    let path = archive_path(collection, subject, key);
    if path.exists() {
        Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
    } else {
        Ok(None)
    }
}

pub fn enqueue(collection: &Path, request: Request) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&request)?.len() <= 512 * 1024,
        "maintenance manifest too large"
    );
    ensure!(
        !request.idempotency_key.is_empty() && request.idempotency_key.len() <= 128,
        "invalid idempotency key"
    );
    if let Some(previous) = get(collection, &request.subject, &request.idempotency_key)? {
        ensure!(
            previous.notetype_id == request.notetype_id
                && previous.expected_revision == request.expected_revision
                && previous.template_update == request.template_update,
            "idempotency key reused"
        );
        return Ok(serde_json::to_value(previous)?);
    }
    if let Some(previous) = load(collection)? {
        if previous.subject == request.subject
            && previous.idempotency_key == request.idempotency_key
        {
            ensure!(
                previous.notetype_id == request.notetype_id
                    && previous.expected_revision == request.expected_revision
                    && previous.template_update == request.template_update,
                "idempotency key reused"
            );
            return Ok(serde_json::to_value(previous)?);
        }
        ensure!(!blocked(collection)?, "maintenance already in progress");
        let archive = archive_path(collection, &previous.subject, &previous.idempotency_key);
        ensure!(!archive.exists(), "migration key already archived");
        std::fs::copy(collection.with_extension("maintenance.json"), archive)?;
    }
    save(collection, &request)?;
    Ok(serde_json::to_value(request)?)
}

pub fn preview(
    col: &mut Collection,
    id: i64,
    update: Option<&crate::templates::Update>,
) -> Result<Value> {
    match update {
        Some(input) => {
            ensure!(input.notetype_id == id, "note type mismatch");
            crate::templates::preview(col, input)
        }
        None => crate::contexts::migration_preview(col, id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn confirmed_migration_round_trips_with_backup_manifest_and_retry_receipt() -> Result<()>
    {
        use anki::decks::DeckId;
        use anki::notetype::CardTemplate;
        use anki::notetype::NoteField;
        use anki::search::SortMode;
        use anki::sync::http_server::default_ip_header;
        use anki::sync::http_server::SimpleServer;
        use anki::sync::http_server::SyncServerConfig;
        use anki::sync::login::sync_login;

        let temp = tempfile::tempdir()?;
        std::env::set_var("SYNC_USER1", "fixture:fixture-password");
        let (address, server) = SimpleServer::make_server(SyncServerConfig {
            host: "127.0.0.1".parse()?,
            port: 0,
            base_folder: temp.path().join("server"),
            ip_header: default_ip_header(),
        })
        .await
        .unwrap();
        let task = tokio::spawn(server);
        let endpoint = format!("http://{address}/");
        let client = reqwest::Client::builder().no_proxy().build()?;
        let mut auth = sync_login(
            "fixture",
            "fixture-password",
            Some(endpoint.clone()),
            client.clone(),
        )
        .await?;
        auth.endpoint = Some(endpoint.parse()?);
        let path = temp.path().join("collection.anki2");
        let mut col = CollectionBuilder::new(&path)
            .with_desktop_media_paths()
            .build()?;
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/readest-v3.json"))?;
        let mut nt = col.get_notetype_by_name("Basic")?.unwrap().as_ref().clone();
        nt.id = NotetypeId(0);
        nt.name = fixture["name"].as_str().unwrap().into();
        nt.fields = fixture["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| NoteField::new(f.as_str().unwrap()))
            .collect();
        nt.templates = fixture["templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                CardTemplate::new(
                    t["name"].as_str().unwrap(),
                    t["front"].as_str().unwrap(),
                    t["back"].as_str().unwrap(),
                )
            })
            .collect();
        nt.config.css = fixture["css"].as_str().unwrap().into();
        col.add_notetype(&mut nt, false)?;
        let nt = col.get_notetype(nt.id)?.unwrap();
        let mut note = nt.new_note();
        for index in 0..nt.fields.len() {
            note.set_field(index, "original quotation or value")?;
        }
        col.add_note(&mut note, DeckId(1))?;
        let cards = col.search_cards(note.id, SortMode::NoOrder)?;
        col.storage.db().execute(
            "insert into revlog values (1234567890000,?1,-1,3,10,5,2500,1234,1)",
            [cards[0].0],
        )?;
        col.full_upload(auth, client).await?;
        let mut col = CollectionBuilder::new(&path).build()?;
        let preview = crate::contexts::migration_preview(&mut col, nt.id.0)?;
        col.close(None)?;
        let request = Request {
            subject: "owner".into(),
            idempotency_key: "migration-1".into(),
            notetype_id: nt.id.0,
            expected_revision: preview["expected_revision"].as_str().unwrap().into(),
            state: "requested".into(),
            preview,
            backup: None,
            template_update: None,
        };
        enqueue(&path, request.clone())?;
        assert!(blocked(&path)?);
        assert!(enqueue(
            &path,
            Request {
                idempotency_key: "migration-other".into(),
                ..request.clone()
            }
        )
        .is_err());
        let password = temp.path().join("password");
        std::fs::write(&password, "fixture-password")?;
        std::env::set_var("ANKI_COLLECTION", &path);
        std::env::set_var("ANKI_SYNC_ENDPOINT", endpoint);
        std::env::set_var("ANKI_SYNC_USERNAME", "fixture");
        std::env::set_var("ANKI_SYNC_PASSWORD_FILE", password);
        std::env::set_var(
            "ANKI_MAINTENANCE_BACKUP",
            "/opt/anki/rollback/migration-test",
        );
        run().await?;
        assert!(!blocked(&path)?);
        let receipt = get(&path, "owner", "migration-1")?.unwrap();
        assert_eq!(receipt.state, "synced_successfully");
        assert_eq!(
            receipt.backup.as_deref(),
            Some("/opt/anki/rollback/migration-test")
        );
        assert_eq!(
            enqueue(&path, request.clone())?["state"],
            "synced_successfully"
        );
        let col = CollectionBuilder::new(&path).build()?;
        assert_eq!(
            col.storage
                .get_note(note.id)?
                .unwrap()
                .fields()
                .last()
                .unwrap(),
            ""
        );
        col.close(None)?;
        assert!(run().await.is_err());
        enqueue(
            &path,
            Request {
                idempotency_key: "migration-2".into(),
                ..request.clone()
            },
        )?;
        assert_eq!(
            get(&path, "owner", "migration-1")?.unwrap().state,
            "synced_successfully"
        );
        assert!(get(&path, "another-owner", "migration-1")?.is_none());
        assert_eq!(enqueue(&path, request)?["state"], "synced_successfully");
        // A stale request fails before changing data and safely releases the
        // maintenance barrier. An ambiguous phase never retries an upload.
        assert!(run().await.is_err());
        assert_eq!(load(&path)?.unwrap().state, "not_applied");
        assert!(!blocked(&path)?);
        let mut ambiguous = load(&path)?.unwrap();
        ambiguous.state = "local_applied".into();
        save(&path, &ambiguous)?;
        assert!(run().await.is_err());
        assert!(blocked(&path)?);
        ambiguous.state = "not_applied".into();
        save(&path, &ambiguous)?;
        let mut col = CollectionBuilder::new(&path).build()?;
        let current_type = col.get_notetype(nt.id)?.unwrap();
        let input = crate::templates::Update {
            notetype_id: nt.id.0,
            templates: vec![crate::templates::Template {
                ordinal: 0,
                front: Some(format!(
                    "<main>{}</main>",
                    current_type.templates[0].config.q_format
                )),
                back: Some(format!(
                    "<article>{}</article>",
                    current_type.templates[0].config.a_format
                )),
            }],
            css: Some(".card { color: navy; }".into()),
        };
        let preview = crate::templates::preview(&mut col, &input)?;
        col.close(None)?;
        let template_request = Request {
            subject: "owner".into(),
            idempotency_key: "template-1".into(),
            notetype_id: nt.id.0,
            expected_revision: preview["expected_revision"].as_str().unwrap().into(),
            state: "requested".into(),
            preview,
            backup: None,
            template_update: Some(input.clone()),
        };
        enqueue(&path, template_request.clone())?;
        run().await?;
        assert_eq!(
            get(&path, "owner", "template-1")?.unwrap().state,
            "synced_successfully"
        );
        assert_eq!(
            enqueue(&path, template_request.clone())?["state"],
            "synced_successfully"
        );
        let mut conflict = template_request;
        conflict.template_update.as_mut().unwrap().css = Some("different".into());
        assert!(enqueue(&path, conflict).is_err());
        let mut col = CollectionBuilder::new(&path).build()?;
        let saved = col.get_notetype(nt.id)?.unwrap();
        assert_eq!(saved.config.css, input.css.unwrap());
        assert_eq!(
            saved.templates[0].config.q_format,
            input.templates[0].front.as_ref().unwrap().as_str()
        );
        assert_eq!(
            saved.templates[0].config.a_format,
            input.templates[0].back.as_ref().unwrap().as_str()
        );
        col.close(None)?;
        task.abort();
        Ok(())
    }
}
