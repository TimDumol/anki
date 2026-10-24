# Copyright: Ankitects Pty Ltd and contributors
# License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

"""Exercise the coordinator's exit handler without Docker or live collections."""

import pathlib
import subprocess
import tempfile
import unittest


class MaintenanceRecovery(unittest.TestCase):
    def test_public_routes_resume_only_after_healthy_backends(self):
        source = pathlib.Path(__file__).with_name("maintenance.sh").read_text()
        handler = source[source.index("finish() {") : source.index("trap finish EXIT")]
        for state, ready in [
            ("requested", True),
            ("not_applied", True),
            ("synced_successfully", True),
            ("synced_successfully", False),
            ("reconciliation_required", True),
        ]:
            with self.subTest(state=state, ready=ready), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                (root / "anki.caddy").write_text("public routes")
                site = root / "live.caddy"
                site.write_text("maintenance 503")
                log = root / "events"
                script = '''
set -eu
backup=$1
site=$2
compose=unused
mock_state=$3
events=$4
ready=$5
request_state() { printf '%s' "$mock_state"; }
docker() { echo ready >> "$events"; return "$ready"; }
reload() { echo public >> "$events"; }
'''
                result = subprocess.run(
                    [
                        "bash", "-c", script + handler + '\nfinish "$@"', "test",
                        str(root), str(site), state, str(log), "0" if ready else "1",
                    ],
                    capture_output=True,
                    text=True,
                )
                if state == "reconciliation_required":
                    self.assertEqual(site.read_text(), "maintenance 503")
                    self.assertFalse(log.exists())
                elif ready:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(site.read_text(), "public routes")
                    self.assertEqual(log.read_text().splitlines(), ["ready", "public"])
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(site.read_text(), "maintenance 503")
                    self.assertEqual(log.read_text().splitlines(), ["ready"])


if __name__ == "__main__":
    unittest.main()
