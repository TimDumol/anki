// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::collections::BTreeMap;
use std::path::Path;

use anki::collection::Collection;
use anki::collection::CollectionBuilder;
use anki::decks::DeckId;
use anki::notes::Note;
use anki::notes::NoteId;
use anki::notetype::NotetypeId;
use anki::search::SortMode;
use anyhow::bail;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::failure::Failure;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub note_id: i64,
    #[serde(default)]
    pub expected_revision: Option<String>,
    #[serde(default)]
    pub field_patch: BTreeMap<String, String>,
    #[serde(default)]
    pub tag_patch: TagPatch,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TagPatch {
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub deck_id: i64,
    pub notetype_id: i64,
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub external_dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<crate::contexts::CreationContexts>,
}

#[derive(Clone, Deserialize, Serialize)]
struct PlannedCreate {
    input: Create,
    fields: BTreeMap<String, String>,
    schema: String,
    card_count: usize,
    tags: Vec<String>,
    normalize_text: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct Snapshot {
    id: i64,
    notetype_id: i64,
    fields: Vec<String>,
    tags: Vec<String>,
}

impl Snapshot {
    fn of(note: &Note) -> Self {
        Self {
            id: note.id.0,
            notetype_id: note.notetype_id.0,
            fields: note.fields().clone(),
            tags: note.tags.clone(),
        }
    }

    fn restore(&self, note: &mut Note) -> Result<()> {
        ensure!(note.notetype_id.0 == self.notetype_id, "note type changed");
        ensure!(
            note.fields().len() == self.fields.len(),
            "field count changed"
        );
        for (ordinal, value) in self.fields.iter().enumerate() {
            note.set_field(ordinal, value.clone())?;
        }
        note.tags.clone_from(&self.tags);
        Ok(())
    }
}

pub fn revision(note: &Note) -> String {
    digest(&json!({"notetype_id": note.notetype_id.0, "fields": note.fields(), "tags": note.tags}))
}

fn digest(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= 128,
        Failure::new(
            "invalid_key",
            "Keys must contain between 1 and 128 bytes.",
            false
        )
    );
    Ok(())
}

fn validate_fields(fields: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        fields.len() <= 64,
        Failure::new("invalid_fields", "Supply at most 64 fields.", false)
    );
    for (name, value) in fields {
        ensure!(
            !name.is_empty() && name.len() <= 128,
            Failure::new(
                "invalid_field_name",
                "Field names must contain between 1 and 128 bytes.",
                false
            )
        );
        ensure!(
            value.len() <= 32 * 1024 && !value.contains('\x1f'),
            Failure::new(
                "invalid_field_content",
                "Field content exceeds 32 KiB or contains a field separator.",
                false
            )
        );
        // Content tools cannot introduce scripts or remote executable content.
        let lower = value.to_ascii_lowercase();
        ensure!(
            !lower.contains("<script")
                && !lower.contains("javascript:")
                && !lower.contains("<iframe"),
            Failure::new(
                "executable_content",
                "Field content contains executable HTML; remove it before retrying.",
                false
            )
        );
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<()> {
    ensure!(
        tags.len() <= 64,
        Failure::new("invalid_tags", "Supply at most 64 tags.", false)
    );
    ensure!(
        tags.iter().all(|tag| !tag.is_empty()
            && tag.len() <= 128
            && !tag.chars().any(|c| c.is_whitespace() || c.is_control())),
        Failure::new(
            "invalid_tags",
            "Tags must be bounded nonempty strings without whitespace or control characters.",
            false
        )
    );
    Ok(())
}

fn schema(col: &mut Collection, id: i64) -> Result<String> {
    let nt = col
        .get_notetype(NotetypeId(id))?
        .context("note type missing")?;
    Ok(digest(
        &json!({"fields": nt.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),
        "templates": nt.templates.iter().map(|t| (&t.config.q_format, &t.config.a_format)).collect::<Vec<_>>(),
        "css": nt.config.css}),
    ))
}

#[derive(Clone, Deserialize, Serialize)]
struct Change {
    before: Snapshot,
    after: Snapshot,
    before_revision: String,
    after_revision: String,
    schema: String,
    card_ids: Vec<i64>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Preview {
    subject: String,
    expires: u64,
    changes: Vec<Change>,
    #[serde(default)]
    creates: Vec<PlannedCreate>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Receipt {
    pub digest: String,
    pub state: String,
    pub note_ids: Vec<i64>,
    changes: Vec<Change>,
    create: Option<Create>,
    #[serde(default)]
    creates: Vec<PlannedCreate>,
    guid: Option<String>,
    completed: usize,
    pub preview_token: Option<String>,
}

impl Receipt {
    pub fn completed_notes(&self) -> usize {
        self.completed
    }
}

fn creation_guid(subject: &str, key: &str) -> String {
    digest(&json!(["anki-mcp-create-v1", subject, key]))
}

fn insert_planned(col: &mut Collection, planned: &PlannedCreate, guid: &str) -> Result<Note> {
    let nt = col
        .get_notetype(NotetypeId(planned.input.notetype_id))?
        .context("note type missing")?;
    let mut note = nt.new_note();
    note.guid = guid.into();
    note.tags.clone_from(&planned.tags);
    for (ordinal, field) in nt.fields.iter().enumerate() {
        note.set_field(
            ordinal,
            planned
                .fields
                .get(&field.name)
                .context("schema changed")?
                .clone(),
        )?;
    }
    col.add_note(&mut note, DeckId(planned.input.deck_id))?;
    col.storage
        .get_note(note.id)?
        .context("created note missing")
}

fn matches_plan(col: &mut Collection, note: &Note, planned: &PlannedCreate) -> Result<bool> {
    if note.notetype_id.0 != planned.input.notetype_id {
        return Ok(false);
    }
    let nt = col
        .get_notetype(note.notetype_id)?
        .context("note type missing")?;
    let mut expected_tags = planned.tags.clone();
    let mut actual_tags = note.tags.clone();
    expected_tags.sort();
    expected_tags.dedup();
    actual_tags.sort();
    actual_tags.dedup();
    Ok(schema(col, note.notetype_id.0)? == planned.schema
        && expected_tags == actual_tags
        && nt
            .fields
            .iter()
            .zip(note.fields())
            .all(|(field, value)| planned.fields.get(&field.name) == Some(value)))
}

pub struct Journal(Connection);

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS previews (token TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS receipts (subject TEXT NOT NULL, key TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(subject,key));")?;
        Ok(Self(db))
    }

    pub fn preview(
        &self,
        col: &mut Collection,
        subject: &str,
        updates: Vec<Update>,
    ) -> Result<Value> {
        self.preview_inner(col, subject, updates, false)
    }

    pub fn preview_context(
        &self,
        col: &mut Collection,
        subject: &str,
        update: Update,
    ) -> Result<Value> {
        self.preview_inner(col, subject, vec![update], true)
    }

    fn preview_inner(
        &self,
        col: &mut Collection,
        subject: &str,
        updates: Vec<Update>,
        context_enrichment: bool,
    ) -> Result<Value> {
        ensure!(
            !updates.is_empty() && updates.len() <= 20,
            Failure::new("invalid_batch", "Supply between 1 and 20 notes.", false)
        );
        let mut ids: Vec<_> = updates.iter().map(|u| u.note_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ensure!(
            ids.len() == updates.len(),
            Failure::new(
                "duplicate_note",
                "Each note ID may appear only once per batch.",
                false
            )
        );
        // Exercise upstream card generation in a private clone before permitting a
        // write.
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("preview.anki2");
        col.storage
            .db()
            .execute("VACUUM INTO ?", [path.to_str().context("preview path")?])?;
        let mut clone = CollectionBuilder::new(&path).build()?;
        let mut changes = Vec::new();
        let mut diffs = Vec::new();
        for update in updates {
            ensure!(
                context_enrichment
                    || !update.field_patch.keys().any(|name| matches!(
                        name.as_str(),
                        "AlternateContexts" | "RecognitionContexts" | "ProductionContexts"
                    )),
                Failure::new(
                    "invalid_contexts",
                    "Use the validated context enrichment workflow for context fields.",
                    false
                )
            );
            validate_fields(&update.field_patch)?;
            validate_tags(&update.tag_patch.add)?;
            validate_tags(&update.tag_patch.remove)?;
            let before = clone
                .storage
                .get_note(NoteId(update.note_id))?
                .context("note missing")?;
            if update
                .expected_revision
                .as_ref()
                .is_some_and(|expected| expected != &revision(&before))
            {
                return Err(Failure::new(
                    "stale_revision",
                    "Note changed since it was read; fetch it again before editing.",
                    false,
                )
                .details(json!({"note_id":update.note_id,"current_revision":revision(&before)}))
                .into());
            }
            let nt = clone
                .get_notetype(before.notetype_id)?
                .context("note type missing")?;
            let cards = clone.search_cards(before.id, SortMode::NoOrder)?;
            let mut after = before.clone();
            for (name, value) in &update.field_patch {
                let ordinal = nt
                    .fields
                    .iter()
                    .position(|f| f.name == *name)
                    .ok_or_else(|| {
                        Failure::new(
                            "unknown_field",
                            "Field does not exist on this note type.",
                            false,
                        )
                        .details(json!({"note_id":update.note_id,"field":name}))
                    })?;
                after.set_field(ordinal, ammonia::clean(value))?;
            }
            after.tags.retain(|tag| {
                !update
                    .tag_patch
                    .remove
                    .iter()
                    .any(|removed| removed.eq_ignore_ascii_case(tag))
            });
            after.tags.extend(update.tag_patch.add);
            clone.update_note(&mut after)?;
            let after = clone
                .storage
                .get_note(before.id)?
                .context("updated note missing")?;
            ensure!(
                clone.search_cards(before.id, SortMode::NoOrder)? == cards,
                Failure::new(
                    "card_generation_changed",
                    "Update would change generated cards; use maintenance for this change.",
                    false
                )
            );
            diffs.push(json!({"note_id": before.id.0, "before": Snapshot::of(&before), "after": Snapshot::of(&after), "expected_revision": revision(&before)}));
            changes.push(Change {
                before: Snapshot::of(&before),
                after: Snapshot::of(&after),
                before_revision: revision(&before),
                after_revision: revision(&after),
                schema: schema(&mut clone, before.notetype_id.0)?,
                card_ids: cards.iter().map(|id| id.0).collect(),
            });
        }
        clone.close(None)?;
        let token = format!("{:032x}", u128::from_le_bytes(rand::random::<[u8; 16]>()))
            + &format!("{:032x}", u128::from_le_bytes(rand::random::<[u8; 16]>()));
        let preview = Preview {
            subject: subject.into(),
            expires: now()? + 900,
            changes,
            creates: vec![],
        };
        self.0.execute(
            "DELETE FROM previews WHERE json_extract(body,'$.expires') < ?",
            [now()?],
        )?;
        self.0.execute(
            "INSERT INTO previews VALUES (?,?)",
            (&token, serde_json::to_string(&preview)?),
        )?;
        Ok(
            json!({"preview_token":token, "expires_unix":preview.expires, "diffs":diffs, "requires_confirmation":true, "card_generation_changed":false}),
        )
    }

    pub fn load_preview(&self, subject: &str, token: &str) -> Result<Preview> {
        ensure!(
            token.len() <= 128,
            Failure::new(
                "invalid_preview",
                "Preview token is invalid; request a new preview.",
                false
            )
        );
        let body: String = self
            .0
            .query_row("SELECT body FROM previews WHERE token=?", [token], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| {
                Failure::new(
                    "invalid_preview",
                    "Preview does not exist or expired; request a new preview.",
                    false,
                )
            })?;
        let preview: Preview = serde_json::from_str(&body)?;
        ensure!(
            preview.subject == subject,
            Failure::new(
                "invalid_preview",
                "Preview token is not available to this user.",
                false
            )
        );
        Ok(preview)
    }

    pub fn get(&self, subject: &str, key: &str) -> Result<Option<Receipt>> {
        validate_key(key)?;
        let body: Option<String> = self
            .0
            .query_row(
                "SELECT body FROM receipts WHERE subject=? AND key=?",
                (subject, key),
                |row| row.get(0),
            )
            .optional()?;
        body.map(|body| serde_json::from_str(&body).map_err(Into::into))
            .transpose()
    }

    pub fn save(&self, subject: &str, key: &str, receipt: &Receipt) -> Result<()> {
        self.0.execute("INSERT INTO receipts VALUES (?,?,?) ON CONFLICT(subject,key) DO UPDATE SET body=excluded.body", (subject,key,serde_json::to_string(receipt)?))?;
        Ok(())
    }

    pub fn operation(&self, subject: &str, key: &str) -> Result<Value> {
        let receipt = self.get(subject, key)?.ok_or_else(|| {
            Failure::new(
                "operation_missing",
                "No operation exists for this retry key.",
                false,
            )
        })?;
        Ok(
            json!({"idempotency_key":key,"state":receipt.state,"note_ids":receipt.note_ids,
            "completed_notes":receipt.completed,"preview_token":receipt.preview_token,
            "rollback_manifest":receipt.changes,"creation":receipt.create,"creations":receipt.creates}),
        )
    }

    pub fn pending(&self) -> Result<u64> {
        Ok(self.0.query_row("SELECT count(*) FROM receipts WHERE json_extract(body,'$.state') NOT IN ('synced_successfully','not_applied')", [], |row| row.get(0))?)
    }

    pub fn recover_all(&self, col: &mut Collection) -> Result<()> {
        let mut stmt = self.0.prepare(
            "SELECT subject,key,body FROM receipts WHERE json_extract(body,'$.state')='prepared'",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (subject, key, body) in rows {
            let mut receipt = serde_json::from_str(&body)?;
            self.reconcile_local(col, &subject, &key, &mut receipt)?;
        }
        Ok(())
    }

    pub fn mark_synced(&self) -> Result<()> {
        self.0.execute("UPDATE receipts SET body=json_set(body,'$.state','synced_successfully') WHERE json_extract(body,'$.state')='saved_locally_sync_pending'", [])?;
        Ok(())
    }

    pub fn prepare_update(
        &self,
        col: &mut Collection,
        subject: &str,
        key: &str,
        token: &str,
        preview: Preview,
    ) -> Result<Receipt> {
        let operation_digest = if preview.creates.is_empty() {
            digest(&serde_json::to_value(&preview.changes)?)
        } else {
            digest(&serde_json::to_value(&preview.creates)?)
        };
        if let Some(receipt) = self.get(subject, key)? {
            ensure!(
                receipt.digest == operation_digest,
                Failure::new(
                    "idempotency_conflict",
                    "Retry key was already used for a different operation.",
                    false
                )
            );
            return Ok(receipt);
        }
        if now()? > preview.expires {
            return Err(Failure::new(
                "preview_expired",
                "Preview expired; request a new preview.",
                false,
            )
            .into());
        }
        ensure!(
            self.pending()? == 0,
            Failure::new(
                "pending_operation",
                "Resolve the pending operation before starting a new write.",
                false
            )
        );
        for change in &preview.changes {
            let note = col
                .storage
                .get_note(NoteId(change.before.id))?
                .context("note missing")?;
            ensure!(
                revision(&note) == change.before_revision
                    && schema(col, note.notetype_id.0)? == change.schema,
                Failure::new(
                    "stale_preview",
                    "Note or schema changed; request a new preview.",
                    false
                )
            );
            ensure!(
                col.search_cards(note.id, SortMode::NoOrder)?
                    .iter()
                    .map(|id| id.0)
                    .collect::<Vec<_>>()
                    == change.card_ids,
                Failure::new(
                    "stale_preview",
                    "Generated cards changed; request a new preview.",
                    false
                )
            );
        }
        for planned in &preview.creates {
            self.check_creation_key(subject, &planned.input.external_dedupe_key)?;
            if schema(col, planned.input.notetype_id)? != planned.schema
                || col.get_config_bool(anki::config::BoolKey::NormalizeNoteText)
                    != planned.normalize_text
            {
                return Err(Failure::new(
                    "stale_schema",
                    "Note type changed; request a new creation preview.",
                    false,
                )
                .into());
            }
            let deck = col
                .get_deck(DeckId(planned.input.deck_id))?
                .context("deck missing")?;
            ensure!(
                matches!(deck.kind, anki::decks::DeckKind::Normal(_)),
                Failure::new(
                    "filtered_deck",
                    "Choose a normal deck for note creation.",
                    false
                )
            );
        }
        let receipt = Receipt {
            digest: operation_digest,
            state: "prepared".into(),
            creates: preview.creates,
            note_ids: preview.changes.iter().map(|c| c.before.id).collect(),
            changes: preview.changes,
            create: None,
            guid: None,
            completed: 0,
            preview_token: Some(token.into()),
        };
        self.save(subject, key, &receipt)?;
        Ok(receipt)
    }

    fn check_creation_key(&self, subject: &str, key: &str) -> Result<()> {
        if self.get(subject, key)?.is_some() {
            return Err(Failure::new("creation_key_conflict", "External creation key is already an operation key; retry the original operation or choose a new key.", false).into());
        }
        let mut statement = self
            .0
            .prepare("SELECT body FROM receipts WHERE subject=?")?;
        let bodies = statement.query_map([subject], |row| row.get::<_, String>(0))?;
        for body in bodies {
            let receipt: Receipt = serde_json::from_str(&body?)?;
            if receipt
                .create
                .as_ref()
                .is_some_and(|input| input.external_dedupe_key == key)
                || receipt
                    .creates
                    .iter()
                    .any(|planned| planned.input.external_dedupe_key == key)
            {
                return Err(Failure::new("creation_key_conflict", "External creation key is already reserved; poll or retry the original operation.", false).details(json!({"external_dedupe_key":key})).into());
            }
        }
        Ok(())
    }

    fn plan_creates(
        &self,
        col: &mut Collection,
        inputs: Vec<Create>,
    ) -> Result<(Vec<PlannedCreate>, Vec<Value>)> {
        if inputs.is_empty() || inputs.len() > 20 {
            return Err(
                Failure::new("invalid_batch", "Supply between 1 and 20 notes.", false).into(),
            );
        }
        let mut keys = std::collections::HashSet::new();
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("create-preview.anki2");
        col.storage
            .db()
            .execute("VACUUM INTO ?", [path.to_str().context("preview path")?])?;
        let mut clone = CollectionBuilder::new(&path).build()?;
        let mut plans = Vec::new();
        let mut previews = Vec::new();
        let mut duplicate_index: BTreeMap<i64, BTreeMap<String, Vec<Value>>> = BTreeMap::new();
        for (index, input) in inputs.into_iter().enumerate() {
            validate_key(&input.external_dedupe_key)?;
            ensure!(
                keys.insert(input.external_dedupe_key.clone()),
                Failure::new(
                    "duplicate_key",
                    "Each external creation key may appear only once per batch.",
                    false
                )
            );
            validate_fields(&input.fields)?;
            validate_tags(&input.tags)?;
            let nt = clone
                .get_notetype(NotetypeId(input.notetype_id))?
                .ok_or_else(|| {
                    Failure::new(
                        "notetype_missing",
                        "Note type does not exist; refresh authoring context.",
                        false,
                    )
                })?;
            let deck = clone.get_deck(DeckId(input.deck_id))?.ok_or_else(|| {
                Failure::new(
                    "deck_missing",
                    "Deck does not exist; refresh authoring context.",
                    false,
                )
            })?;
            ensure!(
                matches!(deck.kind, anki::decks::DeckKind::Normal(_)),
                Failure::new(
                    "filtered_deck",
                    "Choose a normal deck for note creation.",
                    false
                )
            );
            if nt.fields.len() != input.fields.len()
                || !nt.fields.iter().all(|f| input.fields.contains_key(&f.name))
            {
                return Err(Failure::new("invalid_fields", "Supply every named field exactly once.", false)
                    .details(json!({"index":index,"required_fields":nt.fields.iter().map(|f|&f.name).collect::<Vec<_>>()})).into());
            }
            ensure!(input.fields.iter().all(|(name,value)| !matches!(name.as_str(), "AlternateContexts" | "RecognitionContexts" | "ProductionContexts") || value.trim().is_empty() || value.trim() == "[]"), Failure::new("invalid_contexts", "Supply candidate contexts through contexts; leave dedicated context fields empty.", false));
            let mut fields: BTreeMap<_, _> = input
                .fields
                .iter()
                .map(|(name, value)| (name.clone(), ammonia::clean(value)))
                .collect();
            if let Some(contexts) = &input.contexts {
                let encoded = crate::contexts::creation(&nt, &fields, contexts, &input.external_dedupe_key)
                    .map_err(|_|Failure::new("invalid_contexts", "Contexts require a migrated supported note type and exact target forms; review candidate text.", false).details(json!({"index":index})))?;
                fields.insert(crate::contexts::FIELD.into(), encoded);
            }
            let mut planned = PlannedCreate {
                schema: schema(&mut clone, input.notetype_id)?,
                tags: input.tags.clone(),
                input,
                fields,
                card_count: 0,
                normalize_text: clone.get_config_bool(anki::config::BoolKey::NormalizeNoteText),
            };
            let note = insert_planned(&mut clone, &planned, "preview")?;
            planned.fields = nt
                .fields
                .iter()
                .zip(note.fields())
                .map(|(field, value)| (field.name.clone(), value.clone()))
                .collect();
            planned.tags.clone_from(&note.tags);
            // Duplicate candidates match the same note type and exact first-field
            // text with HTML stripped. They are advisory, not semantic matching.
            let first_name = &nt.fields[0].name;
            let first =
                anki::text::strip_html_preserving_media_filenames(&planned.fields[first_name])
                    .into_owned();
            if let std::collections::btree_map::Entry::Vacant(entry) =
                duplicate_index.entry(nt.id.0)
            {
                let candidates: Vec<(i64, String)> = clone.storage.db()
                    .prepare("SELECT id, CASE WHEN instr(flds,char(31))=0 THEN flds ELSE substr(flds,1,instr(flds,char(31))-1) END FROM notes WHERE mid=?1 AND id<>?2 ORDER BY id")?
                    .query_map([nt.id.0, note.id.0], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                let mut by_text: BTreeMap<String, Vec<Value>> = BTreeMap::new();
                for (id, field) in candidates {
                    by_text
                        .entry(
                            anki::text::strip_html_preserving_media_filenames(&field).into_owned(),
                        )
                        .or_default()
                        .push(json!({"note_id":id}));
                }
                entry.insert(by_text);
            }
            let by_text = duplicate_index
                .get_mut(&nt.id.0)
                .context("duplicate index missing")?;
            let matches = by_text.entry(first).or_default();
            let duplicate_count = matches.len();
            let duplicates: Vec<_> = matches.iter().take(10).cloned().collect();
            matches.push(json!({"batch_index":index}));
            planned.card_count = clone.search_cards(note.id, SortMode::NoOrder)?.len();
            if planned.card_count == 0 {
                return Err(Failure::new(
                    "no_cards_generated",
                    "Note would generate no cards; revise its fields.",
                    false,
                )
                .details(json!({"index":index}))
                .into());
            }
            previews.push(json!({"index":index,"note":planned.input,"fields":planned.fields,"tags":note.tags,
                "card_count":planned.card_count,"duplicate_candidates":duplicates,"duplicate_count":duplicate_count,
                "duplicate_match":"same_notetype_exact_first_field_without_html","semantic_review_required":planned.input.contexts.is_some()}));
            plans.push(planned);
        }
        clone.close(None)?;
        Ok((plans, previews))
    }

    pub fn preview_create(
        &self,
        col: &mut Collection,
        subject: &str,
        inputs: Vec<Create>,
    ) -> Result<Value> {
        for input in &inputs {
            self.check_creation_key(subject, &input.external_dedupe_key)?;
        }
        let (creates, notes) = self.plan_creates(col, inputs)?;
        let token = format!(
            "{:032x}{:032x}",
            u128::from_le_bytes(rand::random()),
            u128::from_le_bytes(rand::random())
        );
        let preview = Preview {
            subject: subject.into(),
            expires: now()? + 900,
            changes: vec![],
            creates,
        };
        self.0.execute(
            "DELETE FROM previews WHERE json_extract(body,'$.expires') < ?",
            [now()?],
        )?;
        self.0.execute(
            "INSERT INTO previews VALUES (?,?)",
            (&token, serde_json::to_string(&preview)?),
        )?;
        Ok(
            json!({"preview_token":token,"expires_unix":preview.expires,"notes":notes,"requires_confirmation":true}),
        )
    }

    pub fn prepare_create(
        &self,
        col: &mut Collection,
        subject: &str,
        input: Create,
    ) -> Result<Receipt> {
        validate_key(&input.external_dedupe_key)?;
        let operation_digest = digest(&serde_json::to_value(&input)?);
        if let Some(receipt) = self.get(subject, &input.external_dedupe_key)? {
            if receipt.digest != operation_digest {
                return Err(Failure::new(
                    "idempotency_conflict",
                    "Retry key was already used for different content.",
                    false,
                )
                .into());
            }
            return Ok(receipt);
        }
        ensure!(
            self.pending()? == 0,
            Failure::new(
                "pending_operation",
                "Resolve the pending operation before starting a new write.",
                false
            )
        );
        self.check_creation_key(subject, &input.external_dedupe_key)?;
        let (creates, _) = self.plan_creates(col, vec![input.clone()])?;
        let guid = creation_guid(subject, &input.external_dedupe_key);
        let receipt = Receipt {
            digest: operation_digest,
            state: "prepared".into(),
            note_ids: vec![],
            changes: vec![],
            create: Some(input),
            creates,
            guid: Some(guid),
            completed: 0,
            preview_token: None,
        };
        self.save(
            subject,
            &receipt.create.as_ref().unwrap().external_dedupe_key,
            &receipt,
        )?;
        Ok(receipt)
    }

    pub fn reconcile_local(
        &self,
        col: &mut Collection,
        subject: &str,
        key: &str,
        receipt: &mut Receipt,
    ) -> Result<()> {
        if receipt.state != "prepared" {
            return Ok(());
        }
        if !receipt.creates.is_empty() {
            while receipt.completed < receipt.creates.len() {
                let planned = &receipt.creates[receipt.completed];
                let guid = creation_guid(subject, &planned.input.external_dedupe_key);
                let id: Option<i64> = col
                    .storage
                    .db()
                    .query_row("SELECT id FROM notes WHERE guid=?", [&guid], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if let Some(id) = id {
                    let note = col
                        .storage
                        .get_note(NoteId(id))?
                        .context("created note missing")?;
                    if !matches_plan(col, &note, planned)? {
                        receipt.state = "reconciliation_required".into();
                        break;
                    }
                    receipt.note_ids.push(id);
                    receipt.completed += 1;
                } else {
                    break;
                }
            }
            if receipt.completed == receipt.creates.len() {
                receipt.state = "saved_locally_sync_pending".into();
            }
        } else if let Some(guid) = &receipt.guid {
            let id: Option<i64> = col
                .storage
                .db()
                .query_row("SELECT id FROM notes WHERE guid=?", [guid], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(id) = id {
                receipt.note_ids = vec![id];
                receipt.state = "saved_locally_sync_pending".into();
            }
        } else {
            while receipt.completed < receipt.changes.len() {
                let change = &receipt.changes[receipt.completed];
                let note = col
                    .storage
                    .get_note(NoteId(change.before.id))?
                    .context("note missing during reconciliation")?;
                let current = revision(&note);
                if current == change.after_revision {
                    receipt.completed += 1;
                } else if current == change.before_revision {
                    break;
                } else {
                    receipt.state = "reconciliation_required".into();
                    break;
                }
            }
            if receipt.completed == receipt.changes.len() {
                receipt.state = "saved_locally_sync_pending".into();
            }
        }
        self.save(subject, key, receipt)?;
        Ok(())
    }

    /// Reconcile the gap between Anki's commit and the separate durable
    /// journal. An unrecognized state never causes a blind retry or
    /// overwrite.
    pub fn commit_local(
        &self,
        col: &mut Collection,
        subject: &str,
        key: &str,
        receipt: &mut Receipt,
    ) -> Result<()> {
        if receipt.state != "prepared" {
            return Ok(());
        }
        if !receipt.creates.is_empty() {
            for ordinal in receipt.completed..receipt.creates.len() {
                let planned = &receipt.creates[ordinal];
                if schema(col, planned.input.notetype_id)? != planned.schema
                    || col.get_config_bool(anki::config::BoolKey::NormalizeNoteText)
                        != planned.normalize_text
                {
                    receipt.state = "reconciliation_required".into();
                    self.save(subject, key, receipt)?;
                    return Err(Failure::new(
                        "stale_schema",
                        "Note type changed during creation; reconciliation required.",
                        false,
                    )
                    .into());
                }
                let guid = creation_guid(subject, &planned.input.external_dedupe_key);
                let existing: Option<i64> = col
                    .storage
                    .db()
                    .query_row("SELECT id FROM notes WHERE guid=?", [&guid], |row| {
                        row.get(0)
                    })
                    .optional()?;
                let id = if let Some(id) = existing {
                    let note = col
                        .storage
                        .get_note(NoteId(id))?
                        .context("created note missing")?;
                    if !matches_plan(col, &note, planned)? {
                        receipt.state = "reconciliation_required".into();
                        self.save(subject, key, receipt)?;
                        return Err(Failure::new(
                            "creation_conflict",
                            "Existing creation changed; reconciliation required.",
                            false,
                        )
                        .into());
                    }
                    id
                } else {
                    let note = insert_planned(col, planned, &guid)?;
                    if !matches_plan(col, &note, planned)?
                        || col.search_cards(note.id, SortMode::NoOrder)?.len() != planned.card_count
                    {
                        receipt.note_ids.push(note.id.0);
                        receipt.state = "reconciliation_required".into();
                        self.save(subject, key, receipt)?;
                        return Err(Failure::new("creation_conflict", "Created content differed from the reviewed preview; reconciliation required.", false).into());
                    }
                    note.id.0
                };
                receipt.note_ids.push(id);
                receipt.completed = ordinal + 1;
                self.save(subject, key, receipt)?;
            }
        } else if let Some(input) = &receipt.create {
            let guid = receipt.guid.as_ref().context("creation GUID missing")?;
            let existing: Option<i64> = col
                .storage
                .db()
                .query_row("SELECT id FROM notes WHERE guid=?", [guid], |row| {
                    row.get(0)
                })
                .optional()?;
            let id = if let Some(id) = existing {
                id
            } else {
                let nt = col
                    .get_notetype(NotetypeId(input.notetype_id))?
                    .context("note type missing")?;
                let mut note = nt.new_note();
                note.guid.clone_from(guid);
                note.tags.clone_from(&input.tags);
                for (ordinal, field) in nt.fields.iter().enumerate() {
                    note.set_field(
                        ordinal,
                        ammonia::clean(input.fields.get(&field.name).context("schema changed")?),
                    )?;
                }
                col.add_note(&mut note, DeckId(input.deck_id))?;
                note.id.0
            };
            receipt.note_ids = vec![id];
        } else {
            for ordinal in receipt.completed..receipt.changes.len() {
                let change = &receipt.changes[ordinal];
                let mut note = col
                    .storage
                    .get_note(NoteId(change.before.id))?
                    .context("note missing")?;
                let current = revision(&note);
                if current != change.after_revision {
                    if current != change.before_revision
                        || schema(col, note.notetype_id.0)? != change.schema
                    {
                        receipt.state = "reconciliation_required".into();
                        self.save(subject, key, receipt)?;
                        bail!("collection changed during interrupted commit; manual reconciliation required");
                    }
                    change.after.restore(&mut note)?;
                    col.update_note(&mut note)?;
                    ensure!(
                        revision(
                            &col.storage
                                .get_note(note.id)?
                                .context("updated note missing")?
                        ) == change.after_revision,
                        "unexpected normalization"
                    );
                }
                receipt.completed = ordinal + 1;
                self.save(subject, key, receipt)?;
            }
        }
        receipt.state = "saved_locally_sync_pending".into();
        self.save(subject, key, receipt)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &Path) -> Result<(Collection, NoteId)> {
        let mut col = CollectionBuilder::new(path).build()?;
        let nt = col.get_notetype_by_name("Basic")?.unwrap();
        let mut note = nt.new_note();
        note.set_field(0, "original quotation")?;
        note.set_field(1, "original answer")?;
        col.add_note(&mut note, DeckId(1))?;
        let card = col.search_cards(note.id, SortMode::NoOrder)?[0];
        col.storage.db().execute(
            "insert into revlog values (1234567890000, ?1, -1, 3, 10, 5, 2500, 1234, 1)",
            [card.0],
        )?;
        Ok((col, note.id))
    }

    fn patch(id: NoteId, value: &str) -> Update {
        Update {
            note_id: id.0,
            expected_revision: None,
            field_patch: BTreeMap::from([("Back".into(), value.into())]),
            tag_patch: TagPatch::default(),
        }
    }

    #[test]
    fn new_cloze_cards_are_rejected_before_the_source_collection_changes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut col = CollectionBuilder::new(temp.path().join("collection.anki2")).build()?;
        let nt = col.get_notetype_by_name("Cloze")?.unwrap();
        let mut note = nt.new_note();
        note.set_field(0, "{{c1::eat}}")?;
        col.add_note(&mut note, DeckId(1))?;
        let cards = col.search_cards(note.id, SortMode::NoOrder)?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let update = Update {
            note_id: note.id.0,
            expected_revision: None,
            field_patch: BTreeMap::from([("Text".into(), "{{c1::eat}} {{c2::drink}}".into())]),
            tag_patch: TagPatch::default(),
        };
        assert!(journal.preview(&mut col, "owner", vec![update]).is_err());
        assert_eq!(
            col.storage.get_note(note.id)?.unwrap().fields()[0],
            "{{c1::eat}}"
        );
        assert_eq!(col.search_cards(note.id, SortMode::NoOrder)?, cards);
        Ok(())
    }

    #[test]
    fn preview_is_nonmutating_and_commit_preserves_ids_quotation_and_reviews() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, id) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let cards = col.search_cards(id, SortMode::NoOrder)?;
        let review: Vec<i64> = col.storage.db().query_row(
            "select id,cid,ease,ivl,lastIvl,factor,time from revlog",
            [],
            |r| (0..7).map(|i| r.get(i)).collect(),
        )?;
        let preview = journal.preview(&mut col, "owner", vec![patch(id, "updated answer")])?;
        assert_eq!(
            col.storage.get_note(id)?.unwrap().fields()[1],
            "original answer"
        );
        let token = preview["preview_token"].as_str().unwrap();
        assert!(journal.load_preview("other", token).is_err());
        let mut receipt = journal.prepare_update(
            &mut col,
            "owner",
            "request-1",
            token,
            journal.load_preview("owner", token)?,
        )?;
        journal.commit_local(&mut col, "owner", "request-1", &mut receipt)?;
        let note = col.storage.get_note(id)?.unwrap();
        assert_eq!(note.fields(), &["original quotation", "updated answer"]);
        assert_eq!(col.search_cards(id, SortMode::NoOrder)?, cards);
        let after: Vec<i64> = col.storage.db().query_row(
            "select id,cid,ease,ivl,lastIvl,factor,time from revlog",
            [],
            |r| (0..7).map(|i| r.get(i)).collect(),
        )?;
        assert_eq!(after, review);
        assert_eq!(receipt.state, "saved_locally_sync_pending");
        journal.commit_local(&mut col, "owner", "request-1", &mut receipt)?;
        assert_eq!(col.storage.get_all_note_ids()?.len(), 1);
        journal.mark_synced()?;
        assert_eq!(
            journal.get("owner", "request-1")?.unwrap().state,
            "synced_successfully"
        );
        Ok(())
    }

    #[test]
    fn stale_expired_and_unknown_field_previews_never_apply() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, id) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let result = journal.preview(&mut col, "owner", vec![patch(id, "new answer")])?;
        let token = result["preview_token"].as_str().unwrap();
        let mut preview = journal.load_preview("owner", token)?;
        preview.expires = 0;
        assert!(journal
            .prepare_update(&mut col, "owner", "expired", token, preview)
            .is_err());
        let mut note = col.storage.get_note(id)?.unwrap();
        note.set_field(1, "device changed answer")?;
        col.update_note(&mut note)?;
        assert!(journal
            .prepare_update(
                &mut col,
                "owner",
                "stale",
                token,
                journal.load_preview("owner", token)?
            )
            .is_err());
        assert_eq!(
            col.storage.get_note(id)?.unwrap().fields()[1],
            "device changed answer"
        );
        assert_eq!(journal.pending()?, 0);
        assert!(journal
            .preview(
                &mut col,
                "owner",
                vec![Update {
                    field_patch: BTreeMap::from([("nonexistent".into(), "x".into())]),
                    ..patch(id, "x")
                }]
            )
            .is_err());
        Ok(())
    }

    #[test]
    fn restart_reconciles_a_commit_before_its_receipt_and_does_not_duplicate_creation() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("collection.anki2");
        let journal_path = temp.path().join("journal.sqlite");
        let (mut col, id) = fixture(&path)?;
        let journal = Journal::open(&journal_path)?;
        let result = journal.preview(&mut col, "owner", vec![patch(id, "after crash")])?;
        let token = result["preview_token"].as_str().unwrap();
        let receipt = journal.prepare_update(
            &mut col,
            "owner",
            "crash-update",
            token,
            journal.load_preview("owner", token)?,
        )?;
        // The exact durable gap: Anki committed, but the receipt still says prepared.
        let mut note = col.storage.get_note(id)?.unwrap();
        receipt.changes[0].after.restore(&mut note)?;
        col.update_note(&mut note)?;
        col.close(None)?;
        drop(journal);
        let mut col = CollectionBuilder::new(&path).build()?;
        let journal = Journal::open(&journal_path)?;
        journal.recover_all(&mut col)?;
        assert_eq!(
            journal.get("owner", "crash-update")?.unwrap().state,
            "saved_locally_sync_pending"
        );
        journal.mark_synced()?;
        let nt = col.get_notetype_by_name("Basic")?.unwrap();
        let input = Create {
            deck_id: 1,
            notetype_id: nt.id.0,
            fields: BTreeMap::from([
                ("Front".into(), "new question".into()),
                ("Back".into(), "new answer".into()),
            ]),
            tags: vec![],
            external_dedupe_key: "correction-1".into(),
            contexts: None,
        };
        let receipt = journal.prepare_create(&mut col, "owner", input.clone())?;
        let mut note = nt.new_note();
        note.guid = receipt.guid.unwrap();
        note.set_field(0, "new question")?;
        note.set_field(1, "new answer")?;
        col.add_note(&mut note, DeckId(1))?;
        let created_id = note.id;
        col.close(None)?;
        drop(journal);
        let mut col = CollectionBuilder::new(&path).build()?;
        let journal = Journal::open(&journal_path)?;
        journal.recover_all(&mut col)?;
        let mut receipt = journal.prepare_create(&mut col, "owner", input.clone())?;
        journal.commit_local(&mut col, "owner", "correction-1", &mut receipt)?;
        assert_eq!(receipt.note_ids, vec![created_id.0]);
        assert_eq!(col.storage.get_all_note_ids()?.len(), 2);
        let mut conflicting = input;
        conflicting
            .fields
            .insert("Back".into(), "different request".into());
        assert!(journal
            .prepare_create(&mut col, "owner", conflicting)
            .is_err());
        Ok(())
    }

    #[test]
    fn batch_bounds_and_interrupted_partial_progress_are_explicit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, first) = fixture(&temp.path().join("collection.anki2"))?;
        let nt = col.get_notetype_by_name("Basic")?.unwrap();
        let mut note = nt.new_note();
        note.set_field(0, "second question")?;
        col.add_note(&mut note, DeckId(1))?;
        let second = note.id;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        assert!(journal
            .preview(&mut col, "owner", vec![patch(first, "x"); 21])
            .is_err());
        assert!(journal
            .preview(
                &mut col,
                "owner",
                vec![patch(first, "x"), patch(first, "y")]
            )
            .is_err());
        let result = journal.preview(
            &mut col,
            "owner",
            vec![patch(first, "one"), patch(second, "two")],
        )?;
        let token = result["preview_token"].as_str().unwrap();
        let mut receipt = journal.prepare_update(
            &mut col,
            "owner",
            "batch",
            token,
            journal.load_preview("owner", token)?,
        )?;
        let mut note = col.storage.get_note(first)?.unwrap();
        receipt.changes[0].after.restore(&mut note)?;
        col.update_note(&mut note)?;
        journal.reconcile_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(receipt.completed, 1);
        journal.commit_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(col.storage.get_note(second)?.unwrap().fields()[1], "two");
        assert_eq!(receipt.completed, 2);
        Ok(())
    }
    fn creation(col: &mut Collection, key: &str, front: &str) -> Result<Create> {
        Ok(Create {
            deck_id: 1,
            notetype_id: col.get_notetype_by_name("Basic")?.unwrap().id.0,
            fields: BTreeMap::from([
                ("Front".into(), front.into()),
                ("Back".into(), "<b onclick='bad()'>answer</b>".into()),
            ]),
            tags: vec!["authoring".into()],
            external_dedupe_key: key.into(),
            contexts: None,
        })
    }

    #[test]
    fn tag_only_preview_checks_the_original_read_revision() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, id) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let original = col.storage.get_note(id)?.unwrap();
        let input: Update = serde_json::from_value(
            json!({"note_id":id.0,"expected_revision":revision(&original),"tag_patch":{"add":["edited"]}}),
        )?;
        let result = journal.preview(&mut col, "owner", vec![input.clone()])?;
        let token = result["preview_token"].as_str().unwrap();
        let mut receipt = journal.prepare_update(
            &mut col,
            "owner",
            "tag-edit",
            token,
            journal.load_preview("owner", token)?,
        )?;
        journal.commit_local(&mut col, "owner", "tag-edit", &mut receipt)?;
        let edited = col.storage.get_note(id)?.unwrap();
        assert_eq!(edited.fields(), original.fields());
        assert_eq!(edited.tags, vec!["edited"]);
        let error = journal
            .preview(&mut col, "owner", vec![input])
            .err()
            .unwrap();
        assert_eq!(Failure::from_error(&error).code, "stale_revision");
        Ok(())
    }

    #[test]
    fn creation_preview_sanitizes_and_reports_collection_and_batch_duplicates() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, id) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let first = creation(&mut col, "first", "<b>original quotation</b>")?;
        let second = creation(&mut col, "second", "original quotation")?;
        let preview = journal.preview_create(&mut col, "owner", vec![first, second])?;
        assert_eq!(col.storage.get_all_note_ids()?.len(), 1);
        assert_eq!(preview["notes"][0]["fields"]["Back"], "<b>answer</b>");
        assert_eq!(preview["notes"][0]["card_count"], 1);
        assert_eq!(
            preview["notes"][0]["duplicate_candidates"][0]["note_id"],
            id.0
        );
        assert_eq!(preview["notes"][1]["duplicate_count"], 2);
        assert_eq!(
            preview["notes"][1]["duplicate_candidates"][1]["batch_index"],
            0
        );
        let token = preview["preview_token"].as_str().unwrap();
        assert!(journal.load_preview("other", token).is_err());
        let mut receipt = journal.prepare_update(
            &mut col,
            "owner",
            "batch",
            token,
            journal.load_preview("owner", token)?,
        )?;
        journal.commit_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(receipt.completed_notes(), 2);
        assert_eq!(receipt.note_ids.len(), 2);
        let note = col.storage.get_note(NoteId(receipt.note_ids[0]))?.unwrap();
        assert_eq!(note.fields()[1], "<b>answer</b>");
        journal.commit_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(col.storage.get_all_note_ids()?.len(), 3);
        journal.mark_synced()?;
        let replay = journal.prepare_update(
            &mut col,
            "owner",
            "batch",
            token,
            journal.load_preview("owner", token)?,
        )?;
        assert_eq!(replay.note_ids, receipt.note_ids);
        let input = creation(&mut col, "first", "different")?;
        assert_eq!(
            Failure::from_error(
                &journal
                    .prepare_create(&mut col, "owner", input)
                    .err()
                    .unwrap()
            )
            .code,
            "creation_key_conflict"
        );
        Ok(())
    }

    #[test]
    fn creation_batch_resumes_after_a_commit_without_its_progress_receipt() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("collection.anki2");
        let journal_path = temp.path().join("journal.sqlite");
        let (mut col, _) = fixture(&path)?;
        let journal = Journal::open(&journal_path)?;
        let first = creation(&mut col, "first", "one")?;
        let second = creation(&mut col, "second", "two")?;
        let preview = journal.preview_create(&mut col, "owner", vec![first, second])?;
        let token = preview["preview_token"].as_str().unwrap();
        let receipt = journal.prepare_update(
            &mut col,
            "owner",
            "batch",
            token,
            journal.load_preview("owner", token)?,
        )?;
        let first_id = insert_planned(
            &mut col,
            &receipt.creates[0],
            &creation_guid("owner", "first"),
        )?
        .id
        .0;
        col.close(None)?;
        drop(journal);
        let mut col = CollectionBuilder::new(&path).build()?;
        let journal = Journal::open(&journal_path)?;
        journal.recover_all(&mut col)?;
        let mut receipt = journal.get("owner", "batch")?.unwrap();
        assert_eq!(receipt.completed_notes(), 1);
        assert_eq!(receipt.note_ids, vec![first_id]);
        journal.commit_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(receipt.completed_notes(), 2);
        assert_eq!(receipt.note_ids[0], first_id);
        assert_eq!(col.storage.get_all_note_ids()?.len(), 3);
        Ok(())
    }

    #[test]
    fn invalid_creation_batches_and_stale_schemas_never_commit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, _) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let first = creation(&mut col, "first", "one")?;
        assert!(journal.preview_create(&mut col, "owner", vec![]).is_err());
        assert!(journal
            .preview_create(&mut col, "owner", vec![first.clone(); 21])
            .is_err());
        assert!(journal
            .preview_create(&mut col, "owner", vec![first.clone(), first.clone()])
            .is_err());
        let mut invalid = creation(&mut col, "second", "two")?;
        invalid.fields.remove("Back");
        let error = journal
            .preview_create(&mut col, "owner", vec![first.clone(), invalid])
            .err()
            .unwrap();
        assert_eq!(Failure::from_error(&error).code, "invalid_fields");
        assert_eq!(journal.pending()?, 0);
        assert_eq!(col.storage.get_all_note_ids()?.len(), 1);
        let preview = journal.preview_create(&mut col, "owner", vec![first.clone()])?;
        let token = preview["preview_token"].as_str().unwrap();
        let mut nt = col
            .get_notetype(NotetypeId(first.notetype_id))?
            .unwrap()
            .as_ref()
            .clone();
        nt.config.css.push_str("\n.card { color: red; }");
        col.update_notetype(&mut nt, false)?;
        let error = journal
            .prepare_update(
                &mut col,
                "owner",
                "stale",
                token,
                journal.load_preview("owner", token)?,
            )
            .err()
            .unwrap();
        assert_eq!(Failure::from_error(&error).code, "stale_schema");
        assert_eq!(journal.pending()?, 0);
        assert_eq!(col.storage.get_all_note_ids()?.len(), 1);
        Ok(())
    }
    #[test]
    fn creation_rejects_changed_text_normalization_before_committing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, _) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let input = creation(&mut col, "normalization", "cafe\u{301}")?;
        let preview = journal.preview_create(&mut col, "owner", vec![input])?;
        assert_eq!(preview["notes"][0]["fields"]["Front"], "café");
        let token = preview["preview_token"].as_str().unwrap();
        col.set_config_bool(anki::config::BoolKey::NormalizeNoteText, false, false)?;
        let error = journal
            .prepare_update(
                &mut col,
                "owner",
                "normalized-batch",
                token,
                journal.load_preview("owner", token)?,
            )
            .err()
            .unwrap();
        assert_eq!(Failure::from_error(&error).code, "stale_schema");
        assert_eq!(journal.pending()?, 0);
        assert_eq!(col.storage.get_all_note_ids()?.len(), 1);
        Ok(())
    }

    #[test]
    fn legacy_creation_receipts_without_batch_plans_remain_recoverable() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, _) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let input = creation(&mut col, "legacy", "legacy question")?;
        let receipt = journal.prepare_create(&mut col, "owner", input.clone())?;
        let mut body = serde_json::to_value(&receipt)?;
        body.as_object_mut().unwrap().remove("creates");
        // Old Create payloads also predate optional creation contexts.
        assert!(body["create"].get("contexts").is_none());
        let mut old: Receipt = serde_json::from_value(body)?;
        journal.save("owner", "legacy", &old)?;
        journal.commit_local(&mut col, "owner", "legacy", &mut old)?;
        let replay = journal.prepare_create(&mut col, "owner", input)?;
        assert_eq!(replay.note_ids, old.note_ids);
        assert_eq!(col.storage.get_all_note_ids()?.len(), 2);
        Ok(())
    }

    #[test]
    fn interrupted_creation_does_not_overwrite_a_changed_note() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (mut col, _) = fixture(&temp.path().join("collection.anki2"))?;
        let journal = Journal::open(&temp.path().join("journal.sqlite"))?;
        let first = creation(&mut col, "first", "one")?;
        let second = creation(&mut col, "second", "two")?;
        let preview = journal.preview_create(&mut col, "owner", vec![first, second])?;
        let token = preview["preview_token"].as_str().unwrap();
        let mut receipt = journal.prepare_update(
            &mut col,
            "owner",
            "batch",
            token,
            journal.load_preview("owner", token)?,
        )?;
        let mut first = insert_planned(
            &mut col,
            &receipt.creates[0],
            &creation_guid("owner", "first"),
        )?;
        first.set_field(1, "edited on another client")?;
        col.update_note(&mut first)?;
        journal.reconcile_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(receipt.state, "reconciliation_required");
        journal.commit_local(&mut col, "owner", "batch", &mut receipt)?;
        assert_eq!(
            col.storage.get_note(first.id)?.unwrap().fields()[1],
            "edited on another client"
        );
        assert_eq!(col.storage.get_all_note_ids()?.len(), 2);
        Ok(())
    }
}
