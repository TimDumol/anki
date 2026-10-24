# Execution status — 2026-10-13

The user switched their everyday Desktop profile to Jupiter and uploaded the
collection. The populated native account is now `timdumol` (renamed from
`fixture`, with the same password). AnkiDroid has no subsequent edits/reviews;
its endpoint change and actual device rendering are not confirmed. The user
explicitly skipped the manual Desktop/AnkiDroid context acceptance exercise.

| Gate | Evidence and remaining acceptance                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ---- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| G0   | User verified the full `.colpkg` restore with scheduling/media. Rust baseline: 665 notes, 699 cards, 2,003 reviews, 6 decks, 18 note types, 4,511 export media files.                                                                                                                                                                                                                                                                                                                                                       |
| G1   | Pinned official native server deployed; user confirms everyday Desktop sync/upload works. Native/Rust fixture round trips preserve reviews and media. Actual AnkiDroid round trip remains unverified.                                                                                                                                                                                                                                                                                                                       |
| G2   | Separate MCP collection matches the uploaded native collection: all note fields/tags/IDs, card IDs/scheduling, review history and 4,512 current media filenames/checksums. Download-only bootstrap and guarded sync pass.                                                                                                                                                                                                                                                                                                   |
| G3   | User confirms ChatGPT discovery and `anki_status` work. Issuer/audience/expiry/owner/signature/scope and public-host regression tests pass. Existing PKCE client now has read, sync, write_notes and maintenance grants; re-authorize to obtain new permissions.                                                                                                                                                                                                                                                            |
| G4   | Guarded previews/update/bulk/create, durable SQLite receipts, stale conflicts, dedupe, per-note partial progress, crash reconciliation and outage/restart recovery are implemented and tested. Deployed with writes enabled; real connected ChatGPT status and sync calls succeed with zero pending operations.                                                                                                                                                                                                             |
| G5   | Selected Dict v2 / Readest v3 schemas inventoried. Fixed migration adds AlternateContexts; private-clone checks preserve original fields and cards. Synthetic browser tests pass for four cards/six contexts, same-page and separate-page flips, and unavailable-storage fallback. Maintenance apply, context apply, correction creation and weakness search are implemented. Production migrations await explicit confirmation; actual client rendering remains unverified and its manual exercise was waived by the user. |
| G6   | Everyday Desktop already moved by the user. Account rename and server/sidecar parity are verified. Android configuration remains a client-side action.                                                                                                                                                                                                                                                                                                                                                                      |

Migration previews affect Dict: 30 notes / 44 cards; Readest: 92 notes / 108 cards.
No production note fields or templates have been changed by this implementation.

The native and MCP collection/media were backed up consistently and replicated
to the user-approved `rclone:presto-drive:presto-backup` destination. Offsite
snapshot `4b62beee` (20:18:53 UTC) contains the populated collection and renamed
account. Its Anki subtree restored with file verification: 9,040 files, 75.18 MiB.
Every native and MCP database/media file matched the consistent staging copy.
An isolated loopback-only server from the restored data served a fresh Rust
download with 665 notes and all 4,512 media checksums matching live state. Drill
containers, tunnel and temporary credential files were removed.

Native and MCP semantic fingerprints before template changes:

- Notes: `6dd29c265e0f821c1d6ab46dae11d65fe626e573563646e316c2d6c84d3cb224`
- Cards/scheduling: `18c701af827f9fa7ba3a70ab1c7efd5ec058ea738e45f62874bfd5bd36be8d56`
- Reviews: `24a3f2a60889e38c502d9f773b1c9e19f89dd18288d11aa72b8e903506577def`
- Media manifest: `24a82c79e31beefb21936ba5cae048cffdd6ff63a5130be47d71f8e43ff0b8e8`

Production-size steady-state memory observed: native 9.359 MiB, MCP 7.133 MiB,
with 768 MiB caps. Full-sync peak profiling and actual device recovery remain
unverified. The existing five Caddy services are checked during every deployment.
See README.md for connector settings, client sync, maintenance and recovery.

The guarded tool deployment completed on 2026-10-13: Ansible 32 successful tasks,
zero failures, both Anki services healthy, all five existing Caddy services healthy,
and the maintenance timer active. Live status after deployment: 667 notes, 701
cards, 2,016 reviews, 18 note types; these include the user's intervening additions
and reviews. Source templates are unchanged and migrations remain unrequested.

Validation passed: `just check`, scoped Clippy/format checks, 13 MCP Rust tests
and the native-client test. Browser coverage additionally verifies legacy context
fallback, production cues and correct answers. The 4-card fixture is available
under ignored artifacts; the device exercise remains explicitly waived.

Live immutable images:

- Native: `sha256:eb91f08364ab28ca67c81621f54bbfeede786def7d43135b24d29c359a5cccf1`
- MCP: `sha256:0cd7d694b0e2d8614e28bf40e477104dc086e51c980b3845cc54a33726959b09`

A standalone copied SQLite database can omit committed changes still in WAL.
Restore the complete collection directory (including any WAL files), or make an
online SDK/SQLite backup. Current migration previews were refreshed from the
consistent snapshot including its WAL.

Post-deployment recovery evidence: coordinated backup `6cca2aae` and approved
offsite snapshot `d5382132`, both at 2026-10-13 07:43:47 UTC. Both backup jobs
reported success. Restoring the new MCP journal from the offsite snapshot with
restic file verification recovered both tables, passed SQLite integrity and
matched the original database bytes. The disposable restore directory was
removed. Native/MCP snapshot parity includes all 4,512 media files and these
semantic hashes (JSON rows, UTF-8, compact separators):

- Notes: `79921624927969fd3e1d1c8eafd49510bc5465a407e0cac8d6b4d29bbbb999fc`
- Cards: `ac749ce344da4e6000f78de4aacb9b7408142a298b3d0203930e16a99ceb0364`
- Reviews: `2e6ccc93ba559b7e92492ec86bdcb834826852bb011188ae1747f66167ce8ba5`
- Schemas: `6597ec454fad663343cd5c7c09088d2c3cd2914d6218bec5cc455a67f8c8d0fe`

The pre-deployment disposable snapshot has the same note/card/review hashes.
The maintenance service's idle execution reports success; no production
migration was queued. Its full apply path has native-server fixture coverage,
but the host coordinator's production apply remains unexercised until confirmation.

## Template editing deployment — 2026-10-17

Deployed `anki_preview_template_update` and `anki_apply_template_update` for
existing card front/back sources and note-type CSS. Edits use the existing
backed-up host maintenance workflow, with exact revision/patch binding,
principal-bound retry receipts, clone validation and native-server verification.
Fields, card IDs, scheduling and reviews are preserved. Applying an edit requires
maintenance permission and explicit confirmation; other devices must full
DOWNLOAD after completion. No production template edit was requested during this
deployment. Refresh connector tool discovery to expose the new tools.

Validation: `just check`, `just anki-stack-check`, `just anki-stack-test` (15 MCP
tests plus one native-client test), and `just anki-deploy-syntax` passed. Coverage
includes front/back/CSS server round trips, unchanged source after preview,
revision changes for both source and patch, invalid/oversized inputs and retry
key conflicts. Actual device rendering remains untested.

`just anki-deploy` completed with 32 successful Ansible tasks and zero failures.
Both public Anki health endpoints, authentication enforcement, and all five
existing proxy services passed. A connected MCP status call confirmed guarded
writes enabled, no pending operations or maintenance, and no sync failure:
674 notes, 712 cards, 2,292 reviews and 18 note types.

MCP image config: `sha256:e2b51dacd1f6f638c1d31597194e1cbdc98dc3945df9efd5a7ff776f9ac1325e`.

## Maintenance restart race — 2026-10-18

Caddy recorded three HTTP 502s at 06:16:32 UTC (08:16:32 Madrid) for
`anki_list_decks`, `anki_get_authoring_context` and `anki_status`. Template
maintenance had verified successfully, but its exit handler restored public
routes before starting MCP. Docker DNS could not resolve the stopped backend;
MCP became healthy at 06:16:38 UTC.

The coordinator now waits for healthy backends before restoring public routes,
and leaves maintenance 503 responses in place if startup fails. MCP uses
`http://anki-sync:8080/` on the private Docker network so initial synchronization
can complete while public sync is paused. No host backend port is published.
The regression test covers successful recovery states, failed startup and
reconciliation; it fails against the original handler. Deployment now runs this
test and the deployment syntax check automatically.

`just anki-deploy` passed: project check, scoped lint, 26 MCP tests, one native
sync test, coordinator regression and deployment syntax. Ansible completed with
32 successful tasks and zero failures. Both public health endpoints,
authentication enforcement and all five other proxy services passed. The
installed coordinator SHA256 matches the local file. Connected MCP status and
authoring-context calls succeeded: 686 notes, 725 cards, 2,362 reviews, 18 note
types, no failed sync, pending operations or pending maintenance.
