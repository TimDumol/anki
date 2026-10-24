# Jupiter inventory (2026-10-12)

Target: `jupiter.timdumol.com`, SSH deployment account `timdumol` with passwordless sudo.
Architecture: x86_64. RAM: 3814 MiB, available at inspection 2805 MiB; no swap.
Disk: 38 GiB root filesystem, approximately 20 GiB available.

The existing edge is `app-caddy-1`, image `caddy:2.11.4`, owned by
`/opt/aprendiendo-mcp/app/compose.production.yaml`. Its Caddyfile imports
`sites-enabled/*`; fragments live in `/opt/caddy/sites-enabled`. Backends
join the external Docker bridge `caddy-edge`. Do not replace the edge Compose
project or publish additional host HTTP ports.

Existing routes to preserve:

| Host                    | Backend               |
| ----------------------- | --------------------- |
| mars.timdumol.com       | Aprendiendo, mcp:8080 |
| auth.aries.timdumol.com | fitsync-pocketid:1411 |
| api.aries.timdumol.com  | fitsync-fitnessd:3000 |
| athena.timdumol.com     | presto-finance:3000   |
| vulcan.timdumol.com     | vulcan-vikunja:3456   |

Backup: `aries-server-backup.timer` invokes `/usr/local/sbin/aries-server-backup`.
It takes SQLite online backups and checks integrity before restic, records a
manifest and image IDs, and excludes the corresponding live databases.
Repository: `/var/lib/presto-backup/repository`; credentials remain on server in
`/etc/presto-backup/password`. Retention: within 7 days, 30 daily, 12 weekly,
12 monthly. The script labels snapshots `--host zeus --tag aries-server`;
this label alone does **not** prove an offsite copy exists. Verify replication
and a restore before accepting G0/recovery or production cutover.

Approved domains: `api.jupiter.timdumol.com` for native sync and
`mcp.jupiter.timdumol.com` for MCP. Issuer: `https://auth.aries.timdumol.com`.
Verified export supplied: `anki-premigrate.colpkg`, restored by the user in Desktop.
Rust disposable import: 665 notes, 699 cards, 2,003 review records.
Client versions supplied: Desktop 26.08.1 and AnkiDroid 2.25.1.
The initial staging server and Rust adapter share source commit
`dd324c8acc3d61f7f2696ea6b9f26bf2b494ca45` (checkout version 26.09.3).
Compatibility with these devices remains an acceptance test, not an assumption.

The user moved their everyday Desktop profile to Jupiter and uploaded the
collection. The populated account is `timdumol`; MCP has a separate matching
client collection. ChatGPT discovery/status is confirmed. Rust outage/retry and
maintenance round trips pass. The user explicitly waived the manual context
exercise on Desktop/AnkiDroid; device rendering remains unverified. Production
template changes still require a reviewed migration and explicit confirmation.
