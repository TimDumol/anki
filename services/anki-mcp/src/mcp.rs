// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use axum::http::request::Parts;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResponse;
use rmcp::model::CallToolResult;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerConfig;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::RequestContext;
use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use serde::Deserialize;
use serde_json::json;

use crate::actor::Actor;
use crate::actor::Command;
use crate::auth::Principal;
use crate::contexts;
use crate::writes;

#[derive(Clone)]
pub struct Mcp {
    actor: Actor,
}

impl Mcp {
    pub fn new(actor: Actor) -> Self {
        Self { actor }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    fields: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notes {
    note_ids: Vec<i64>,
    #[serde(default)]
    fields: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreationPreview {
    notes: Vec<writes::Create>,
}

fn default_limit() -> usize {
    20
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Note {
    note_id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notetype {
    notetype_id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkPreview {
    updates: Vec<writes::Update>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    preview_token: String,
    idempotency_key: String,
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationApply {
    notetype_id: i64,
    expected_revision: String,
    idempotency_key: String,
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateApply {
    update: crate::templates::Update,
    expected_revision: String,
    idempotency_key: String,
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    note: writes::Create,
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Correction {
    note: writes::Create,
    lemma: String,
    category: String,
    observed_error: String,
    corrected_form: String,
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Weakness {
    #[serde(default)]
    notetype: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    lemma: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let definitions = [
            ("anki_status", "Inspect collection counts, explicit local sync state, pending operations and guarded write availability.", json!({}), json!([])),
            ("anki_search_notes", "Automatically synchronize, then search Anki notes with bounded previews and pagination.", json!({
                "query": {"type":"string", "maxLength":2048}, "limit": {"type":"integer", "minimum":1, "maximum":100, "default":20},
                "offset": {"type":"integer", "minimum":0, "maximum":100000}, "fields":projection_schema()}), json!(["query"])),
            ("anki_get_authoring_context", "Automatically synchronize and discover decks, note types, named field schemas and authoring capabilities in one call. Check permissions and writes_enabled before authoring.", json!({}), json!([])),
            ("anki_get_notes", "Read up to 100 notes in one synchronized snapshot, including revisions, tags and card IDs. Optional fields selects named fields; an empty array returns metadata only. Missing notes are reported per item.", json!({"note_ids":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"integer"}},"fields":projection_schema()}),json!(["note_ids"])),
            ("anki_preview_notes_creation", "Preview 1..20 notes in a private clone after automatic sync. Returns exact sanitized fields, card counts, advisory duplicate candidates and a 15-minute token. Review content and context equivalence, then apply with anki_apply_notes_creation. No collection content is changed.", json!({"notes":{"type":"array","minItems":1,"maxItems":20,"items":create_schema()}}),json!(["notes"])),
            ("anki_apply_notes_creation", "Create a reviewed batch using its preview token, explicit confirmation and retry key. Per-note progress is durable; a batch is not cross-sync atomic. External dedupe keys identify individual notes. Poll anki_get_operation after an ambiguous result.", apply_properties(),json!(["preview_token","idempotency_key","confirmed"])),
            ("anki_get_note", "Read fields, tags, card IDs and a revision hash.", json!({"note_id":{"type":"integer"}}), json!(["note_id"])),
            ("anki_list_decks", "List deck IDs and names.", json!({}), json!([])),
            ("anki_get_notetype", "Read field ordinals, template source and CSS.", json!({"notetype_id":{"type":"integer"}}), json!(["notetype_id"])),
            ("anki_preview_note_update", "Preview a named-field/tag patch after synchronizing. Supply expected_revision from the original read to detect intervening edits. field_patch may be omitted for tag-only changes. Returns before/after, revision and a 15-minute token; does not change your collection.", update_properties(), json!(["note_id"])),
            ("anki_preview_bulk_update", "Preview up to 20 distinct notes. Rejects card generation changes. Bulk commits have per-note durable progress, not cross-sync atomicity.", json!({"updates":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","properties":update_properties(),"required":["note_id"],"additionalProperties":false}}}), json!(["updates"])),
            ("anki_apply_note_update", "Apply a reviewed preview with explicit confirmation, a principal-bound retry key and stale-edit checks. Requires anki:write_notes and enabled writes.", apply_properties(), json!(["preview_token","idempotency_key","confirmed"])),
            ("anki_apply_bulk_update", "Apply a reviewed manifest of at most 20 notes; partial local progress is journaled. Requires anki:write_notes and explicit confirmation.", apply_properties(), json!(["preview_token","idempotency_key","confirmed"])),
            ("anki_create_notes", "Legacy single-note creation; prefer anki_preview_notes_creation and anki_apply_notes_creation for sanitized previews and batches. Create one reviewed note with all named fields, explicit confirmation and a durable external dedupe key. Reusing a key with different content is rejected. Requires anki:write_notes.", json!({"note":create_schema(),"confirmed":{"type":"boolean"}}), json!(["note","confirmed"])),
            ("anki_get_operation", "Retry pending synchronization and read your durable receipt, per-note progress and rollback manifest using the original retry/dedupe key. Remains readable if synchronization fails.", json!({"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}), json!(["idempotency_key"])),
            ("anki_preview_template_update", "Preview edits to existing card fronts/backs and note-type CSS in a private collection clone. Preserves fields, card IDs, scheduling and reviews. Returns source diffs and an exact revision. Applying uses backed-up maintenance and requires full DOWNLOAD on other devices.", template_properties(), json!(["notetype_id"])),
            ("anki_apply_template_update", "Apply the reviewed template/CSS update with its exact revision, explicit confirmation and retry key. Requires anki:maintenance and enabled writes. Host maintenance backs up both collections, verifies the server upload and preserves cards/reviews. Poll anki_get_operation. Other devices must full DOWNLOAD.", json!({"update":{"type":"object","properties":template_properties(),"required":["notetype_id"],"additionalProperties":false},"expected_revision":{"type":"string","minLength":64,"maxLength":64},"idempotency_key":{"type":"string","minLength":1,"maxLength":128},"confirmed":{"type":"boolean"}}), json!(["update","expected_revision","idempotency_key","confirmed"])),
            ("anki_preview_notetype_migration", "Preview the fixed versioned AlternateContexts migration for Dict v2 or Readest v3. Review source diffs, affected counts and full-sync requirements before requesting maintenance.", json!({"notetype_id":{"type":"integer"}}),json!(["notetype_id"])),
            ("anki_apply_notetype_migration", "Request a reviewed migration with its exact revision and explicit confirmation. Requires anki:maintenance. The host coordinator blocks public Anki sync, stops MCP, snapshots both volumes, applies the fixed migration, verifies a fresh server download, then reopens sync. Devices will need a full DOWNLOAD. Poll anki_get_operation; ambiguous failures remain blocked for operator recovery.",json!({"notetype_id":{"type":"integer"},"expected_revision":{"type":"string","maxLength":64},"idempotency_key":{"type":"string","maxLength":128},"confirmed":{"type":"boolean"}}),json!(["notetype_id","expected_revision","idempotency_key","confirmed"])),
            ("anki_preview_context_enrichment", "Validate up to five candidate contexts per direction against the exact original target/form. Preserves original quotations. Returns a guarded write preview if the dedicated field exists; otherwise reports required migration. You must review linguistic equivalence.", json!({"note_id":{"type":"integer"},"recognition":{"type":"array","maxItems":5,"items":context_schema()},"production":{"type":"array","maxItems":5,"items":context_schema()}}),json!(["note_id","recognition","production"])),
            ("anki_apply_context_enrichment", "Commit a validated context preview after reviewing linguistic equivalence, with explicit confirmation and an idempotency key. Preserves quotations, cards and review history.",apply_properties(),json!(["preview_token","idempotency_key","confirmed"])),
            ("anki_create_card_from_correction", "Create a reviewed Spanish practice note from an observed error. Supply every named field of the chosen note type. Records error/correction in UsageNote, Extra or Back, and searchable correction/lemma/category tags. Requires explicit confirmation and anki:write_notes.",json!({"note":create_schema(),"lemma":{"type":"string","maxLength":64},"category":{"type":"string","maxLength":64},"observed_error":{"type":"string","maxLength":2048},"corrected_form":{"type":"string","maxLength":2048},"confirmed":{"type":"boolean"}}),json!(["note","lemma","category","observed_error","corrected_form","confirmed"])),
            ("anki_find_weakness_cards", "Find correction notes by note type, tag and lemma using bounded Anki searches. Returns note previews; anki_get_note includes generated card IDs.",json!({"notetype":{"type":"string","maxLength":128},"tag":{"type":"string","maxLength":128},"lemma":{"type":"string","maxLength":64},"limit":{"type":"integer","minimum":1,"maximum":100},"offset":{"type":"integer","minimum":0,"maximum":100000}}),json!([])),
        ];
        let tools = definitions.into_iter().filter(|(name,_,_,_)| self.actor.writes_enabled() || !name.starts_with("anki_apply_") && !name.starts_with("anki_create_")).map(|(name, description, properties, required)| {
            let schema = json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false});
            Tool::new(name, description, schema.as_object().unwrap().clone())
                .with_annotations(ToolAnnotations::from_raw(None, Some(!name.starts_with("anki_apply_") && !name.starts_with("anki_create_")), Some(false), Some(true), Some(true)))
        }).collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let principal = ctx
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<Principal>())
            .ok_or_else(|| ErrorData::invalid_request("verified principal required", None))?;
        if !principal.has_scope("anki:read") {
            return Err(ErrorData::invalid_request("anki:read required", None));
        }
        let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let invalid = |_| ErrorData::invalid_params("invalid tool arguments", None);
        if matches!(
            request.name.as_ref(),
            "anki_apply_notetype_migration" | "anki_apply_template_update"
        ) && !principal.has_permission("anki:maintenance")
        {
            return Err(ErrorData::invalid_request(
                "anki:maintenance required",
                None,
            ));
        }
        if (request.name.starts_with("anki_apply_")
            && !matches!(
                request.name.as_ref(),
                "anki_apply_notetype_migration" | "anki_apply_template_update"
            )
            || request.name.starts_with("anki_create_"))
            && !principal.has_permission("anki:write_notes")
        {
            return Err(ErrorData::invalid_request(
                "anki:write_notes required",
                None,
            ));
        }
        let command = match request.name.as_ref() {
            "anki_status" | "anki_list_decks" | "anki_get_authoring_context" => {
                if arguments.as_object().is_none_or(|a| !a.is_empty()) {
                    return Err(ErrorData::invalid_params("tool takes no arguments", None));
                }
                if request.name == "anki_status" {
                    Command::Status
                } else if request.name == "anki_get_authoring_context" {
                    Command::AuthoringContext
                } else {
                    Command::Decks
                }
            }
            "anki_get_notes" => {
                let input: Notes = serde_json::from_value(arguments).map_err(invalid)?;
                Command::Notes {
                    note_ids: input.note_ids,
                    fields: input.fields,
                }
            }
            "anki_preview_notes_creation" => {
                let input: CreationPreview = serde_json::from_value(arguments).map_err(invalid)?;
                Command::CreatePreview {
                    subject: principal.sub.clone(),
                    notes: input.notes,
                }
            }
            "anki_search_notes" => {
                let input: Search = serde_json::from_value(arguments).map_err(invalid)?;
                if input.query.len() > 2048
                    || !(1..=100).contains(&input.limit)
                    || input.offset > 100000
                {
                    return Err(ErrorData::invalid_params("search exceeds limits", None));
                }
                Command::Search {
                    query: input.query,
                    offset: input.offset,
                    limit: input.limit,
                    fields: input.fields,
                }
            }
            "anki_get_note" => Command::Note(
                serde_json::from_value::<Note>(arguments)
                    .map_err(invalid)?
                    .note_id,
            ),
            "anki_get_notetype" => Command::Notetype(
                serde_json::from_value::<Notetype>(arguments)
                    .map_err(invalid)?
                    .notetype_id,
            ),
            "anki_preview_note_update" => Command::Preview {
                subject: principal.sub.clone(),
                updates: vec![serde_json::from_value(arguments).map_err(invalid)?],
            },
            "anki_preview_bulk_update" => Command::Preview {
                subject: principal.sub.clone(),
                updates: serde_json::from_value::<BulkPreview>(arguments)
                    .map_err(invalid)?
                    .updates,
            },
            "anki_apply_notes_creation"
            | "anki_apply_note_update"
            | "anki_apply_bulk_update"
            | "anki_apply_context_enrichment" => {
                let input: Apply = serde_json::from_value(arguments).map_err(invalid)?;
                Command::Apply {
                    subject: principal.sub.clone(),
                    token: input.preview_token,
                    key: input.idempotency_key,
                    confirmed: input.confirmed,
                }
            }
            "anki_create_notes" => {
                let input: Create = serde_json::from_value(arguments).map_err(invalid)?;
                Command::Create {
                    subject: principal.sub.clone(),
                    input: input.note,
                    confirmed: input.confirmed,
                }
            }
            "anki_create_card_from_correction" => {
                let mut input: Correction = serde_json::from_value(arguments).map_err(invalid)?;
                let lemma = metadata_tag(&input.lemma)?;
                let category = metadata_tag(&input.category)?;
                if input.observed_error.is_empty()
                    || input.corrected_form.is_empty()
                    || input.observed_error.len() > 2048
                    || input.corrected_form.len() > 2048
                {
                    return Err(ErrorData::invalid_params(
                        "bounded correction text required",
                        None,
                    ));
                }
                let field = ["UsageNote","Extra","Back"].into_iter().find(|f| input.note.fields.contains_key(*f)).ok_or_else(|| ErrorData::invalid_params("note type needs UsageNote, Extra or Back to preserve correction metadata",None))?;
                let metadata = format!(
                    "<div><b>Observed error:</b> {}<br><b>Correction:</b> {}</div>",
                    escape_html(&input.observed_error),
                    escape_html(&input.corrected_form)
                );
                input
                    .note
                    .fields
                    .get_mut(field)
                    .unwrap()
                    .push_str(&metadata);
                input.note.tags.extend([
                    "anki-mcp::correction".into(),
                    format!("anki-mcp::lemma::{lemma}"),
                    format!("anki-mcp::weakness::{category}"),
                ]);
                Command::Create {
                    subject: principal.sub.clone(),
                    input: input.note,
                    confirmed: input.confirmed,
                }
            }
            "anki_find_weakness_cards" => {
                let input: Weakness = serde_json::from_value(arguments).map_err(invalid)?;
                let mut query = "tag:anki-mcp::correction".to_string();
                for (prefix, value) in [("note", input.notetype), ("tag", input.tag)] {
                    if let Some(value) = value {
                        if value.len() > 128 || value.chars().any(char::is_control) {
                            return Err(ErrorData::invalid_params("invalid filter", None));
                        }
                        query.push_str(&format!(
                            " {prefix}:\"{}\"",
                            value.replace('\\', "\\\\").replace('"', "\\\"")
                        ));
                    }
                }
                if let Some(lemma) = input.lemma {
                    query.push_str(&format!(" tag:anki-mcp::lemma::{}", metadata_tag(&lemma)?));
                }
                Command::Search {
                    query,
                    offset: input.offset,
                    limit: input.limit,
                    fields: None,
                }
            }
            "anki_get_operation" => Command::Operation {
                subject: principal.sub.clone(),
                key: serde_json::from_value::<Operation>(arguments)
                    .map_err(invalid)?
                    .idempotency_key,
            },
            "anki_preview_template_update" => {
                Command::TemplatePreview(serde_json::from_value(arguments).map_err(invalid)?)
            }
            "anki_apply_template_update" => {
                let input: TemplateApply = serde_json::from_value(arguments).map_err(invalid)?;
                Command::MigrationApply {
                    subject: principal.sub.clone(),
                    notetype_id: input.update.notetype_id,
                    revision: input.expected_revision,
                    template_update: Some(input.update),
                    key: input.idempotency_key,
                    confirmed: input.confirmed,
                }
            }
            "anki_preview_notetype_migration" => Command::MigrationPreview(
                serde_json::from_value::<Notetype>(arguments)
                    .map_err(invalid)?
                    .notetype_id,
            ),
            "anki_apply_notetype_migration" => {
                let input: MigrationApply = serde_json::from_value(arguments).map_err(invalid)?;
                Command::MigrationApply {
                    subject: principal.sub.clone(),
                    notetype_id: input.notetype_id,
                    revision: input.expected_revision,
                    template_update: None,
                    key: input.idempotency_key,
                    confirmed: input.confirmed,
                }
            }
            "anki_preview_context_enrichment" => Command::ContextPreview {
                subject: principal.sub.clone(),
                input: serde_json::from_value::<contexts::Enrichment>(arguments)
                    .map_err(invalid)?,
            },
            _ => return Err(ErrorData::invalid_params("unknown tool", None)),
        };
        match self.actor.call(command).await {
            Ok(mut value) => {
                if request.name == "anki_get_authoring_context" {
                    value["permissions"] = json!({"read":true,"write_notes":principal.has_permission("anki:write_notes"),"maintenance":principal.has_permission("anki:maintenance")});
                }
                Ok(CallToolResult::structured(value).into())
            }
            Err(error) => Ok(CallToolResult::structured_error(json!({"error":error})).into()),
        }
    }
}

fn update_properties() -> serde_json::Value {
    json!({"note_id":{"type":"integer"},"expected_revision":{"type":"string","minLength":64,"maxLength":64},"field_patch":{"type":"object","additionalProperties":{"type":"string","maxLength":32768}},"tag_patch":{"type":"object","properties":{"add":{"type":"array","items":{"type":"string"}},"remove":{"type":"array","items":{"type":"string"}}},"additionalProperties":false}})
}

fn apply_properties() -> serde_json::Value {
    json!({"preview_token":{"type":"string","maxLength":128},"idempotency_key":{"type":"string","minLength":1,"maxLength":128},"confirmed":{"type":"boolean"}})
}

fn context_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"before":{"type":"string","maxLength":2048},"target":{"type":"string","maxLength":2048},"after":{"type":"string","maxLength":2048},"translation":{"type":"string","maxLength":2048}},"required":["before","target","after","translation"],"additionalProperties":false})
}

fn create_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"deck_id":{"type":"integer"},"notetype_id":{"type":"integer"},"fields":{"type":"object","additionalProperties":{"type":"string","maxLength":32768}},"tags":{"type":"array","items":{"type":"string"}},"external_dedupe_key":{"type":"string","minLength":1,"maxLength":128},"contexts":{"type":"object","properties":{"recognition":{"type":"array","maxItems":5,"items":context_schema()},"production":{"type":"array","maxItems":5,"items":context_schema()}},"required":["recognition","production"],"additionalProperties":false}},"required":["deck_id","notetype_id","fields","external_dedupe_key"],"additionalProperties":false})
}

fn metadata_tag(value: &str) -> Result<String, ErrorData> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '))
    {
        return Err(ErrorData::invalid_params(
            "metadata labels must contain bounded letters, digits, spaces, underscores or hyphens",
            None,
        ));
    }
    Ok(value.to_lowercase().replace(' ', "_"))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn template_properties() -> serde_json::Value {
    json!({"notetype_id":{"type":"integer"},"templates":{"type":"array","maxItems":100,"items":{"type":"object","properties":{"ordinal":{"type":"integer","minimum":0},"front":{"type":"string","maxLength":65536},"back":{"type":"string","maxLength":65536}},"required":["ordinal"],"additionalProperties":false}},"css":{"type":"string","maxLength":65536}})
}

fn projection_schema() -> serde_json::Value {
    json!({"type":"array","maxItems":64,"items":{"type":"string","minLength":1,"maxLength":128}})
}
