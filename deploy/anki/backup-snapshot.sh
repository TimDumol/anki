#!/usr/bin/env bash
# Called by the existing aries-server-backup job while holding its job lock.
set -Eeuo pipefail
umask 077
stage_dir="${1:?backup staging directory required}"
compose_file=/opt/anki/compose.yaml
[[ -f "$compose_file" ]] || exit 0
snapshot_dir="$(mktemp -d "$stage_dir/anki-next.XXXXXX")"
mapfile -t running_ids < <(docker compose -f "$compose_file" ps --status running -q)
resume() {
    if ((${#running_ids[@]})); then
        docker start "${running_ids[@]}" >/dev/null
    fi
}
trap resume EXIT
if ((${#running_ids[@]})); then
    docker stop --time 60 "${running_ids[@]}" >/dev/null
fi
# Snapshot collection, media, future MCP journal, credentials and deployment
# metadata together. Neither daemon is allowed to write during the copy.
cp -a /opt/anki/. "$snapshot_dir/"
resume
trap - EXIT
rm -rf -- "$stage_dir/anki-previous"
if [[ -d "$stage_dir/anki" ]]; then
    mv -- "$stage_dir/anki" "$stage_dir/anki-previous"
fi
mv -- "$snapshot_dir" "$stage_dir/anki"
rm -rf -- "$stage_dir/anki-previous"
printf '%s\n' 'Anki collection/media/journal consistent snapshot complete'
