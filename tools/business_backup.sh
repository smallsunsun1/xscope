#!/bin/sh
# PostgreSQL tools only. No Gateway WAL, identity tables or automatic deletion.
set -eu
umask 077
task_tmp=$(mktemp -d /tmp/xscope-backup.XXXXXX)
task_exporter=
cleanup() {
  if [ -n "$task_exporter" ]; then
    printf 'ROLLBACK;\n\\q\n' >&3 2>/dev/null || true
    exec 3>&-
    wait "$task_exporter" 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'exit 1' INT TERM
task_bundle=$(mktemp -d /backups/backup-XXXXXXXX)
# Keep an exported snapshot open so the schema, migrations and verification
# manifest are from one database snapshot even while financial writers run.
mkfifo "$task_tmp/commands"
psql -XqAt -v ON_ERROR_STOP=1 <"$task_tmp/commands" >"$task_tmp/snapshot" 2>"$task_tmp/error" &
task_exporter=$!
exec 3>"$task_tmp/commands"
printf "SET idle_in_transaction_session_timeout='780s';\nBEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;\nSELECT pg_export_snapshot();\n" >&3
task_wait=0
while [ ! -s "$task_tmp/snapshot" ]; do
  task_wait=$((task_wait + 1))
  if [ "$task_wait" -ge 20 ] || ! kill -0 "$task_exporter" 2>/dev/null; then
    echo 'Backup snapshot unavailable; private details suppressed' >&2; exit 1
  fi
  sleep 1
done
task_snapshot=$(head -n 1 "$task_tmp/snapshot")
case "$task_snapshot" in ''|*[!0-9A-Fa-f-]*) echo 'Invalid snapshot identity' >&2; exit 1;; esac
if ! pg_dump -Fc --snapshot="$task_snapshot" --schema=xscope --lock-wait-timeout=5000 --file="$task_bundle/business.dump" 2>"$task_tmp/dump-error"; then
  echo 'Business backup failed; incomplete bundle retained' >&2; exit 1
fi
if ! pg_dump -Fc --snapshot="$task_snapshot" --table=public.seaql_migrations --lock-wait-timeout=5000 --file="$task_bundle/migrations.dump" 2>"$task_tmp/migration-error"; then
  echo 'Migration backup failed; incomplete bundle retained' >&2; exit 1
fi
# Generate one count query per business table; no table contents in logs.
if ! psql -XqAt -v ON_ERROR_STOP=1 >"$task_bundle/counts.tsv" 2>"$task_tmp/count-error" <<SQL
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET TRANSACTION SNAPSHOT '$task_snapshot';
SELECT format('SELECT %L, count(*) FROM %I.%I', tablename, schemaname, tablename)
FROM pg_tables WHERE schemaname='xscope' ORDER BY tablename
\gexec
COMMIT;
SQL
then echo 'Backup manifest failed; incomplete bundle retained' >&2; exit 1; fi
pg_restore --list "$task_bundle/business.dump" >/dev/null 2>"$task_tmp/list-error"
pg_restore --list "$task_bundle/migrations.dump" >/dev/null 2>"$task_tmp/list-error"
date -u +%Y-%m-%dT%H:%M:%SZ >"$task_bundle/completed-at"
(cd "$task_bundle" && sha256sum business.dump migrations.dump counts.tsv completed-at > SHA256SUMS)
# Marker is last; a partial directory is never selected as a restorable backup.
touch "$task_bundle/COMPLETE"
echo 'Business backup completed; identity tables excluded; no retention deletion performed'
