# Anki on Jupiter

Run `just anki-deploy` from this checkout. It runs the full project check, Rust dependency security audit and
integration/authentication tests, builds images locally, transfers a single
Docker archive, and applies Ansible. It validates the combined existing Caddy
configuration and restores the prior Anki Compose/site fragment on failure.
The deploy script holds the existing backup job's remote lock throughout Ansible,
so a rollout cannot overlap the coordinated snapshot or offsite replication.
The external `caddy-edge` network and the other applications remain owned by
their existing deployments. No Anki backend port is published on the host.

Endpoints:

- Native sync: `https://api.jupiter.timdumol.com/`
- MCP Streamable HTTP: `https://mcp.jupiter.timdumol.com/mcp`
- Pocket ID: `https://auth.aries.timdumol.com`

ChatGPT custom MCP settings: name `Anki`, server URL as above, authentication
`OAuth`, client ID `anki-chatgpt`, blank client secret. This is a registered public
PKCE client. The existing ChatGPT connection requests `anki:read` and
`anki:sync`. Pocket ID also allows `anki:write_notes` and `anki:maintenance`;
requesting those separately requires the plugin configuration update below. The issuer
advertises RFC 9207 support and the registered callback is
`https://chatgpt.com/connector_platform_oauth_redirect`. If ChatGPT displays a
different callback, register that exact callback before completing login.

Synchronization is automatic; there is no manual sync tool. Content reads and
previews synchronize before returning, and writes synchronize before and after
committing. Content reads fail if refresh fails instead of returning stale data.
Status and operation-receipt polling retry synchronization but remain readable
when it fails; inspect `last_sync_failed` or the receipt's sync state. Maintenance
continues to block ordinary collection operations. Temporarily, the legacy
`anki:sync` scope also authorizes note writes and
maintenance, so existing ChatGPT grants can use those operations. Explicit
confirmation, previews, backups and full-download requirements still apply.

TODO: Update the ChatGPT plugin OAuth configuration to request
`anki:write_notes` and `anki:maintenance`, reauthorize the connection, verify its
granted permissions, then remove the `anki:sync` compatibility rule.

The MCP owns `/opt/anki/data/mcp/collection.anki2`, separate from the sync volume.
It mirrors the everyday collection uploaded by the user, in account `timdumol`.
This account was renamed from `fixture` without changing its password or data.
Native credentials are root-only in `/opt/anki/secrets/sync.env`; the
MCP receives a separate read-only password file. Never put either native
credentials or Pocket ID's administrative API key into ChatGPT settings.

Read tools include status, search, get note, list decks, get note type,
note/bulk update previews, operation receipts, migration preview and context
enrichment preview. Guarded note/bulk/context writes, creation from corrections,
weakness search and maintenance-scoped template migration are available with
`ANKI_WRITES_ENABLED=1`. All mutations require explicit confirmation. Note writes
require `anki:write_notes`; schema migrations require `anki:maintenance`. Pocket
ID grants both to the existing owner-only ChatGPT client.

Desktop: Tools → Preferences → Syncing → Custom sync server:
`https://api.jupiter.timdumol.com/`. Log in as `timdumol` with the unchanged
password. Retrieve it privately if needed:

```sh
ssh jupiter.timdumol.com "sudo -n sed -n 's/^SYNC_USER2=timdumol://p' /opt/anki/secrets/sync.env"
```

The server already contains the uploaded everyday collection. AnkiDroid should
use the same endpoint/account and download that authoritative copy after backing
up any device state. The user reported no Android edits/reviews since Desktop
switched. Do not continue reviewing against AnkiWeb after this change.

## Authoring workflow

Call `anki_get_authoring_context` once to discover decks, every note type
(including unused types), field names/ordinals and template names. Filter decks
using `can_create_notes`. Capabilities report limits and server write availability;
`permissions` reports the authenticated client's granted scopes. Both enabled
writes and the appropriate permission are required to apply changes. Content
reads report `last_sync_unix` after automatic synchronization.

For updates, use `anki_search_notes` with optional `fields`, or
`anki_get_notes` with 1–100 `note_ids` and optional `fields`. Omitting `fields` in
batch retrieval returns all fields; search keeps its compact first-field preview
by default. Supplying `fields: []` returns metadata only. Projections retain the
whole-note `revision`, tags and card IDs and report requested `missing_fields`.
A deleted note in batch retrieval produces an item error instead of failing the
entire batch. Search results always include a revision, even without projection.

Pass that revision as `expected_revision` to `anki_preview_note_update`, or to
each item in `anki_preview_bulk_update`, to detect changes since the original
read. `field_patch` may be omitted for tag-only edits. For example:

```json
{
    "note_id": 123,
    "expected_revision": "<revision from the read>",
    "tag_patch": { "add": ["reviewed"], "remove": ["draft"] }
}
```

Review the returned diff and apply using the existing preview token flow. Apply
also checks for changes made after preview, including note-type schema changes.
Content updates cannot change the generated card IDs; cloze edits that add cards
require maintenance.

For creation, call `anki_preview_notes_creation` with `notes`, an array of 1–20
objects containing `deck_id`, `notetype_id`, every named `fields` entry, optional
`tags`, and a distinct `external_dedupe_key`. A single private clone validates the
entire batch and returns sanitized, normalized `fields` and `tags`, generated
`card_count`, and advisory `duplicate_candidates` for each item. Candidates match
the same note type and exact first-field text with HTML stripped. They include
existing `note_id` values and earlier `batch_index` values (zero-based); at most
ten candidates are returned per item, alongside the full `duplicate_count`.
This is content matching, separate from durable retry deduplication.

After review, call `anki_apply_notes_creation` with `preview_token`, a stable
batch `idempotency_key`, and `confirmed: true`. The token expires after 15 minutes
and is bound to the authenticated user and exact planned content. Apply rejects
changed schemas and reused creation keys before committing. Retry an ambiguous
apply using the same token/key, or poll `anki_get_operation`. Per-note progress
survives restarts; bulk creation is not atomic across synchronization. The legacy
single-note `anki_create_notes` interface remains available for existing clients.

Supported, migrated language note types also accept `contexts` during creation:
`{"recognition": [...], "production": [...]}`. Each candidate has `before`,
`target`, `after` and `translation`; each direction allows at most five candidates.
Leave dedicated context fields empty in `fields`. The same exact-target and
plain-text validation used by enrichment runs before creation, so no second
write is needed. Review linguistic equivalence when the preview reports
`semantic_review_required`.

Tool results include MCP `structuredContent` and matching JSON text for older
clients. Tool execution errors have an `error` object with `code`, `message`,
`retryable` and optional `details`. Examples include `stale_revision`,
`stale_preview`, `stale_schema`, `invalid_fields`, `creation_key_conflict` and
`sync_unavailable`; upstream error bodies are never returned. Successful writes
and receipt polling include current note revisions/card IDs, `completed_notes`,
`last_sync_failed` and explicit `sync_pending`. A local commit is only reported as
`synced_successfully` after synchronization succeeds.

## Guarded writes and context acceptance

Previews synchronize first and return a principal-bound token valid for 15
minutes. Updates compare the note's complete field/tag revision and template
schema after another sync. A private clone rejects changes that generate extra
cards. The separate SQLite journal uses WAL and synchronous FULL; receipts retain
before/after fields, card IDs, operation digest and per-note progress. Restart
reconciliation checks already-committed content before any retry. New writes
cannot bypass unresolved operations. Batches contain at most 20 distinct notes;
partial progress is recoverable, but not atomic across synchronization.

Creates use a deterministic GUID bound to the principal and external dedupe key.
Retry keys with different content are rejected. Rich field patches are sanitized;
no SQL, arbitrary execution, review answering, deletion or scheduling mutation
tool is exposed. A local commit reports `saved_locally_sync_pending` until sync
really succeeds. `anki_get_operation` exposes the durable receipt/rollback data.

The deployed mirror has writes enabled and full-download fallback disabled. A
server-requested full sync stops ordinary MCP synchronization and requires
maintenance. Ordinary note operations never perform a full upload; only the
explicitly confirmed, host-coordinated template migration command can upload a
full collection.

Chosen enrichment targets: Dict·Anki Language v2 and Readest Language v3.
Version 1 adds `AlternateContexts`, holding HTML-escaped versioned JSON with
recognition/production arrays. Candidate contexts must use the exact original
target/form, contain no HTML or extra cloze markers, and pass linguistic review.
The original quotation/source/usage fields remain unchanged. Dict's old context
arrays remain available when the new field is empty. Readest reads the dedicated field first and preserves embedded UsageNote
contexts as a legacy fallback. Applying schema changes
uses the backed-up maintenance workflow below. The user explicitly skipped the
manual Desktop/AnkiDroid acceptance exercise; rendering on those applications
remains unverified. Browser lifecycle tests passed.

`just anki-context-fixture DISPOSABLE_COLLECTION OUTPUT.apkg` builds only two
synthetic notes/four cards in separately named cloned types and a test deck; it
does not modify source notes/types. The generated
`artifacts/anki-context-acceptance.apkg` remains available for optional future
Desktop/Android acceptance. Across repeated presentations, verify different
contexts with the same context on both sides, including offline/restart and media.
The accompanying Rust-rendered JSON can be checked with
`just anki-context-browser-test OUTPUT.rendered.json`; set
`ANKI_CONTEXT_CHROMIUM` if using an existing cached Chromium executable. Browser
tests cover same-page flips, separate-page flips and unavailable storage, but
do not replace tests in the actual Desktop/AnkiDroid applications.

## Template maintenance

Review `anki_preview_notetype_migration` for one selected type, then call
`anki_apply_notetype_migration` with its exact `expected_revision`, a unique
`idempotency_key` and `confirmed: true`. The tool queues a durable request; it
does not report success before completion. `anki_get_operation` returns the
request state, reviewed manifest and rollback backup path. The host's
`anki-maintenance.timer` checks requests every minute. Normal writes stop as soon
as a migration is queued. During execution only the two Anki routes return 503;
the other Caddy services remain available.

The coordinator holds the existing backup lock, pauses public Anki traffic, stops
both Anki daemons and copies collection/media/journal/secrets/Compose into a
private `/opt/anki/rollback/migration-*` directory. It restarts only native sync,
then runs the fixed migration command in an isolated MCP container using the
internal native endpoint. The command synchronizes, rechecks the reviewed schema,
applies the migration through Anki's SDK, checks original note fields and all
card/scheduling/review data, uploads, and verifies a fresh native-server download.
Public traffic and MCP resume only after successful verification. Each device
subsequently needs a full **DOWNLOAD** from Jupiter; first sync any outstanding
device changes before confirming a migration.

MCP synchronizes over the private Docker endpoint, including while public routes
are paused. The coordinator waits for both containers to be healthy before
restoring public routes; failed startup leaves the maintenance 503 in place.

If an error occurs after the irreversible phase starts, the manifest records
`reconciliation_required`; public Anki stays paused and no upload automatically
retries. Inspect `journalctl -u anki-maintenance.service`, the JSON manifest and
the recorded backup. Compare server and sidecar collections before deciding to
finish or restore. To restore, stop both Anki services under the backup job lock,
preserve the failed state separately, replace both data directories together from
the recorded consistent backup, restore its Caddy fragment and restart both
services. Restored requests are still marked `requested`: mark that restored
request `not_applied` before re-enabling the timer, or it will request maintenance
again. Do not overwrite live collections during a diagnostic restore drill.

Corrections require a complete reviewed note payload and `confirmed: true`. They
retain the observed error and corrected form in `UsageNote`, `Extra` or `Back`,
with searchable `anki-mcp::correction`, lemma and weakness-category tags. They
use the same durable creation dedupe key as ordinary note creation.

## Backups and recovery

The existing `aries-server-backup` job holds its existing job lock and invokes
`/opt/anki/backup-snapshot.sh`. The helper stops the running Anki containers,
copies collection/media/MCP state/secrets/deployment metadata together, and
restarts exactly the previously running containers. Its consistent copy joins
the existing encrypted restic snapshot. Expect a brief Anki-only interruption.
The existing `presto-backup` job copies the shared repository offsite when its
Drive repository is configured. A successful local backup does not prove
successful offsite replication.

For recovery, restore the `server-staging/anki` subtree into a private disposable
directory using restic's `--verify`. Never overwrite live volumes during a drill.
Start an isolated server with the restored sync data and credentials, bind its
port to loopback, and use `just anki-spike --bootstrap ...` to download into a new
client collection. `just anki-inspect-collection PATH` opens a disposable restored
collection through Anki's Rust APIs with integrity checking. Include any SQLite WAL files when restoring a collection; copying only the main
SQLite file can miss recent committed changes. Compare note/card/
review counts and media checksums, and repeat the drill for MCP state.

## Verified baseline

The user confirmed a full Desktop restore of `anki-premigrate.colpkg` including
scheduling and media. A separate Rust import recovered 665 notes, 699 cards and
2,003 review records. Export SHA256:
`7cabda376d77d72ce531ef5ef90ca0565af17c4615fa3f23c5f536e8911b1e82`.
The export and API key are excluded from Git and Docker contexts.

## Editing existing templates

`anki_preview_template_update` accepts `notetype_id`, optional `css`, and
`templates` patches with a zero-based `ordinal` and optional `front`/`back`.
Omitted source stays unchanged. Read ordinals/source with `anki_get_notetype`.
This edits existing card templates and note-type styling; it does not add,
remove or reorder templates or fields. Source is preserved, including template
JavaScript. Each source is limited to 64 KiB and the patch to 256 KiB.

Preview synchronizes automatically, validates Anki template syntax in a
private collection clone, and rejects changes to cards, scheduling, reviews or
original note content. Review the returned source diffs and affected counts.
Call `anki_apply_template_update` with the same patch in `update`, the exact
`expected_revision`, a unique `idempotency_key`, and `confirmed: true`.
Apply requires `anki:maintenance` and enabled writes; revisions bind both the
current note type and the proposed patch. Poll `anki_get_operation` with the key.

Apply uses the existing host maintenance coordinator: public sync is paused,
both volumes are backed up, the edit is applied and uploaded, and a fresh server
download verifies templates and preserved history before sync reopens. This
conservative workflow requires a full **DOWNLOAD** on other devices afterward.
Deploying the tools alone does not edit templates or request maintenance.
Refresh connector tool discovery after deployment to expose the two new tools.

Both server images build the official sync implementation from this workspace
and its security-patched `Cargo.lock`. The `anki-sync:dd324c8` tag identifies the
upstream source baseline; dependencies include the local security updates.
See [SECURITY-REVIEW.md](SECURITY-REVIEW.md) for the October 2026 findings and
remaining desktop/development dependency advisories.
