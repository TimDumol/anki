#!/usr/bin/env bash
# Fixed host coordinator; the unprivileged MCP can request a versioned
# migration or bounded template/CSS edits. Never execute commands or paths supplied in the request JSON.
set -Eeuo pipefail
umask 077
exec 9>/var/lib/presto-backup/job.lock
flock -n 9 || exit 0
request=/opt/anki/data/mcp/collection.maintenance.json
request_state() {
    python3 - "$request" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
print(json.loads(p.read_text()).get('state', '') if p.exists() else '')
PY
}
[[ "$(request_state)" == requested ]] || exit 0
compose=/opt/anki/compose.yaml
site=/opt/caddy/sites-enabled/anki.caddy
caddy=app-caddy-1
backup="$(mktemp -d /opt/anki/rollback/migration-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX)"
cp -a "$site" "$backup/anki.caddy"
reload() {
    docker exec "$caddy" caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null
    docker exec "$caddy" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null
}
finish() {
    local state
    state="$(request_state)"
    case "$state" in
        requested|not_applied|synced_successfully)
            # Keep public traffic paused until both backends are ready. MCP
            # synchronizes over the internal network while these routes are 503.
            docker compose -f "$compose" up -d --wait --wait-timeout 120 >/dev/null || return
            cp "$backup/anki.caddy" "$site"
            reload
            ;;
        *)
            printf '%s\n' 'Migration requires operator reconciliation; Anki endpoints remain paused.' >&2
            ;;
    esac
}
trap finish EXIT
cat > /opt/caddy/anki-maintenance.tmp <<'CADDY'
api.jupiter.timdumol.com {
    header Retry-After 60
    respond "Anki template maintenance: retry shortly" 503
}
mcp.jupiter.timdumol.com {
    header Retry-After 60
    respond "Anki template maintenance: retry shortly" 503
}
CADDY
mv /opt/caddy/anki-maintenance.tmp "$site"
reload
# Stop active sync sessions before taking the consistent backup. Only the
# isolated internal native endpoint is available during the subsequent upload.
docker compose -f "$compose" stop --timeout 60 mcp sync >/dev/null
cp -a /opt/anki/data /opt/anki/secrets /opt/anki/compose.yaml "$backup/"
sync -f "$backup"
docker compose -f "$compose" up -d --wait --wait-timeout 120 sync >/dev/null
docker compose -f "$compose" run --rm --no-deps \
    -e ANKI_SYNC_ENDPOINT=http://anki-sync:8080/ \
    -e "ANKI_MAINTENANCE_BACKUP=$backup" mcp --run-maintenance >/dev/null
printf '%s\n' "Anki migration verified; rollback snapshot: $backup"
