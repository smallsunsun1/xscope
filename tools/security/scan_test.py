"""The public-repository detector must fail closed and redact its own output."""
import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest

from python.runfiles import runfiles
from scan import run_scan

resolver = runfiles.Create()
binary = resolver.Rlocation(sys.argv.pop(1))
config = resolver.Rlocation(sys.argv.pop(1))


class ScanTest(unittest.TestCase):
    def test_sensitive_identifiers_are_detected_without_echo(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            source = directory / "working"
            source.mkdir()
            synthetic_id = "LTAI" + "0" * 20
            synthetic_endpoint = "example-" + "0" * 16 + ".oss-cn-hangzhou.oss-accesspoint.aliyuncs.com"
            (source / "fixture.txt").write_text(synthetic_id + "\n" + synthetic_endpoint)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                failed = run_scan(binary, config, "dir", source, directory / "report.json")
            self.assertTrue(failed)
            self.assertIn("aliyun-access-key-id", output.getvalue())
            self.assertIn("private-oss-access-point", output.getvalue())
            self.assertNotIn(synthetic_id, output.getvalue())
            self.assertNotIn(synthetic_endpoint, output.getvalue())


if __name__ == "__main__":
    unittest.main()
