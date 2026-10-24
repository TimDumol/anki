// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use anki::collection::Collection;
use anki::notes::NoteId;
use anki::search::SortMode;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use crate::failure::Failure;
use crate::writes;

pub fn validate_projection(fields: &Option<Vec<String>>) -> Result<()> {
    if fields.as_ref().is_some_and(|names| {
        names.len() > 64 || names.iter().any(|n| n.is_empty() || n.len() > 128)
    }) {
        return Err(Failure::new(
            "invalid_projection",
            "Request at most 64 bounded field names.",
            false,
        )
        .into());
    }
    Ok(())
}

pub fn note(col: &mut Collection, id: i64, selected: Option<&[String]>) -> Result<Value> {
    let note = col.storage.get_note(NoteId(id))?.ok_or_else(|| {
        Failure::new(
            "note_missing",
            "Note no longer exists; search again.",
            false,
        )
        .details(json!({"note_id":id}))
    })?;
    let nt = col.get_notetype(note.notetype_id)?.ok_or_else(|| {
        Failure::new(
            "notetype_missing",
            "Note type no longer exists; refresh authoring context.",
            false,
        )
    })?;
    let fields: Vec<_> = nt
        .fields
        .iter()
        .enumerate()
        .zip(note.fields())
        .filter(|((_, field), _)| selected.is_none_or(|names| names.contains(&field.name)))
        .map(|((ordinal, field), value)| json!({"ordinal":ordinal,"name":field.name,"value":value}))
        .collect();
    let missing: Vec<_> = selected
        .unwrap_or_default()
        .iter()
        .filter(|name| !nt.fields.iter().any(|f| &f.name == *name))
        .collect();
    let cards = col.search_cards(note.id, SortMode::NoOrder)?;
    Ok(
        json!({"id":id,"notetype_id":nt.id.0,"notetype_name":nt.name,"fields":fields,
        "missing_fields":missing,"tags":note.tags,"card_ids":cards.iter().map(|id|id.0).collect::<Vec<_>>(),"revision":writes::revision(&note)}),
    )
}

pub fn notes(col: &mut Collection, ids: &[i64], fields: &Option<Vec<String>>) -> Result<Value> {
    validate_projection(fields)?;
    if ids.is_empty() || ids.len() > 100 {
        return Err(Failure::new(
            "invalid_batch",
            "Request between 1 and 100 note IDs.",
            false,
        )
        .into());
    }
    // Missing notes are reported individually so one deletion does not discard a
    // batch.
    let mut results = Vec::new();
    for id in ids {
        match note(col, *id, fields.as_deref()) {
            Ok(value) => results.push(value),
            Err(error)
                if error
                    .downcast_ref::<Failure>()
                    .is_some_and(|e| e.code == "note_missing") =>
            {
                results.push(json!({"id":id,"error":Failure::from_error(&error)}))
            }
            Err(error) => return Err(error),
        }
    }
    Ok(json!({"notes":results}))
}

pub fn context(col: &mut Collection, writes_enabled: bool) -> Result<Value> {
    let mut decks = Vec::new();
    for (id, name) in col.get_all_deck_names(false)? {
        let normal = col
            .get_deck(id)?
            .is_some_and(|deck| matches!(deck.kind, anki::decks::DeckKind::Normal(_)));
        decks.push(json!({"id":id.0,"name":name,"can_create_notes":normal}));
    }
    let mut types = col.get_all_notetypes()?;
    types.sort_by_key(|nt| (nt.name.clone(), nt.id.0));
    let types: Vec<_> = types
        .into_iter()
        .map(|nt| {
            let fields: Vec<_> = nt
                .fields
                .iter()
                .enumerate()
                .map(|(ordinal, f)| json!({"ordinal":ordinal,"name":f.name}))
                .collect();
            let templates: Vec<_> = nt
                .templates
                .iter()
                .enumerate()
                .map(|(ordinal, t)| json!({"ordinal":ordinal,"name":t.name}))
                .collect();
            json!({"id":nt.id.0,"name":nt.name,"fields":fields,"templates":templates,
            "supports_contexts":crate::contexts::supports_creation(&nt)})
        })
        .collect();
    Ok(
        json!({"decks":decks,"notetypes":types,"capabilities":{"writes_enabled":writes_enabled,
        "automatic_sync":true,"max_read_batch":100,"max_write_batch":20,"creation_preview":true,
        "revision_checks":true,"tag_only_updates":true,"creation_contexts":true,
        "update_card_generation_changes":false}}),
    )
}

#[cfg(test)]
mod tests {
    use anki::collection::CollectionBuilder;
    use anki::decks::DeckId;

    use super::*;

    #[test]
    fn authoring_context_discovers_unused_types_and_field_schemas() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut col = CollectionBuilder::new(temp.path().join("test.anki2")).build()?;
        let result = context(&mut col, false)?;
        let types = result["notetypes"].as_array().unwrap();
        let basic = types.iter().find(|nt| nt["name"] == "Basic").unwrap();
        assert_eq!(basic["fields"][0]["name"], "Front");
        assert_eq!(basic["fields"][1]["ordinal"], 1);
        assert_eq!(result["decks"][0]["id"], 1);
        assert_eq!(result["decks"][0]["can_create_notes"], true);
        assert_eq!(result["capabilities"]["writes_enabled"], false);
        Ok(())
    }

    #[test]
    fn projected_batches_preserve_revisions_and_report_missing_notes_and_fields() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut col = CollectionBuilder::new(temp.path().join("test.anki2")).build()?;
        let nt = col.get_notetype_by_name("Basic")?.unwrap();
        let mut original = nt.new_note();
        original.set_field(0, "question")?;
        original.set_field(1, "answer")?;
        col.add_note(&mut original, DeckId(1))?;
        let selected = Some(vec!["Back".into(), "unknown".into()]);
        let result = notes(&mut col, &[original.id.0, -1], &selected)?;
        assert_eq!(
            result["notes"][0]["fields"],
            json!([{"ordinal":1,"name":"Back","value":"answer"}])
        );
        assert_eq!(result["notes"][0]["missing_fields"], json!(["unknown"]));
        let full = note(&mut col, original.id.0, None)?;
        assert_eq!(result["notes"][0]["revision"], full["revision"]);
        assert_eq!(result["notes"][0]["card_ids"], full["card_ids"]);
        assert_eq!(result["notes"][1]["error"]["code"], "note_missing");
        let metadata = note(&mut col, original.id.0, Some(&[]))?;
        assert_eq!(metadata["fields"], json!([]));
        assert_eq!(metadata["revision"], full["revision"]);
        assert!(notes(&mut col, &[], &None).is_err());
        assert!(notes(&mut col, &vec![original.id.0; 101], &None).is_err());
        Ok(())
    }
}
