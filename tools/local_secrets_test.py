"""Synthetic-only bootstrap safety checks; no live secrets or cluster mutations."""
import contextlib
import io
import unittest
from unittest.mock import patch

import local_secrets


class LocalSecretsTest(unittest.TestCase):
    def test_bootstrap_preserves_existing_and_does_not_emit_values(self):
        with patch.object(local_secrets, "captured", side_effect=["secret/existing", ""]) as captured:
            with contextlib.redirect_stdout(io.StringIO()) as output:
                local_secrets.create_if_missing("xscope-platform-secrets", {"secret": "synthetic-sensitive-value"})
        self.assertNotIn("synthetic-sensitive-value", output.getvalue())
        self.assertEqual(captured.call_count, 2)
        self.assertEqual(captured.call_args.args[0], "annotate")

    def test_database_missing_secret_with_pvc_fails_closed(self):
        with patch.object(local_secrets, "captured", side_effect=["", "pvc/data-postgres-0"]) as captured:
            with self.assertRaises(RuntimeError):
                local_secrets.create_if_missing("xscope-platform-secrets", {})
        self.assertEqual(captured.call_count, 2)


    def test_secret_goes_to_stdin_without_apply_annotation(self):
        with patch.object(local_secrets, "captured", side_effect=["", ""]) as request:
            local_secrets.create_if_missing("synthetic-secret", {"key": "synthetic-value"})
        args, kwargs = request.call_args
        self.assertEqual(args, ("create", "-f", "-"))
        self.assertNotIn("synthetic-value", str(args))
        self.assertEqual(kwargs["payload"]["stringData"], {"key": "synthetic-value"})
        self.assertNotIn("annotations", kwargs["payload"]["metadata"])


if __name__ == "__main__":
    unittest.main()
