// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::collections::BTreeMap;

use anki::collection::Collection;
use anki::collection::CollectionBuilder;
use anki::notes::NoteId;
use anki::notetype::NoteField;
use anki::notetype::Notetype;
use anki::notetype::NotetypeId;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::writes::TagPatch;
use crate::writes::Update;

pub const FIELD: &str = "AlternateContexts";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextExample {
    before: String,
    target: String,
    after: String,
    translation: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrichment {
    pub note_id: i64,
    recognition: Vec<ContextExample>,
    production: Vec<ContextExample>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreationContexts {
    pub recognition: Vec<ContextExample>,
    pub production: Vec<ContextExample>,
}

pub fn supports_creation(nt: &Notetype) -> bool {
    eligible(nt) && nt.fields.iter().any(|field| field.name == FIELD)
}

pub fn creation(
    nt: &Notetype,
    fields: &BTreeMap<String, String>,
    input: &CreationContexts,
    key: &str,
) -> Result<String> {
    ensure!(
        supports_creation(nt),
        "context migration required before creation with contexts"
    );
    let fields = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Ok(html_escape(
        &payload(nt, &fields, &input.recognition, &input.production, key)?.to_string(),
    ))
}

fn payload(
    nt: &Notetype,
    fields: &BTreeMap<&str, &str>,
    recognition_examples: &[ContextExample],
    production_examples: &[ContextExample],
    key: &str,
) -> Result<Value> {
    ensure!(
        !recognition_examples.is_empty() || !production_examples.is_empty(),
        "supply candidate contexts"
    );
    let dict = nt.name == "Dict·Anki Language v2";
    let recognition = fields
        .get(if dict {
            "SentenceTarget"
        } else {
            "SelectedText"
        })
        .context("recognition target missing")?;
    let production = fields
        .get(if dict {
            "ProductionSentenceTarget"
        } else {
            "VocabularyAnswer"
        })
        .context("production target missing")?;
    validate(
        recognition_examples,
        &anki::text::strip_html_preserving_media_filenames(recognition),
    )?;
    validate(
        production_examples,
        &anki::text::strip_html_preserving_media_filenames(production),
    )?;
    let cue = fields
        .get("VocabularyHint")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| fields.get("Meaning"))
        .copied()
        .unwrap_or("");
    Ok(
        json!({"version":1,"key":key,"cue":anki::text::strip_html_preserving_media_filenames(cue),"recognition":recognition_examples,"production":production_examples}),
    )
}

fn eligible(nt: &Notetype) -> bool {
    matches!(
        nt.name.as_str(),
        "Dict·Anki Language v2" | "Readest Language v3"
    )
}

fn plain(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 2048
            && !value.contains(['<', '>'])
            && !value.contains("{{")
            && !value.chars().any(|c| c.is_control() && c != '\n'),
        "context must be bounded plain text without cloze/template markers"
    );
    Ok(())
}

fn validate(examples: &[ContextExample], target: &str) -> Result<()> {
    ensure!(examples.len() <= 5, "at most five contexts per direction");
    let mut seen = std::collections::HashSet::new();
    for example in examples {
        for value in [
            &example.before,
            &example.target,
            &example.after,
            &example.translation,
        ] {
            plain(value)?;
        }
        ensure!(
            !target.trim().is_empty() && example.target.trim() == target.trim(),
            "candidate must test the exact original target/form"
        );
        ensure!(
            !example.before.contains(target) && !example.after.contains(target),
            "target appears outside the tested slot"
        );
        let sentence = format!("{}{}{}", example.before, example.target, example.after);
        ensure!(seen.insert(sentence), "duplicate candidate context");
    }
    Ok(())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn enrichment(col: &mut Collection, input: Enrichment) -> Result<(Value, Option<Update>)> {
    let note = col
        .storage
        .get_note(NoteId(input.note_id))?
        .context("note missing")?;
    let nt = col
        .get_notetype(note.notetype_id)?
        .context("note type missing")?;
    ensure!(eligible(&nt), "note type is not selected for enrichment");
    let fields: BTreeMap<_, _> = nt
        .fields
        .iter()
        .zip(note.fields())
        .map(|(f, v)| (f.name.as_str(), v.as_str()))
        .collect();
    let payload = payload(
        &nt,
        &fields,
        &input.recognition,
        &input.production,
        &input.note_id.to_string(),
    )?;
    let encoded = html_escape(&payload.to_string());
    let migrated = fields.contains_key(FIELD);
    let update = migrated.then(|| Update {
        note_id: input.note_id,
        expected_revision: Some(crate::writes::revision(&note)),
        field_patch: BTreeMap::from([(FIELD.into(), encoded)]),
        tag_patch: TagPatch::default(),
    });
    Ok((
        json!({"note_id":input.note_id,"notetype_id":nt.id.0,"contexts":payload,"migration_required":!migrated,
        "original_quotation_fields_unchanged":true,"semantic_review_required":true}),
        update,
    ))
}

/// Fixed, versioned migration only: no arbitrary template code from tool input.
pub fn migrated(nt: &Notetype) -> Result<Notetype> {
    ensure!(eligible(nt), "note type is not selected for migration");
    ensure!(
        !nt.fields.iter().any(|f| f.name == FIELD),
        "context field already exists; inspect version before upgrading"
    );
    ensure!(
        nt.fields
            .iter()
            .enumerate()
            .all(|(ordinal, field)| field.ord == Some(ordinal as u32)),
        "reload canonical field ordinals before migration"
    );
    let mut after = nt.clone();
    after.fields.push(NoteField::new(FIELD));
    let dict = nt.name == "Dict·Anki Language v2";
    for template in &mut after.templates {
        for source in [&mut template.config.q_format, &mut template.config.a_format] {
            if dict {
                ensure!(
                    source.contains("JSON.parse(root.querySelector('.variant-data').textContent)"),
                    "unrecognized Dict template; refuse blind migration"
                );
                *source = source.replace("<div class='variant-data' hidden>", "<div class='alternate-data' hidden>{{AlternateContexts}}</div><div class='variant-data' hidden>")
                    .replace("var contexts = [primary].concat(JSON.parse(root.querySelector('.variant-data').textContent));",
                        "var raw = root.querySelector('.alternate-data').textContent, payload = {key: root.querySelector('.note-key').textContent}; var contexts = [primary]; if(raw.trim()){payload=JSON.parse(raw);if(payload.version!==1){throw new Error('Unsupported contexts');}contexts=contexts.concat(payload[root.dataset.direction === 'production' ? 'production' : 'recognition']);}else{var legacy=root.querySelector('.variant-data').textContent;contexts=contexts.concat(JSON.parse(legacy.trim()||'[]'));}")
                    .replace("var key = 'dict-anki-v2:' + root.querySelector('.note-key').textContent", "var key = 'dict-anki-v2:' + payload.key");
            } else {
                ensure!(
                    source.contains("<div class=\"context-data\">{{UsageNote}}</div>")
                        && source.contains("root.querySelector('.context-json').textContent"),
                    "unrecognized Readest template; refuse blind migration"
                );
                *source = source.replace("<div class=\"context-data\">{{UsageNote}}</div>","<div class=\"alternate-data\">{{AlternateContexts}}</div><div class=\"context-data\">{{UsageNote}}</div><span class=\"context-cue\">{{VocabularyHint}}</span><span class=\"context-key\">{{LookupId}}</span>")
                    .replace("var payload=JSON.parse(root.querySelector('.context-json').textContent);", "var raw=root.querySelector('.alternate-data').textContent,legacy=root.querySelector('.context-json'),payload={key:root.querySelector('.context-key').textContent,recognition:[],production:[]};if(raw.trim()){payload=JSON.parse(raw);if(payload.version!==1){throw new Error('Unsupported contexts');}}else if(legacy){payload=JSON.parse(legacy.textContent);}payload.cue=payload.cue||root.querySelector('.context-cue').textContent;");
            }
            // If no browser storage is available, use the original context.
            // Random front/back lifecycle still requires actual client acceptance.
            *source = source.replace("} else if (storage || memory) {", "} else if (storage) {")
                .replace("var previous = saved", "if(!storage){contexts=[primary];signature=JSON.stringify(contexts);saved=null;} var previous = saved")
                .replace("var previous=saved", "if(!storage){contexts=[primary];signature=JSON.stringify(contexts);saved=null;}var previous=saved")
                .replace("if(!reveal){index=", "if(!reveal&&storage){index=");
        }
    }
    Ok(after)
}

pub fn migration_preview(col: &mut Collection, id: i64) -> Result<Value> {
    let nt = col
        .get_notetype(NotetypeId(id))?
        .context("note type missing")?;
    let mut after = migrated(&nt)?;
    let (notes,cards): (u64,u64) = col.storage.db().query_row("select (select count(*) from notes where mid=?1),(select count(*) from cards where nid in (select id from notes where mid=?1))", [id], |row| Ok((row.get(0)?,row.get(1)?)))?;
    let templates: Vec<_> = nt.templates.iter().zip(&after.templates).map(|(before,after)| json!({"name":before.name,"front_before":before.config.q_format,"front_after":after.config.q_format,"back_before":before.config.a_format,"back_after":after.config.a_format})).collect();
    let card_ids = |col: &Collection| -> Result<Vec<i64>> {
        Ok(col
            .storage
            .db()
            .prepare(
                "select id from cards where nid in (select id from notes where mid=?) order by id",
            )?
            .query_map([id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    };
    let before_cards = card_ids(col)?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("migration-preview.anki2");
    col.storage
        .db()
        .execute("VACUUM INTO ?", [path.to_str().context("preview path")?])?;
    let mut clone = CollectionBuilder::new(&path).build()?;
    clone.update_notetype(&mut after, false)?;
    let ids = col.storage.get_all_note_ids()?;
    for id in ids {
        let before = col.storage.get_note(id)?.context("note missing")?;
        if before.notetype_id == nt.id {
            let migrated = clone
                .storage
                .get_note(id)?
                .context("note lost during migration")?;
            ensure!(
                migrated.fields().get(..before.fields().len()) == Some(before.fields().as_slice()),
                "migration changes original note fields"
            );
        }
    }
    let cards_changed = before_cards != card_ids(&clone)?;
    clone.close(None)?;
    let revision = blake3::hash(json!({"fields":nt.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),"templates":templates,"css":nt.config.css}).to_string().as_bytes()).to_hex().to_string();
    Ok(
        json!({"notetype_id":id,"name":nt.name,"migration_version":1,"expected_revision":revision,
        "added_fields":[FIELD],"removed_fields":[],"renamed_fields":[],"templates":templates,"css_before":nt.config.css,"css_after":after.config.css,
        "affected_notes":notes,"affected_cards":cards,"template_count_unchanged":true,"requires_full_sync":true,
        "card_generation_changed_in_clone":cards_changed,
        "apply_enabled":false,"requires_backup_and_maintenance":true,"device_rendering_verified":false,"device_test_recommended":true}),
    )
}

/// Export synthetic examples using cloned types, without altering source notes.
pub fn build_fixture(source: &mut Collection, output: &std::path::Path) -> Result<Value> {
    use anki::import_export::package::ExportAnkiPackageOptions;

    ensure!(
        !output.exists(),
        "refuse to replace an existing acceptance package"
    );
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("acceptance.anki2");
    let mut target = CollectionBuilder::new(&path)
        .with_desktop_media_paths()
        .build()?;
    let deck = target.get_or_create_normal_deck("Anki MCP · Context acceptance")?;
    let samples = json!([
        {"before":"Me gusta ","target":"comer","after":" con mis amigos.","translation":"I like eating with my friends."},
        {"before":"Vamos a ","target":"comer","after":" después del paseo.","translation":"We are going to eat after the walk."},
        {"before":"Es importante ","target":"comer","after":" despacio.","translation":"It is important to eat slowly."},
        {"before":"Prefiero ","target":"comer","after":" en casa.","translation":"I prefer eating at home."},
        {"before":"¿Quieres ","target":"comer","after":" conmigo?","translation":"Do you want to eat with me?"}
    ]);
    for name in ["Dict·Anki Language v2", "Readest Language v3"] {
        let original = source
            .get_notetype_by_name(name)?
            .context("selected source note type missing")?;
        let mut nt = migrated(&original)?;
        nt.id = NotetypeId(0);
        nt.name = format!("Anki MCP context test · {name}");
        target.add_notetype(&mut nt, false)?;
        let nt = target
            .get_notetype(nt.id)?
            .context("new fixture type missing")?;
        let mut note = nt.new_note();
        let values = BTreeMap::from([
            ("Sentence", "Hoy quiero comer algo rico."),
            ("SelectedText", "comer"),
            ("SentenceBefore", "Hoy quiero "),
            ("SentenceTarget", "comer"),
            ("SentenceAfter", " algo rico."),
            ("Meaning", "to eat"),
            ("Translation", "Today I want to eat something tasty."),
            ("ProductionSentenceBefore", "Hoy quiero "),
            ("ProductionSentenceTarget", "comer"),
            ("ProductionSentenceAfter", " algo rico."),
            (
                "ProductionTranslation",
                "Today I want to eat something tasty.",
            ),
            ("ProductionPrompt", "Hoy quiero ___ algo rico."),
            ("ProductionAnswer", "comer"),
            ("EnableProduction", "1"),
            ("EnableRecognition", "1"),
            ("EnableVocabulary", "1"),
            ("VocabularyPrompt", "Hoy quiero ___ algo rico."),
            ("VocabularyAnswer", "comer"),
            ("VocabularyHint", "to eat"),
            ("LearningTarget", "comer"),
            ("Lemma", "comer"),
            ("GrammaticalForm", "infinitive"),
            ("RecognitionContexts", "[]"),
            ("ProductionContexts", "[]"),
            ("NoteKey", "synthetic-mcp-fixture"),
            (
                "Source",
                "Synthetic example <img src=\"mcp-context-fixture.svg\">",
            ),
            (
                "SourceText",
                "Hoy quiero comer algo rico. <img src=\"mcp-context-fixture.svg\">",
            ),
            (
                "UsageNote",
                "Original usage note; must remain separate from alternate contexts.",
            ),
            ("Book", "Synthetic acceptance fixture"),
            ("Chapter", "1"),
        ]);
        for (ordinal, field) in nt.fields.iter().enumerate() {
            if let Some(value) = values.get(field.name.as_str()) {
                note.set_field(ordinal, *value)?;
            }
        }
        target.add_note(&mut note, deck.id)?;
        let ordinal = nt
            .fields
            .iter()
            .position(|field| field.name == FIELD)
            .context("fixture context field missing")?;
        note.set_field(ordinal,html_escape(&json!({"version":1,"key":note.id.0.to_string(),"cue":"to eat","recognition":samples,"production":samples}).to_string()))?;
        target.update_note(&mut note)?;
    }
    std::fs::write(path.with_extension("media").join("mcp-context-fixture.svg"),b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"120\" height=\"30\"><text x=\"5\" y=\"20\">MCP media OK</text></svg>")?;
    let exported = target.export_apkg(
        output,
        ExportAnkiPackageOptions {
            with_media: true,
            with_scheduling: false,
            with_deck_configs: true,
            legacy: false,
        },
        "",
        None,
    )?;
    let mut rendered = Vec::new();
    for id in target.search_cards("", anki::search::SortMode::NoOrder)? {
        let output = target.render_existing_card(id, false, false)?;
        rendered.push(
            json!({"id":id.0,"front":output.question(),"back":output.answer(),"css":output.css}),
        );
    }
    // Exercise existing legacy context fields with the new dedicated field
    // empty. The exported package remains the versioned fixture above.
    for card in &mut rendered {
        let id = anki::card::CardId(card["id"].as_i64().unwrap());
        let original_card = target
            .storage
            .get_card(id)?
            .context("fixture card missing")?;
        let mut note = target
            .storage
            .get_note(original_card.note_id())?
            .context("fixture note missing")?;
        let nt = target
            .get_notetype(note.notetype_id)?
            .context("fixture type missing")?;
        for (ordinal, field) in nt.fields.iter().enumerate() {
            match field.name.as_str() {
                FIELD => note.set_field(ordinal, "")?,
                "RecognitionContexts" | "ProductionContexts" => note.set_field(ordinal,html_escape(&samples.to_string()))?,
                "UsageNote" => note.set_field(ordinal,format!("Original usage note.<span class=\"context-json\" hidden>{}</span>",html_escape(&json!({"key":note.id.0.to_string(),"cue":"to eat","recognition":samples,"production":samples}).to_string())))?,
                _ => {}
            }
        }
        target.update_note(&mut note)?;
        let legacy = target.render_existing_card(id, false, false)?;
        card["legacy_front"] = json!(legacy.question());
        card["legacy_back"] = json!(legacy.answer());
    }
    std::fs::write(
        output.with_extension("rendered.json"),
        serde_json::to_vec_pretty(&rendered)?,
    )?;
    target.close(None)?;
    Ok(
        json!({"file":output,"notes":exported,"expected_cards":4,"synthetic_only":true,"original_types_and_notes_unchanged":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_preserve_target_form_and_cannot_inject_html_or_extra_clozes() {
        let candidate = |target: &str| ContextExample {
            before: "Ella quiere ".into(),
            target: target.into(),
            after: " mañana.".into(),
            translation: "She wants it tomorrow.".into(),
        };
        assert!(validate(&[candidate("comer")], "comer").is_ok());
        assert!(validate(&[candidate("comió")], "comer").is_err());
        assert!(validate(&[candidate("{{c2::comer}}")], "comer").is_err());
        assert!(validate(&[candidate("<img onerror=alert(1)>")], "comer").is_err());
        assert!(validate(&[candidate("comer"), candidate("comer")], "comer").is_err());
        assert!(html_escape("</div><script>bad</script>").contains("&lt;script&gt;"));
    }

    #[test]
    fn selected_real_template_migrations_preserve_note_fields_card_ids_and_reviews() -> Result<()> {
        use anki::decks::DeckId;
        use anki::notetype::CardTemplate;
        use anki::search::SortMode;

        for fixture in [
            include_str!("../tests/fixtures/dict-v2.json"),
            include_str!("../tests/fixtures/readest-v3.json"),
        ] {
            let fixture: Value = serde_json::from_str(fixture)?;
            let temp = tempfile::tempdir()?;
            let mut col = CollectionBuilder::new(temp.path().join("test.anki2")).build()?;
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
            for (ordinal, field) in nt.fields.iter().enumerate() {
                let value = if matches!(
                    field.name.as_str(),
                    "RecognitionContexts" | "ProductionContexts"
                ) {
                    ""
                } else {
                    "original quotation or value"
                };
                note.set_field(ordinal, value)?;
            }
            col.add_note(&mut note, DeckId(1))?;
            let original = note.fields().clone();
            let cards = col.search_cards(note.id, SortMode::NoOrder)?;
            assert!(!cards.is_empty());
            col.storage.db().execute(
                "insert into revlog values (1234567890000, ?1, -1, 3, 10, 5, 2500, 1234, 1)",
                [cards[0].0],
            )?;
            let preview = migration_preview(&mut col, nt.id.0)?;
            assert_eq!(preview["card_generation_changed_in_clone"], false);
            assert_eq!(col.storage.get_note(note.id)?.unwrap().fields(), &original);
            let mut migrated = migrated(&nt)?;
            assert!(migrated.templates.iter().all(|t| t
                .config
                .q_format
                .contains("{{AlternateContexts}}")
                && t.config.a_format.contains("{{AlternateContexts}}")));
            col.update_notetype(&mut migrated, false)?;
            let after = col.storage.get_note(note.id)?.unwrap();
            assert_eq!(&after.fields()[..original.len()], original.as_slice());
            assert_eq!(after.fields().last().unwrap(), "");
            assert_eq!(col.search_cards(note.id, SortMode::NoOrder)?, cards);
            let count: i64 = col.storage.db().query_row("select count(*) from revlog where id=1234567890000 and cid=? and ease=3 and ivl=10 and lastIvl=5 and factor=2500 and time=1234", [cards[0].0],|row|row.get(0))?;
            assert_eq!(count, 1);
            assert!(self::migrated(&migrated).is_err());
            let migrated = col.get_notetype(nt.id)?.unwrap();
            let fields = migrated
                .fields
                .iter()
                .zip(after.fields())
                .map(|(field, value)| (field.name.clone(), value.clone()))
                .collect();
            let contexts = || CreationContexts {
                recognition: vec![ContextExample {
                    before: "Test ".into(),
                    target: "original quotation or value".into(),
                    after: ".".into(),
                    translation: "Test translation.".into(),
                }],
                production: vec![],
            };
            let mut input = crate::writes::Create {
                deck_id: 1,
                notetype_id: nt.id.0,
                fields,
                tags: vec![],
                external_dedupe_key: "context-creation".into(),
                contexts: Some(contexts()),
            };
            let journal = crate::writes::Journal::open(&temp.path().join("journal.sqlite"))?;
            input.contexts.as_mut().unwrap().recognition[0].target = "wrong target".into();
            let error = journal
                .preview_create(&mut col, "owner", vec![input.clone()])
                .err()
                .unwrap();
            assert_eq!(
                crate::failure::Failure::from_error(&error).code,
                "invalid_contexts"
            );
            input.contexts = Some(contexts());
            let preview = journal.preview_create(&mut col, "owner", vec![input])?;
            assert_eq!(preview["notes"][0]["semantic_review_required"], true);
            assert!(preview["notes"][0]["fields"][FIELD]
                .as_str()
                .unwrap()
                .contains("context-creation"));
            let token = preview["preview_token"].as_str().unwrap();
            let mut receipt = journal.prepare_update(
                &mut col,
                "owner",
                "create-batch",
                token,
                journal.load_preview("owner", token)?,
            )?;
            journal.commit_local(&mut col, "owner", "create-batch", &mut receipt)?;
            let created = col.storage.get_note(NoteId(receipt.note_ids[0]))?.unwrap();
            assert_eq!(&created.fields()[..original.len()], original.as_slice());
            assert!(!created.fields().last().unwrap().is_empty());
        }
        Ok(())
    }
}
