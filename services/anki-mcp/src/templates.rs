// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use anki::collection::Collection;
use anki::collection::CollectionBuilder;
use anki::notetype::Notetype;
use anki::notetype::NotetypeId;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub notetype_id: i64,
    #[serde(default)]
    pub templates: Vec<Template>,
    pub css: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub ordinal: usize,
    pub front: Option<String>,
    pub back: Option<String>,
}

pub fn updated(nt: &Notetype, input: &Update) -> Result<Notetype> {
    ensure!(
        serde_json::to_vec(input)?.len() <= 256 * 1024,
        "template update exceeds 256 KiB"
    );
    ensure!(nt.id.0 == input.notetype_id, "note type mismatch");
    ensure!(
        !input.templates.is_empty() || input.css.is_some(),
        "empty template update"
    );
    ensure!(
        input.templates.len() <= nt.templates.len() && input.templates.len() <= 100,
        "too many template patches"
    );
    let mut seen = std::collections::HashSet::new();
    let mut after = nt.clone();
    for patch in &input.templates {
        ensure!(seen.insert(patch.ordinal), "duplicate template ordinal");
        ensure!(
            patch.front.is_some() || patch.back.is_some(),
            "empty template patch"
        );
        let template = after
            .templates
            .get_mut(patch.ordinal)
            .context("unknown template ordinal")?;
        for (value, target) in [
            (&patch.front, &mut template.config.q_format),
            (&patch.back, &mut template.config.a_format),
        ] {
            if let Some(value) = value {
                ensure!(value.len() <= 65536, "template source exceeds 64 KiB");
                target.clone_from(value);
            }
        }
    }
    if let Some(css) = &input.css {
        ensure!(css.len() <= 65536, "CSS exceeds 64 KiB");
        after.config.css.clone_from(css);
    }
    Ok(after)
}

pub fn preview(col: &mut Collection, input: &Update) -> Result<Value> {
    let nt = col
        .get_notetype(NotetypeId(input.notetype_id))?
        .context("note type missing")?;
    let mut after = updated(&nt, input)?;
    let notes = crate::maintenance::original_notes(col)?;
    let history = crate::maintenance::history(col)?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("template-preview.anki2");
    col.storage
        .db()
        .execute("VACUUM INTO ?", [path.to_str().context("preview path")?])?;
    let mut clone = CollectionBuilder::new(&path).build()?;
    clone.update_notetype(&mut after, false)?;
    crate::maintenance::verify(&clone, &notes, &history)?;
    clone.close(None)?;
    let revision = blake3::hash(
        json!({"before":format!("{nt:?}"), "update":input})
            .to_string()
            .as_bytes(),
    )
    .to_hex()
    .to_string();
    let templates: Vec<_> = nt.templates.iter().zip(&after.templates).enumerate().map(|(ordinal,(before,after))| json!({"ordinal":ordinal,"name":before.name,"front_before":before.config.q_format,"front_after":after.config.q_format,"back_before":before.config.a_format,"back_after":after.config.a_format})).collect();
    let (notes,cards): (u64,u64) = col.storage.db().query_row("select (select count(*) from notes where mid=?1),(select count(*) from cards where nid in (select id from notes where mid=?1))", [input.notetype_id], |row| Ok((row.get(0)?,row.get(1)?)))?;
    Ok(
        json!({"notetype_id":input.notetype_id,"expected_revision":revision,"templates":templates,"css_before":nt.config.css,"css_after":after.config.css,"affected_notes":notes,"affected_cards":cards,"card_generation_changed_in_clone":false,"requires_full_sync":true,"requires_backup_and_maintenance":true,"device_instruction":"After completion, full DOWNLOAD on other devices.","apply_enabled":false}),
    )
}

#[cfg(test)]
mod tests {
    use anki::decks::DeckId;

    use super::*;

    #[test]
    fn preview_preserves_collection_and_binds_revision_to_source_and_patch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut col = CollectionBuilder::new(temp.path().join("collection.anki2")).build()?;
        let nt = col.get_notetype_by_name("Basic")?.unwrap();
        let mut note = nt.new_note();
        note.set_field(0, "question")?;
        col.add_note(&mut note, DeckId(1))?;
        let history = crate::maintenance::history(&col)?;
        let input = Update {
            notetype_id: nt.id.0,
            templates: vec![Template {
                ordinal: 0,
                front: Some("<main>{{Front}}</main>".into()),
                back: None,
            }],
            css: Some(".card { color: navy; }".into()),
        };
        let first = preview(&mut col, &input)?;
        assert_eq!(first["affected_notes"], 1);
        assert_eq!(first["affected_cards"], 1);
        assert_eq!(
            first["templates"][0]["front_after"],
            "<main>{{Front}}</main>"
        );
        assert_eq!(col.get_notetype(nt.id)?.unwrap().config.css, nt.config.css);
        assert_eq!(col.get_notetype(nt.id)?.unwrap().templates, nt.templates);
        assert_eq!(crate::maintenance::history(&col)?, history);
        assert_eq!(
            first["expected_revision"],
            preview(&mut col, &input)?["expected_revision"]
        );
        let different = Update {
            css: Some("".into()),
            ..input.clone()
        };
        assert_ne!(
            first["expected_revision"],
            preview(&mut col, &different)?["expected_revision"]
        );
        let mut changed = nt.as_ref().clone();
        changed.config.css.push_str("\n.card { color: red; }");
        col.update_notetype(&mut changed, false)?;
        assert_ne!(
            first["expected_revision"],
            preview(&mut col, &input)?["expected_revision"]
        );
        Ok(())
    }

    #[test]
    fn preview_rejects_invalid_patches_without_changing_templates() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut col = CollectionBuilder::new(temp.path().join("collection.anki2")).build()?;
        let nt = col.get_notetype_by_name("Basic")?.unwrap();
        let patch = Template {
            ordinal: 0,
            front: Some("{{MissingField}}".into()),
            back: None,
        };
        let invalid = [
            Update {
                notetype_id: nt.id.0,
                templates: vec![],
                css: None,
            },
            Update {
                notetype_id: nt.id.0,
                templates: vec![Template {
                    ordinal: 99,
                    ..patch.clone()
                }],
                css: None,
            },
            Update {
                notetype_id: nt.id.0,
                templates: vec![patch.clone(), patch.clone()],
                css: None,
            },
            Update {
                notetype_id: nt.id.0,
                templates: vec![patch],
                css: None,
            },
            Update {
                notetype_id: nt.id.0,
                templates: vec![],
                css: Some("x".repeat(65537)),
            },
        ];
        for input in invalid {
            assert!(preview(&mut col, &input).is_err(), "accepted {input:?}");
        }
        assert_eq!(col.get_notetype(nt.id)?.unwrap().templates, nt.templates);
        assert_eq!(col.get_notetype(nt.id)?.unwrap().config.css, nt.config.css);
        Ok(())
    }
}
