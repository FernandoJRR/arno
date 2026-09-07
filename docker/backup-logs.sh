#!/bin/sh
# Off-host copy of arno's chain stores (SPEC §11 decision #15, STACK §8.1).
#
# The hash chain proves entries were not edited/reordered/deleted in the
# interior, but tail-truncation of the newest segment is undetectable from the
# files alone — only a second copy catches it. Run this nightly from host cron:
#   crontab: 0 3 * * * /path/to/arno/docker/backup-logs.sh
#
# Env:
#   ARNO_BACKUP_DEST   destination directory (default ~/arno-logs-backup)
#   ARNO_DATA_DIR      source directory   (default <repo>/data)
set -eu

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${ARNO_BACKUP_DEST:-$HOME/arno-logs-backup}"
SRC="${ARNO_DATA_DIR:-$REPO_DIR/data}"

if [ ! -d "$SRC" ]; then
    echo "backup-logs: source $SRC does not exist — nothing to do" >&2
    exit 1
fi

mkdir -p "$DEST"
# -a preserves timestamps; --chmod forces owner-only even if a source file
# (e.g. the tool policy, written by a container with a different umask) is
# group/world-readable at the source.
rsync -a --chmod=Du=rwx,go-rwx,Fu=rw,go-rwx "$SRC/" "$DEST/"
# Belt-and-suspenders: some rsync builds ignore --chmod on existing files.
chmod -R go-rwx "$DEST"
echo "backup-logs: copied $(ls -1 "$DEST" | wc -l | tr -d ' ') file(s) to $DEST"
