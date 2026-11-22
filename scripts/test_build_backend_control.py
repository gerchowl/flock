"""GC retry regressions and opt-in real-daemon lifetime probes."""

import argparse
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import unittest

from scripts.build_backend_control import quarantine


class QuarantineTests(unittest.TestCase):
    def test_gc_in_one_concurrent_member_excludes_the_whole_batch(self):
        rows = [{"exit": 0, "nix_auto_gc_messages": 1}, {"exit": 0}]
        self.assertFalse(quarantine(rows))
        self.assertTrue(all(r["gc_contaminated"] and not r["aggregate_eligible"] for r in rows))

    def run_policy(self, contaminated_attempts):
        # The controller is exercised from sample output to recorded eligibility.
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        from scripts.measure_build_backend import Measurement, log_annotations
        controller = Measurement.__new__(Measurement)
        controller.args = argparse.Namespace(gc_attempts=3)
        controller.data = {"rows": [], "groups": [], "lifecycle_runs": [{}]}
        controller.repo = Path(".")
        controller.target = Path("target")
        controller.env = {}
        controller.admit = lambda: None
        controller.checkpoint = lambda: None

        def sample(name, *_):
            dirty = "nix-fresh-eval" in name and any(
                f"a{attempt}-" in name for attempt in range(1, contaminated_attempts + 1))
            return {"scenario": name, "exit": 0, "wall_s": 99 if dirty else 1,
                    **log_annotations("waiting for the big garbage collector lock" if dirty else "ok")}

        controller.measure = sample
        controller.nix_runs()
        return controller.data

    def test_dirty_attempt_is_retained_but_only_retry_enters_aggregate(self):
        data = self.run_policy(1)
        rows = [r for r in data["rows"] if r["scenario"].startswith("nix-fresh-eval")]
        self.assertEqual([r["aggregate_eligible"] for r in rows], [False, True])
        self.assertEqual([r["wall_s"] for r in rows if r["aggregate_eligible"]], [1])

    def test_persistent_gc_stops_after_three_attempts_and_has_no_aggregate(self):
        data = self.run_policy(3)
        rows = [r for r in data["rows"] if r["scenario"].startswith("nix-fresh-eval")]
        self.assertEqual(len(rows), 3)
        self.assertFalse(any(r["aggregate_eligible"] for r in rows))
        self.assertIn("nix-fresh-eval", data["unavailable"])


PROBE = r'''
import json, os, signal, subprocess, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from build_backend_control import owned_cache, RunInterrupted, endpoint_alive
root, mode = Path(sys.argv[2]), sys.argv[3]
env = {key: value for key, value in os.environ.items() if not key.startswith("FLOCK_")}
env.pop("SCCACHE_SERVER_PORT", None)
env.update(SCCACHE_DIR=str(root / "cache"),
           SCCACHE_SERVER_UDS=str(root / "cache/server.sock"),
           TMPDIR=str(root / "cache/tmp"))
Path(env["TMPDIR"]).mkdir(parents=True)
audit, code, child = {"mode": mode}, 0, None
try:
    with owned_cache(env, audit) as cache:
        child = cache.spawn([sys.executable, "-c",
            "import os, signal; print(os.environ['TMPDIR'], flush=True); signal.pause()"],
            stdout=subprocess.PIPE, text=True)
        audit["child_tmp_owned"] = child.stdout.readline().strip() == env["TMPDIR"]
        print("ready", flush=True)
        if mode == "occupied":
            try:
                with owned_cache(env, {}):
                    raise AssertionError("occupied cache was adopted")
            except RuntimeError:
                audit["existing_preserved"] = endpoint_alive(env["SCCACHE_SERVER_UDS"])
        if mode == "exception":
            raise RuntimeError("injected workload exception")
        if mode in ("SIGINT", "SIGTERM"):
            signal.pause()
except RunInterrupted as error:
    code = 128 + error.signum
except RuntimeError:
    code = 1
finally:
    if child is not None:
        audit["child_exit"] = child.poll()
    (root / "audit.json").write_text(json.dumps(audit))
sys.exit(code)
'''


@unittest.skipUnless(os.environ.get("BUILD_BACKEND_TEST_ROOT"),
                     "real cache probes require an explicit retained scratch root and devShell")
class RealDaemonTests(unittest.TestCase):
    def probe(self, mode, expected):
        root = Path(tempfile.mkdtemp(prefix="case-", dir=os.environ["BUILD_BACKEND_TEST_ROOT"]))
        process = subprocess.Popen([sys.executable, "-c", PROBE,
                                    str(Path(__file__).resolve().parent), str(root), mode],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready, _, _ = select.select([process.stdout], [], [], 30)
            self.assertTrue(ready, "probe never became ready")
            self.assertEqual(process.stdout.readline().strip(), "ready")
            if mode in ("SIGINT", "SIGTERM"):
                process.send_signal(getattr(signal, mode))
            _, errors = process.communicate(timeout=30)
            self.assertEqual(process.returncode, expected, errors)
            audit = json.loads((root / "audit.json").read_text())
            self.assertTrue(audit["started"])
            self.assertTrue(audit["child_tmp_owned"])
            self.assertTrue(audit["endpoint_gone"])
            self.assertEqual(audit["stop_exit"], 0)
            self.assertEqual(audit["child_exit"], -signal.SIGKILL)
            if mode == "occupied":
                self.assertTrue(audit["existing_preserved"])
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=30)

    def test_normal_exit(self):
        self.probe("normal", 0)

    def test_exception(self):
        self.probe("exception", 1)

    def test_sigint(self):
        self.probe("SIGINT", 130)

    def test_sigterm(self):
        self.probe("SIGTERM", 143)

    def test_existing_endpoint_is_not_stopped_by_rejected_owner(self):
        self.probe("occupied", 0)


if __name__ == "__main__":
    unittest.main()
