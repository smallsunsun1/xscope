"""Real pg_dump/pg_restore test on synthetic, isolated PostgreSQL only."""
import os
from pathlib import Path
import secrets
import subprocess
import time
import unittest
from python.runfiles import runfiles


def command(args, payload=None):
    result = subprocess.run(args, input=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=80)
    if result.returncode:
        category = ", ".join(code for code in ["already exists", "does not exist", "permission denied", "could not", "unsupported", "connection", "unexpected", "shutting down", "server closed", "administrator command", "recovery mode", "password authentication failed"] if code in result.stderr.decode().lower()) or "unclassified"
        raise AssertionError("synthetic backup operation failed: " + category + "; private output suppressed")
    return result.stdout.decode().strip()


class BackupTest(unittest.TestCase):
    def test_snapshot_backup_excludes_identity_and_restores_function_dependencies(self):
        result = subprocess.run(["docker", "run", "--rm", "-d", "--network=none", "--cpus=.5", "--memory=256m", "-e", "POSTGRES_PASSWORD", "postgres:16-alpine"], env={**os.environ, "POSTGRES_PASSWORD": secrets.token_urlsafe(32)}, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        container = result.stdout.decode().strip()
        try:
            for _ in range(80):
                if subprocess.run(["docker", "exec", container, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0: break
                time.sleep(.25)
            def sql(value, database="postgres"):
                return command(["docker", "exec", "-i", container, "psql", "-XqAt", "-U", "postgres", "-d", database, "-v", "ON_ERROR_STOP=1"], value.encode())
            sql("""CREATE SCHEMA xscope;
CREATE TABLE xscope.records(id int primary key, amount bigint);
INSERT INTO xscope.records VALUES(1,42);
CREATE FUNCTION xscope.guard() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'append only'; END $$;
CREATE TRIGGER guard BEFORE DELETE ON xscope.records FOR EACH STATEMENT EXECUTE FUNCTION xscope.guard();
CREATE TABLE public.seaql_migrations(version text primary key, applied_at bigint);
INSERT INTO public.seaql_migrations VALUES('synthetic-version',1);
CREATE TABLE public.identity_secret(id int); INSERT INTO public.identity_secret VALUES(1);
CREATE DATABASE restored;""")
            command(["docker", "exec", container, "mkdir", "-p", "/backups"])
            script = Path(runfiles.Create().Rlocation("_main/tools/business_backup.sh")).read_bytes()
            command(["docker", "exec", "-i", "-e", "PGUSER=postgres", "-e", "PGDATABASE=postgres", container, "sh", "-s"], script)
            bundle = command(["docker", "exec", container, "sh", "-c", "for b in /backups/backup-*; do test -f \"$b/COMPLETE\" && echo \"$b\"; done"])
            self.assertTrue(bundle.startswith("/backups/backup-"))
            self.assertEqual(command(["docker", "exec", container, "cat", bundle + "/counts.tsv"]), "records|1")
            command(["docker", "exec", container, "sh", "-c", 'cd "$1" && sha256sum -c SHA256SUMS', "sh", bundle])
            for filename in ["business.dump", "migrations.dump"]:
                command(["docker", "exec", container, "pg_restore", "-U", "postgres", "-d", "restored", "--single-transaction", "--no-owner", "--no-acl", "--exit-on-error", bundle + "/" + filename])
            self.assertEqual(sql("SELECT amount FROM xscope.records", "restored"), "42")
            self.assertEqual(sql("SELECT count(*) FROM public.seaql_migrations", "restored"), "1")
            self.assertEqual(sql("SELECT count(*) FROM pg_tables WHERE tablename='identity_secret'", "restored"), "0")
            result = subprocess.run(["docker", "exec", container, "psql", "-U", "postgres", "-d", "restored", "-v", "ON_ERROR_STOP=1", "-c", "DELETE FROM xscope.records"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.assertNotEqual(result.returncode, 0)
        except Exception:
            log = subprocess.run(["docker", "logs", container], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            text = (log.stdout + log.stderr).decode(errors="replace")
            print("Synthetic PostgreSQL failure classification:", {"signal_9": "terminated by signal 9" in text, "signal_15": "terminated by signal 15" in text, "reinitializing": "reinitializing" in text, "oom": "out of memory" in text})
            raise
        finally:
            command(["docker", "stop", container])


if __name__ == "__main__":
    unittest.main()
