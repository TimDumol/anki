#!/usr/bin/env bash
set -euo pipefail
stack_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "$stack_dir/../.." && pwd)"
cd "$repo_dir"
command -v just >/dev/null
command -v ansible-playbook >/dev/null
anki_sync_domain="${ANKI_SYNC_DOMAIN:-api.jupiter.timdumol.com}"
just check
just anki-security-audit
just anki-stack-check
just anki-stack-test
just anki-maintenance-test
just anki-deploy-syntax
just anki-sync-image
just anki-mcp-image
archive="$(mktemp /tmp/anki-sync-image.XXXXXX.tar)"
backup_lock_in=''
backup_lock_pid=''
cleanup() {
    if [[ -n "$backup_lock_in" ]]; then
        exec {backup_lock_in}>&-
        wait "$backup_lock_pid" || true
    fi
    rm -f "$archive" "${archive}.lock.log"
}
trap cleanup EXIT
docker save --output "$archive" anki-sync:dd324c8 anki-mcp:staging
# Hold the snapshot/replication lock throughout the Ansible rollout.
# Closing this pipe releases it, including on failure.
coproc ANKI_BACKUP_LOCK {
    ssh -T jupiter.timdumol.com "sudo -n flock -w 300 /var/lib/presto-backup/job.lock sh -c 'printf \"locked\\n\"; cat >/dev/null'" 2>"${archive}.lock.log"
}
backup_lock_pid="$ANKI_BACKUP_LOCK_PID"
backup_lock_in="${ANKI_BACKUP_LOCK[1]}"
if ! IFS= read -r -t 310 lock_ready <&"${ANKI_BACKUP_LOCK[0]}"; then
    cat "${archive}.lock.log" >&2
    exit 1
fi
[[ "$lock_ready" == locked ]]
ansible-playbook -i "$stack_dir/ansible/inventory.yml" \
    "$stack_dir/ansible/site.yml" \
    -e "anki_image_archive=$archive" -e "anki_sync_domain=$anki_sync_domain" "$@"
