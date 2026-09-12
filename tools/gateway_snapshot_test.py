"""Read-only rollout snapshot checks; no cluster access or archive extraction."""
import hashlib
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

from prepare_usage_cutover import inspect_snapshot


class SnapshotTest(unittest.TestCase):
    def inspect(self, entries):
        with tempfile.TemporaryDirectory(prefix="xscope-wal-snapshot-") as directory:
            path = Path(directory) / "wal.tar"
            with tarfile.open(path, "w") as snapshot:
                for name, data in entries:
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    snapshot.addfile(member, io.BytesIO(data))
            return inspect_snapshot(path)

    def test_all_generations_including_empty_active_and_intents(self):
        entries = [("./events.jsonl", b"{}\n"),
                   ("./events.jsonl.segments/00000000000000000001.jsonl", b""),
                   ("./events.jsonl.reservations.jsonl", b'{"intent":1}\n'),
                   ("./events.jsonl.manifest", b'{"active":1}')]
        result = self.inspect(entries)
        self.assertEqual(len(result), 3)
        for name, data in entries[:3]:
            self.assertEqual(result[str(Path(name))], (len(data), hashlib.sha256(data).hexdigest()))

    def test_invalid_data_and_paths_abort(self):
        for name, data in [("./events.jsonl", b'{"torn":'),
                           ("./events.jsonl", b"x" * ((1 << 20) + 1)),
                           ("./events.jsonl", b"not json\n"),
                           ("../events.jsonl", b"{}\n"),
                           ("/events.jsonl", b"{}\n")]:
            with self.subTest(name=name, length=len(data)), self.assertRaises(ValueError):
                self.inspect([(name, data)])
        with self.assertRaises(ValueError):
            self.inspect([])

    def test_immutable_reclamation_metadata_is_not_skipped(self):
        entries = [("events.jsonl", b"{}\n"), ("events.jsonl.seal", b'{"version":1}'),
                   ("events.jsonl.segments/00000000000000000001.jsonl.reclaimed", b'{"version":1}')]
        result = self.inspect(entries)
        self.assertEqual(len(result), 3)
        for name, data in entries:
            self.assertEqual(result[name], (len(data), hashlib.sha256(data).hexdigest()))
        with self.assertRaises(ValueError):
            self.inspect([("events.jsonl.reclaimed", b"invalid")])


if __name__ == "__main__":
    unittest.main()
