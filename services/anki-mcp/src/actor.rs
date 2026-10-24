// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anki::collection::Collection;
use anki::collection::CollectionBuilder;
use anki::notetype::NotetypeId;
use anki::search::SortMode;
use anki::sync::collection::normal::SyncActionRequired;
use anki::sync::http_client::HttpSyncClient;
use anki::sync::login::sync_login;
use anki::sync::media::syncer::MediaSyncer;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::authoring;
use crate::contexts;
use crate::failure::Failure;
use crate::maintenance;
use crate::writes;
use crate::writes::Journal;

pub enum Command {
    Status,
    Search {
        query: String,
        offset: usize,
        limit: usize,
        fields: Option<Vec<String>>,
    },
    Note(i64),
    Notes {
        note_ids: Vec<i64>,
        fields: Option<Vec<String>>,
    },
    AuthoringContext,
    CreatePreview {
        subject: String,
        notes: Vec<writes::Create>,
    },
    Decks,
    Notetype(i64),
    Preview {
        subject: String,
        updates: Vec<writes::Update>,
    },
    Apply {
        subject: String,
        token: String,
        key: String,
        confirmed: bool,
    },
    Create {
        subject: String,
        input: writes::Create,
        confirmed: bool,
    },
    Operation {
        subject: String,
        key: String,
    },
    MigrationPreview(i64),
    TemplatePreview(crate::templates::Update),
    MigrationApply {
        subject: String,
        notetype_id: i64,
        revision: String,
        template_update: Option<crate::templates::Update>,
        key: String,
        confirmed: bool,
    },
    ContextPreview {
        subject: String,
        input: contexts::Enrichment,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anki::decks::DeckId;
    use anki::sync::http_server::default_ip_header;
    use anki::sync::http_server::SimpleServer;
    use anki::sync::http_server::SyncServerConfig;

    use super::*;

    fn worker(path: PathBuf, endpoint: String, enabled: bool) -> Result<Worker> {
        Ok(Worker {
            journal: Journal::open(&path.with_extension("mcp-journal.sqlite"))?,
            writes_enabled: enabled,
            allow_readonly_full_download: true,
            path,
            endpoint,
            username: "fixture".into(),
            password: "fixture-password".into(),
            col: None,
            last_sync: None,
            sync_failed: false,
            client: Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()?,
        })
    }

    #[tokio::test]
    async fn offline_commit_retry_and_restart_sync_once_without_losing_reviews() -> Result<()> {
        let temp = tempfile::tempdir()?;
        // The only test in this binary that constructs the upstream server from env.
        std::env::set_var("SYNC_USER1", "fixture:fixture-password");
        let server_path = temp.path().join("server");
        let (address, server) = SimpleServer::make_server(SyncServerConfig {
            host: "127.0.0.1".parse()?,
            port: 0,
            base_folder: server_path.clone(),
            ip_header: default_ip_header(),
        })
        .await
        .unwrap();
        let task = tokio::spawn(server);
        let endpoint = format!("http://{address}/");
        let master_path = temp.path().join("master.anki2");
        let mut master = CollectionBuilder::new(&master_path)
            .with_desktop_media_paths()
            .build()?;
        let nt = master.get_notetype_by_name("Basic")?.unwrap();
        let mut note = nt.new_note();
        note.set_field(0, "original book quotation")?;
        note.set_field(1, "old answer")?;
        master.add_note(&mut note, DeckId(1))?;
        let id = note.id;
        let cards = master.search_cards(id, SortMode::NoOrder)?;
        master.storage.db().execute(
            "insert into revlog values (1234567890000, ?1, -1, 3, 10, 5, 2500, 1234, 1)",
            [cards[0].0],
        )?;
        let client = Client::builder().no_proxy().build()?;
        let mut auth = sync_login(
            "fixture",
            "fixture-password",
            Some(endpoint.clone()),
            client.clone(),
        )
        .await?;
        auth.endpoint = Some(endpoint.parse()?);
        master.full_upload(auth.clone(), client.clone()).await?;
        let path = temp.path().join("sidecar.anki2");
        std::fs::write(path.with_extension("bootstrap-pending"), b"download only")?;
        let mut sidecar = worker(path.clone(), endpoint.clone(), false)?;
        sidecar.sync().await?;
        let mut master = CollectionBuilder::new(&master_path)
            .with_desktop_media_paths()
            .build()?;
        let mut extra_type = master
            .get_notetype_by_name("Basic")?
            .unwrap()
            .as_ref()
            .clone();
        extra_type.id = NotetypeId(0);
        extra_type.name = "Read-only full-download test".into();
        master.add_notetype(&mut extra_type, false)?;
        master.full_upload(auth.clone(), client.clone()).await?;
        let refreshed = sidecar.handle(Command::Notetype(extra_type.id.0)).await?;
        assert_eq!(refreshed["name"], "Read-only full-download test");
        let update = writes::Update {
            note_id: id.0,
            expected_revision: None,
            field_patch: BTreeMap::from([("Back".into(), "updated answer".into())]),
            tag_patch: writes::TagPatch::default(),
        };
        let preview = sidecar
            .handle(Command::Preview {
                subject: "owner".into(),
                updates: vec![update],
            })
            .await?;
        let token = preview["preview_token"].as_str().unwrap().to_owned();
        let disabled = sidecar
            .handle(Command::Apply {
                subject: "owner".into(),
                token: token.clone(),
                key: "edit-1".into(),
                confirmed: true,
            })
            .await?;
        assert_eq!(disabled["status"], "not_applied");
        assert_eq!(
            sidecar
                .col
                .as_ref()
                .unwrap()
                .storage
                .get_note(id)?
                .unwrap()
                .fields()[1],
            "old answer"
        );
        sidecar.writes_enabled = true;
        let preview = sidecar.journal.load_preview("owner", &token)?;
        let mut receipt = sidecar.journal.prepare_update(
            sidecar.col.as_mut().unwrap(),
            "owner",
            "edit-1",
            &token,
            preview,
        )?;
        sidecar.journal.commit_local(
            sidecar.col.as_mut().unwrap(),
            "owner",
            "edit-1",
            &mut receipt,
        )?;
        // A bound socket that never accepts gives a deterministic outage,
        // including with HTTP keep-alive connections to the healthy server.
        let unavailable = std::net::TcpListener::bind("127.0.0.1:0")?;
        sidecar.endpoint = format!("http://{}/", unavailable.local_addr()?);
        sidecar.client = Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(200))
            .build()?;
        let pending = sidecar
            .finish_write("owner", "edit-1", &mut receipt)
            .await?;
        assert_eq!(pending["status"], "saved_locally_sync_pending");
        assert!(sidecar.handle(Command::Note(id.0)).await.is_err());
        let status = sidecar.handle(Command::Status).await?;
        assert_eq!(status["last_sync_failed"], true);
        let offline_receipt = sidecar
            .handle(Command::Operation {
                subject: "owner".into(),
                key: "edit-1".into(),
            })
            .await?;
        assert_eq!(offline_receipt["state"], "saved_locally_sync_pending");
        sidecar.col.take().unwrap().close(None)?;
        drop(sidecar);
        let mut sidecar = worker(path, endpoint, true)?;
        let mut col = sidecar.open()?;
        sidecar.journal.recover_all(&mut col)?;
        sidecar.col = Some(col);
        let recovered = sidecar
            .handle(Command::Operation {
                subject: "owner".into(),
                key: "edit-1".into(),
            })
            .await?;
        assert_eq!(recovered["state"], "synced_successfully");
        let apply = || Command::Apply {
            subject: "owner".into(),
            token: token.clone(),
            key: "edit-1".into(),
            confirmed: true,
        };
        let result = sidecar.handle(apply()).await?;
        assert_eq!(result["status"], "synced_successfully");
        assert_eq!(
            sidecar.handle(apply()).await?["status"],
            "synced_successfully"
        );
        let create = writes::Create {
            deck_id: 1,
            notetype_id: nt.id.0,
            fields: BTreeMap::from([
                ("Front".into(), "new practice".into()),
                ("Back".into(), "answer".into()),
            ]),
            tags: vec![],
            external_dedupe_key: "correction-1".into(),
            contexts: None,
        };
        let created = sidecar
            .handle(Command::Create {
                subject: "owner".into(),
                input: create.clone(),
                confirmed: true,
            })
            .await?;
        assert_eq!(created["status"], "synced_successfully");
        assert_eq!(
            sidecar
                .handle(Command::Create {
                    subject: "owner".into(),
                    input: create,
                    confirmed: true,
                })
                .await?["note_ids"],
            created["note_ids"]
        );
        let mut master = CollectionBuilder::new(&master_path)
            .with_desktop_media_paths()
            .build()?;
        assert_eq!(
            master
                .normal_sync(auth.clone(), client.clone())
                .await?
                .required,
            SyncActionRequired::NoChanges
        );
        assert_eq!(
            master.storage.get_note(id)?.unwrap().fields(),
            &["original book quotation", "updated answer"]
        );
        assert_eq!(master.search_cards(id, SortMode::NoOrder)?, cards);
        assert_eq!(master.storage.get_all_note_ids()?.len(), 2);
        let review: Vec<i64> = master.storage.db().query_row(
            "select id,cid,ease,ivl,lastIvl,factor,time from revlog",
            [],
            |row| (0..7).map(|i| row.get(i)).collect(),
        )?;
        assert_eq!(
            review,
            vec![1234567890000, cards[0].0, 3, 10, 5, 2500, 1234]
        );
        let preview = sidecar
            .handle(Command::Preview {
                subject: "owner".into(),
                updates: vec![writes::Update {
                    note_id: id.0,
                    expected_revision: None,
                    field_patch: BTreeMap::from([("Back".into(), "stale overwrite".into())]),
                    tag_patch: writes::TagPatch::default(),
                }],
            })
            .await?;
        let mut remote = master.storage.get_note(id)?.unwrap();
        remote.set_field(1, "concurrent device edit")?;
        master.update_note(&mut remote)?;
        master.normal_sync(auth, client).await?;
        let conflict = sidecar
            .handle(Command::Apply {
                subject: "owner".into(),
                token: preview["preview_token"].as_str().unwrap().into(),
                key: "stale-edit".into(),
                confirmed: true,
            })
            .await
            .err()
            .unwrap();
        assert_eq!(Failure::from_error(&conflict).code, "stale_preview");
        assert_eq!(
            sidecar
                .col
                .as_ref()
                .unwrap()
                .storage
                .get_note(id)?
                .unwrap()
                .fields()[1],
            "concurrent device edit"
        );
        let authoring = sidecar.handle(Command::AuthoringContext).await?;
        assert!(authoring["notetypes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|nt| nt["name"] == "Basic"));
        let read = sidecar
            .handle(Command::Search {
                query: format!("nid:{}", id.0),
                offset: 0,
                limit: 20,
                fields: Some(vec!["Back".into()]),
            })
            .await?;
        assert_eq!(
            read["notes"][0]["fields"][0]["value"],
            "concurrent device edit"
        );
        assert_eq!(read["notes"][0]["fields"].as_array().unwrap().len(), 1);
        let input = |key: &str| writes::Create {
            deck_id: 1,
            notetype_id: nt.id.0,
            fields: BTreeMap::from([
                ("Front".into(), key.into()),
                ("Back".into(), "batch answer".into()),
            ]),
            tags: vec![],
            external_dedupe_key: key.into(),
            contexts: None,
        };
        let preview = sidecar
            .handle(Command::CreatePreview {
                subject: "owner".into(),
                notes: vec![input("batch-one"), input("batch-two")],
            })
            .await?;
        let created = sidecar
            .handle(Command::Apply {
                subject: "owner".into(),
                token: preview["preview_token"].as_str().unwrap().into(),
                key: "batch-create".into(),
                confirmed: true,
            })
            .await?;
        assert_eq!(created["status"], "synced_successfully");
        assert_eq!(created["completed_notes"], 2);
        assert_eq!(created["sync_pending"], false);
        assert_eq!(created["notes"].as_array().unwrap().len(), 2);
        assert_eq!(created["notes"][0]["card_ids"].as_array().unwrap().len(), 1);
        assert_eq!(created["notes"][0]["revision"].as_str().unwrap().len(), 64);
        let fetched = sidecar
            .handle(Command::Notes {
                note_ids: vec![created["note_ids"][0].as_i64().unwrap()],
                fields: Some(vec!["Back".into()]),
            })
            .await?;
        assert_eq!(fetched["notes"][0]["fields"][0]["value"], "batch answer");
        sidecar.col.take().unwrap().close(None)?;
        task.abort();
        Ok(())
    }
}

struct Request {
    command: Command,
    reply: oneshot::Sender<Result<Value, Failure>>,
}

#[derive(Clone)]
pub struct Actor {
    queue: mpsc::Sender<Request>,
    pub ready: Arc<AtomicBool>,
    writes_enabled: bool,
}

impl Actor {
    pub fn start() -> Result<Self> {
        let path = PathBuf::from(std::env::var("ANKI_COLLECTION")?);
        if !path.exists() && std::env::var("ANKI_BOOTSTRAP_DOWNLOAD").as_deref() != Ok("1") {
            bail!("new MCP collections require explicit ANKI_BOOTSTRAP_DOWNLOAD=1");
        }
        if !path.exists() {
            std::fs::create_dir_all(path.parent().context("collection parent missing")?)?;
            std::fs::write(path.with_extension("bootstrap-pending"), b"download only\n")?;
        }
        let endpoint = std::env::var("ANKI_SYNC_ENDPOINT")?;
        let username = std::env::var("ANKI_SYNC_USERNAME")?;
        let password = std::fs::read_to_string(std::env::var("ANKI_SYNC_PASSWORD_FILE")?)?;
        let writes_enabled = std::env::var("ANKI_WRITES_ENABLED").as_deref() == Ok("1");
        let ready = Arc::new(AtomicBool::new(false));
        let initialized = ready.clone();
        let (queue, mut receiver) = mpsc::channel::<Request>(16);
        std::thread::Builder::new()
            .name("anki-collection".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("collection runtime");
                runtime.block_on(async move {
                    let mut worker = Worker {
                        journal: Journal::open(&path.with_extension("mcp-journal.sqlite"))
                            .expect("durable MCP journal"),
                        writes_enabled,
                        allow_readonly_full_download: std::env::var(
                            "ANKI_ALLOW_READONLY_FULL_DOWNLOAD",
                        )
                        .as_deref()
                            == Ok("1"),
                        path,
                        endpoint,
                        username,
                        password,
                        col: None,
                        last_sync: None,
                        sync_failed: false,
                        client: Client::builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .expect("sync HTTP client"),
                    };
                    if !worker.path.with_extension("bootstrap-pending").exists() {
                        if let Ok(mut col) = worker.open() {
                            if worker.journal.recover_all(&mut col).is_err() {
                                eprintln!(
                                    "MCP journal requires reconciliation; writes remain guarded"
                                );
                            }
                            worker.col = Some(col);
                        }
                    }
                    for attempt in 0..5 {
                        if worker.sync().await.is_ok() {
                            initialized.store(true, Ordering::Release);
                            break;
                        }
                        // Do not log upstream errors, which can contain remote response bodies.
                        eprintln!("MCP initial sync unavailable (attempt {})", attempt + 1);
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    while let Some(request) = receiver.recv().await {
                        let result = worker.handle(request.command).await;
                        initialized.store(
                            worker.last_sync.is_some() && worker.col.is_some(),
                            Ordering::Release,
                        );
                        let response = match result {
                            Ok(value) if value.to_string().len() <= 512 * 1024 => Ok(value),
                            Ok(_) => Err(Failure::new(
                                "response_too_large",
                                "Response exceeds 512 KiB; use fewer notes or selected fields.",
                                false,
                            )),
                            Err(error) => Err(Failure::from_error(&error)),
                        };
                        let _ = request.reply.send(response);
                    }
                    if let Some(col) = worker.col.take() {
                        let _ = col.close(None);
                    }
                });
            })?;
        Ok(Self {
            queue,
            ready,
            writes_enabled,
        })
    }

    pub fn writes_enabled(&self) -> bool {
        self.writes_enabled
    }

    pub async fn call(&self, command: Command) -> Result<Value, Failure> {
        let (reply, response) = oneshot::channel();
        self.queue
            .try_send(Request { command, reply })
            .map_err(|_| {
                Failure::new(
                    "queue_busy",
                    "Collection queue is busy; retry this request.",
                    true,
                )
            })?;
        response.await.map_err(|_| {
            Failure::new(
                "worker_unavailable",
                "Collection worker is unavailable.",
                true,
            )
        })?
    }
}

struct Worker {
    journal: Journal,
    writes_enabled: bool,
    allow_readonly_full_download: bool,
    path: PathBuf,
    endpoint: String,
    username: String,
    password: String,
    col: Option<Collection>,
    client: Client,
    last_sync: Option<u64>,
    sync_failed: bool,
}

impl Worker {
    fn open(&self) -> Result<Collection> {
        Ok(CollectionBuilder::new(&self.path)
            .with_desktop_media_paths()
            .set_check_integrity(true)
            .build()?)
    }

    async fn sync(&mut self) -> Result<()> {
        self.sync_inner().await.map_err(|_|Failure::new("sync_unavailable", "Automatic synchronization failed; retry the request. Full-sync requirements need operator maintenance.", true).into())
    }

    async fn sync_inner(&mut self) -> Result<()> {
        anyhow::ensure!(
            !maintenance::blocked(&self.path)?,
            "maintenance requires reconciliation"
        );
        self.sync_failed = true;
        let mut auth = sync_login(
            self.username.clone(),
            self.password.trim_end().to_owned(),
            Some(self.endpoint.clone()),
            self.client.clone(),
        )
        .await?;
        auth.endpoint = Some(self.endpoint.parse()?);
        if self.col.is_none() {
            let col = self.open()?;
            let new = self.path.with_extension("bootstrap-pending").exists();
            if new {
                anyhow::ensure!(
                    self.journal.pending()? == 0,
                    "bootstrap cannot discard pending operations"
                );
                // A new client can only download. No code path performs a full upload.
                col.full_download(auth.clone(), self.client.clone()).await?;
                self.col = Some(self.open()?);
                std::fs::remove_file(self.path.with_extension("bootstrap-pending"))?;
            } else {
                self.col = Some(col);
            }
        }
        let output = self
            .col
            .as_mut()
            .context("collection unavailable")?
            .normal_sync(auth.clone(), self.client.clone())
            .await?;
        if output.required != SyncActionRequired::NoChanges {
            if self.allow_readonly_full_download
                && !self.writes_enabled
                && self.journal.pending()? == 0
                && matches!(
                    output.required,
                    SyncActionRequired::FullSyncRequired {
                        download_ok: true,
                        ..
                    }
                )
            {
                let snapshot = tempfile::Builder::new()
                    .prefix("readonly-before-full-download-")
                    .suffix(".anki2")
                    .tempfile_in(self.path.parent().context("collection parent missing")?)?;
                self.col.as_ref().unwrap().storage.db().execute(
                    "VACUUM INTO ?",
                    [snapshot.path().to_str().context("snapshot path")?],
                )?;
                snapshot.as_file().sync_all()?;
                snapshot.keep()?;
                self.col
                    .take()
                    .unwrap()
                    .full_download(auth.clone(), self.client.clone())
                    .await?;
                self.col = Some(self.open()?);
            } else {
                bail!("full sync requires explicit maintenance");
            }
        }
        let col = self.col.as_mut().context("collection unavailable")?;
        MediaSyncer::new(
            col.media()?,
            col.new_progress_handler(),
            HttpSyncClient::new(auth, self.client.clone()),
        )?
        .sync(None)
        .await?;
        self.last_sync = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        );
        self.sync_failed = false;
        self.journal.mark_synced()?;
        Ok(())
    }

    async fn handle(&mut self, command: Command) -> Result<Value> {
        if maintenance::blocked(&self.path)?
            && !matches!(
                command,
                Command::Status | Command::Operation { .. } | Command::MigrationApply { .. }
            )
        {
            return Ok(
                json!({"status":"maintenance_pending", "reason":"template migration blocks ordinary collection operations"}),
            );
        }
        match command {
            Command::MigrationApply {
                subject,
                notetype_id,
                revision,
                template_update,
                key,
                confirmed,
            } => {
                if !self.writes_enabled || !confirmed {
                    return Ok(
                        json!({"status":"not_applied", "reason":"explicit confirmation and enabled writes required"}),
                    );
                }
                if let Some(previous) = maintenance::get(&self.path, &subject, &key)? {
                    if previous.subject == subject && previous.idempotency_key == key {
                        anyhow::ensure!(
                            previous.notetype_id == notetype_id
                                && previous.expected_revision == revision
                                && previous.template_update == template_update,
                            "key reused"
                        );
                        return Ok(serde_json::to_value(previous)?);
                    }
                }
                self.sync().await?;
                anyhow::ensure!(self.journal.pending()? == 0, "resolve pending writes first");
                let preview = maintenance::preview(
                    self.col.as_mut().context("collection unavailable")?,
                    notetype_id,
                    template_update.as_ref(),
                )?;
                anyhow::ensure!(
                    preview["expected_revision"] == revision
                        && preview["card_generation_changed_in_clone"] == false,
                    "stale or unsafe migration preview"
                );
                return maintenance::enqueue(
                    &self.path,
                    maintenance::Request {
                        subject,
                        idempotency_key: key,
                        notetype_id,
                        expected_revision: revision,
                        state: "requested".into(),
                        preview,
                        backup: None,
                        template_update,
                    },
                );
            }
            Command::CreatePreview { subject, notes } => {
                self.sync().await?;
                return self.journal.preview_create(
                    self.col.as_mut().context("collection unavailable")?,
                    &subject,
                    notes,
                );
            }
            Command::TemplatePreview(input) => {
                self.sync().await?;
                let mut preview = crate::templates::preview(
                    self.col.as_mut().context("collection unavailable")?,
                    &input,
                )?;
                preview["apply_enabled"] = json!(self.writes_enabled);
                return Ok(preview);
            }
            Command::MigrationPreview(id) => {
                self.sync().await?;
                let mut preview = contexts::migration_preview(
                    self.col.as_mut().context("collection unavailable")?,
                    id,
                )?;
                preview["apply_enabled"] = json!(self.writes_enabled);
                return Ok(preview);
            }
            Command::ContextPreview { subject, input } => {
                self.sync().await?;
                let (mut output, update) = contexts::enrichment(
                    self.col.as_mut().context("collection unavailable")?,
                    input,
                )?;
                if let Some(update) = update {
                    output["write_preview"] = self.journal.preview_context(
                        self.col.as_mut().context("collection unavailable")?,
                        &subject,
                        update,
                    )?;
                }
                return Ok(output);
            }
            Command::Operation { subject, key } => {
                if let Some(request) = maintenance::get(&self.path, &subject, &key)? {
                    if request.subject == subject && request.idempotency_key == key {
                        return Ok(serde_json::to_value(request)?);
                    }
                }
                // Receipt polling also retries pending uploads. Keep the receipt
                // available during outages and maintenance.
                if !maintenance::blocked(&self.path)? {
                    let _ = self.sync().await;
                }
                let mut output = self.journal.operation(&subject, &key)?;
                output["last_sync_unix"] = json!(self.last_sync);
                output["last_sync_failed"] = json!(self.sync_failed);
                output["sync_pending"] = json!(output["state"] == "saved_locally_sync_pending");
                let ids = self
                    .journal
                    .get(&subject, &key)?
                    .context("operation missing")?
                    .note_ids;
                let col = self.col.as_mut().context("collection unavailable")?;
                let mut notes = Vec::new();
                for id in ids {
                    match authoring::note(col, id, Some(&[])) {
                        Ok(note) => notes.push(note),
                        Err(error) => {
                            notes.push(json!({"id":id,"error":Failure::from_error(&error)}))
                        }
                    }
                }
                output["notes"] = json!(notes);
                return Ok(output);
            }
            Command::Preview { subject, updates } => {
                self.sync().await?;
                return self.journal.preview(
                    self.col.as_mut().context("collection unavailable")?,
                    &subject,
                    updates,
                );
            }
            Command::Apply {
                subject,
                token,
                key,
                confirmed,
            } => {
                if !self.writes_enabled || !confirmed {
                    return Ok(
                        json!({"status":"not_applied", "reason":"writes disabled or explicit confirmation missing"}),
                    );
                }
                let mut receipt = match self.journal.get(&subject, &key)? {
                    Some(mut receipt) => {
                        anyhow::ensure!(
                            receipt.preview_token.as_deref() == Some(&token),
                            "idempotency key reused for another preview"
                        );
                        self.journal.reconcile_local(
                            self.col.as_mut().context("collection unavailable")?,
                            &subject,
                            &key,
                            &mut receipt,
                        )?;
                        if receipt.state == "prepared" && self.sync().await.is_err() {
                            return Ok(
                                json!({"status":if receipt.completed_notes() == 0 { "not_applied" } else { "saved_locally_sync_pending" }, "reason":"pre-commit sync unavailable", "idempotency_key":key,"completed_notes":receipt.completed_notes()}),
                            );
                        }
                        receipt
                    }
                    None => {
                        let preview = self.journal.load_preview(&subject, &token)?;
                        if self.sync().await.is_err() {
                            return Ok(
                                json!({"status":"not_applied", "reason":"pre-commit sync unavailable"}),
                            );
                        }
                        self.journal.prepare_update(
                            self.col.as_mut().context("collection unavailable")?,
                            &subject,
                            &key,
                            &token,
                            preview,
                        )?
                    }
                };
                return self.finish_write(&subject, &key, &mut receipt).await;
            }
            Command::Create {
                subject,
                input,
                confirmed,
            } => {
                if !self.writes_enabled || !confirmed {
                    return Ok(
                        json!({"status":"not_applied", "reason":"writes disabled or confirmation missing"}),
                    );
                }
                let key = input.external_dedupe_key.clone();
                if let Some(mut receipt) = self.journal.get(&subject, &key)? {
                    // Verify content digest through the same preparation path before resuming.
                    self.journal.prepare_create(
                        self.col.as_mut().context("collection unavailable")?,
                        &subject,
                        input,
                    )?;
                    self.journal.reconcile_local(
                        self.col.as_mut().context("collection unavailable")?,
                        &subject,
                        &key,
                        &mut receipt,
                    )?;
                    if receipt.state == "prepared" && self.sync().await.is_err() {
                        return Ok(
                            json!({"status":"not_applied", "reason":"pre-commit sync unavailable","idempotency_key":key}),
                        );
                    }
                    return self.finish_write(&subject, &key, &mut receipt).await;
                }
                if self.sync().await.is_err() {
                    return Ok(
                        json!({"status":"not_applied", "reason":"pre-commit sync unavailable"}),
                    );
                }
                let mut receipt = self.journal.prepare_create(
                    self.col.as_mut().context("collection unavailable")?,
                    &subject,
                    input,
                )?;
                return self.finish_write(&subject, &key, &mut receipt).await;
            }
            _ => {}
        }
        if matches!(command, Command::Status) {
            // Diagnostics must remain available when automatic refresh fails.
            if !maintenance::blocked(&self.path)? {
                let _ = self.sync().await;
            }
            let counts = match self.col.as_ref() {
                Some(col) => {
                    let db = col.storage.db();
                    let counts: (u64,u64,u64,u64) = db.query_row(
                        "select (select count(*) from notes), (select count(*) from cards), (select count(*) from revlog), (select count(*) from notetypes)",
                        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
                    json!({"notes": counts.0, "cards": counts.1, "reviews": counts.2, "notetypes": counts.3})
                }
                None => Value::Null,
            };
            return Ok(
                json!({"mode": if self.writes_enabled { "guarded_writes" } else { "read_only" }, "counts": counts, "last_sync_unix": self.last_sync,
                "last_sync_failed": self.sync_failed, "writes_enabled": self.writes_enabled, "maintenance_pending":maintenance::blocked(&self.path)?, "pending_operations": self.journal.pending()?, "source_commit": "dd324c8acc3d61f7f2696ea6b9f26bf2b494ca45"}),
            );
        }
        // Content reads never silently return an outdated mirror. Refresh also
        // retries locally committed writes whose previous upload failed.
        self.sync().await?;
        let last_sync = self.last_sync;
        let mut output = self.read(command)?;
        output["last_sync_unix"] = json!(last_sync);
        output["last_sync_failed"] = json!(false);
        Ok(output)
    }

    fn read(&mut self, command: Command) -> Result<Value> {
        let col = self.col.as_mut().context("collection unavailable")?;
        match command {
            Command::Search {
                query,
                offset,
                limit,
                fields,
            } => {
                authoring::validate_projection(&fields)?;
                if query.len() > 2048 || limit == 0 || limit > 100 || offset > 100000 {
                    bail!("search bounds");
                }
                let mut ids = col.search_notes_unordered(query.as_str())?;
                ids.sort_unstable();
                let total = ids.len();
                let mut notes = Vec::new();
                for id in ids.into_iter().skip(offset).take(limit) {
                    if let Some(note) = col.storage.get_note(id)? {
                        let preview: String = note
                            .fields()
                            .first()
                            .map(String::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(160)
                            .collect();
                        if let Some(fields) = &fields {
                            let mut projected = authoring::note(col, id.0, Some(fields))?;
                            projected["preview"] = json!(preview);
                            notes.push(projected);
                        } else {
                            notes.push(json!({"id": id.0, "notetype_id": note.notetype_id.0, "preview": preview,"revision":writes::revision(&note)}));
                        }
                    }
                }
                Ok(
                    json!({"notes": notes, "total": total, "next_offset": if offset + limit < total { Some(offset + limit) } else { None }}),
                )
            }
            Command::Note(id) => authoring::note(col, id, None),
            Command::Notes { note_ids, fields } => authoring::notes(col, &note_ids, &fields),
            Command::AuthoringContext => authoring::context(col, self.writes_enabled),
            Command::Decks => Ok(
                json!({"decks": col.get_all_deck_names(false)?.into_iter().map(|(id,name)| json!({"id": id.0, "name": name})).collect::<Vec<_>>()}),
            ),
            Command::Notetype(id) => {
                let nt = col
                    .get_notetype(NotetypeId(id))?
                    .context("notetype missing")?;
                let fields: Vec<Value> = nt
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(ordinal, field)| json!({"ordinal": ordinal, "name": field.name}))
                    .collect();
                let templates: Vec<Value> = nt.templates.iter().enumerate().map(|(ordinal,t)|json!({"ordinal": ordinal, "name": t.name, "front": t.config.q_format, "back": t.config.a_format})).collect();
                Ok(
                    json!({"id": id, "name": nt.name, "fields": fields, "templates": templates, "css": nt.config.css}),
                )
            }
            Command::Status
            | Command::CreatePreview { .. }
            | Command::Preview { .. }
            | Command::Apply { .. }
            | Command::Create { .. }
            | Command::Operation { .. }
            | Command::TemplatePreview(_)
            | Command::MigrationPreview(_)
            | Command::MigrationApply { .. }
            | Command::ContextPreview { .. } => unreachable!(),
        }
    }

    async fn finish_write(
        &mut self,
        subject: &str,
        key: &str,
        receipt: &mut writes::Receipt,
    ) -> Result<Value> {
        if receipt.state == "reconciliation_required" {
            return Ok(
                json!({"status":"saved_locally_sync_pending", "reconciliation_required":true, "note_ids":receipt.note_ids,"idempotency_key":key,"completed_notes":receipt.completed_notes()}),
            );
        }
        if receipt.state == "synced_successfully" || receipt.state == "not_applied" {
            return self.write_result(key, receipt);
        }
        let committed = self.journal.commit_local(
            self.col.as_mut().context("collection unavailable")?,
            subject,
            key,
            receipt,
        );
        if let Err(error) = committed {
            return Ok(
                json!({"status":"saved_locally_sync_pending", "sync_pending":true,
                "local_commit_confirmed":receipt.completed_notes() > 0 || !receipt.note_ids.is_empty(),
                "reconciliation_required":true, "note_ids":receipt.note_ids, "completed_notes":receipt.completed_notes(),
                "idempotency_key":key,"error":Failure::from_error(&error)}),
            );
        }
        if self.sync().await.is_ok() {
            receipt.state = "synced_successfully".into();
            self.journal.save(subject, key, receipt)?;
        }
        self.write_result(key, receipt)
    }

    fn write_result(&mut self, key: &str, receipt: &writes::Receipt) -> Result<Value> {
        let col = self.col.as_mut().context("collection unavailable")?;
        let mut notes = Vec::new();
        for id in &receipt.note_ids {
            match authoring::note(col, *id, Some(&[])) {
                Ok(note) => notes.push(note),
                Err(error) => notes.push(json!({"id":id,"error":Failure::from_error(&error)})),
            }
        }
        Ok(
            json!({"status":receipt.state,"sync_pending":receipt.state == "saved_locally_sync_pending",
            "note_ids":receipt.note_ids,"notes":notes,"idempotency_key":key,"completed_notes":receipt.completed_notes(),
            "last_sync_unix":self.last_sync,"last_sync_failed":self.sync_failed}),
        )
    }
}

pub async fn run_maintenance() -> Result<()> {
    crate::maintenance::run().await
}
