#!/usr/bin/env python3
"""Measure the macOS in-place archive backend in an exclusively owned scratch tree.

Run inside nix develop. Supply --target and --slot explicitly. Nothing is removed:
the slot owner must reclaim the listed scratch directories after collecting logs.
Raw logs can contain local paths and must not be published. results.json contains
only metrics, relative scratch names, tool versions and the measured source SHA.
"""

import argparse
from collections import Counter
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import tarfile
import time

from build_backend_control import RunInterrupted, owned_cache, quarantine, restore_quarantine


def output(argv, **kwargs):
    return subprocess.check_output(argv, text=True, **kwargs).strip()


def load():
    return float(output(["sysctl", "-n", "vm.loadavg"]).split()[1])


def lock_kinds(text):
    return dict(Counter(re.findall(r"Blocking waiting for file lock on ([^\r\n]+)", text)))


def log_annotations(text):
    return {"nix_sqlite_busy_messages": sum("SQLite database" in line and "is busy" in line
                                            for line in text.splitlines()),
            "nix_auto_gc_messages": text.count("running auto-GC"),
            "nix_gc_lock_messages": text.count("waiting for the big garbage collector lock")}


def delta(before, after):
    if isinstance(after, dict):
        return {key: delta(before.get(key, {} if isinstance(value, dict) else 0), value)
                for key, value in after.items()}
    if isinstance(after, (int, float)):
        return after - before
    return after


class Measurement:
    def __init__(self, args):
        self.args = args
        self.repo = Path(__file__).resolve().parents[1]
        self.target = args.target.resolve()
        self.root = self.target.parent
        self.prefix = self.target.name
        self.logs = self.path("results")
        if not (args.append_controls or args.rerun_nix) and (self.target.exists() or self.logs.exists()):
            raise RuntimeError("use a fresh exclusively owned target prefix")
        self.disk()
        self.logs.mkdir(parents=True, exist_ok=args.append_controls or args.rerun_nix)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("FLOCK_") and not key.startswith("CARGO_PROFILE_")}
        self.env.update(CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
                        NEXTEST_TEST_THREADS="2", SCCACHE_DIR=str(self.path("sccache")),
                        SCCACHE_SERVER_UDS=str(self.path("sccache") / "server.sock"),
                        SCCACHE_IGNORE_SERVER_IO_ERROR="1")
        temporary = self.path("sccache") / "tmp"
        temporary.mkdir(parents=True, exist_ok=True)
        self.env["TMPDIR"] = str(temporary)
        os.environ["TMPDIR"] = str(temporary)
        self.session = None
        self.env.pop("SCCACHE_SERVER_PORT", None)
        self.env.pop("RUSTFLAGS", None)
        self.env.pop("CARGO_ENCODED_RUSTFLAGS", None)
        self.env["RUSTC_WRAPPER"] = shutil.which("sccache") or ""
        if not self.env["RUSTC_WRAPPER"]:
            raise RuntimeError("sccache is required")
        if args.append_controls or args.rerun_nix:
            self.data = json.loads((self.logs / "results.json").read_text())
            if not args.rerun_nix and self.data["source_sha"] != output(["git", "rev-parse", "HEAD"], cwd=self.repo):
                raise RuntimeError("source changed since the initial measurement")
            return
        self.data = {
            "source_sha": output(["git", "rev-parse", "HEAD"], cwd=self.repo),
            "host": {"os": platform.mac_ver()[0],
                     "cores": int(output(["sysctl", "-n", "hw.ncpu"])),
                     "ram_bytes": int(output(["sysctl", "-n", "hw.memsize"]))},
            "versions": {name: output([name, "--version"], env=self.env)
                         for name in ("rustc", "cargo", "nix", "sccache")},
            "nextest": output(["cargo", "nextest", "--version"], env=self.env),
            "rows": [], "groups": [],
            "skipped": {"four_builds": "host rules allow at most two concurrent builds",
                        "k3s": "cluster access excluded from this round",
                        "tart": "VM access excluded from this round",
                        "test_execution": "archive builds tests without executing them"}}
        self.checkpoint()

    def path(self, suffix):
        return self.root / f"{self.prefix}-{suffix}"

    def disk(self):
        if shutil.disk_usage(self.root).free < 15 * 1024**3:
            raise RuntimeError("disk: less than 15 GiB free")

    def admit(self):
        self.disk()
        if self.session and self.session.cancelled:
            raise RunInterrupted(self.session.cancelled)
        deadline = time.monotonic() + 300 * 60
        while not self.args.slot.is_file() or load() >= 10:
            if time.monotonic() >= deadline:
                raise RuntimeError("no slot within 300 minutes")
            print("Waiting for admission: slot required, load < 10", flush=True)
            # The host admission protocol explicitly requires 60 second polls.
            if self.session:
                if self.session.cancel_event.wait(60):
                    raise RunInterrupted(self.session.cancelled)
            else:
                time.sleep(60)
            self.disk()

    def checkpoint(self):
        # Some version banners include local configuration paths after line one.
        self.data["versions"] = {key: value.splitlines()[0]
                                 for key, value in self.data["versions"].items()}
        self.data["nextest"] = self.data["nextest"].splitlines()[0]
        self.data["host"].pop("architecture", None)
        self.data["host"]["filesystem"] = "APFS"
        self.data["scratch"] = sorted(p.name for p in self.root.glob(f"{self.prefix}*"))
        (self.logs / "results.json").write_text(json.dumps(self.data, indent=2) + "\n")

    def copy(self, source, destination):
        self.disk()
        if destination.exists():
            raise RuntimeError("copy destination already exists")
        start = time.monotonic()
        subprocess.run(["/bin/cp", "-cRp", str(source), str(destination)], check=True)
        self.data.setdefault("copies", []).append({"destination": destination.name,
                                                    "wall_s": time.monotonic() - start})
        self.checkpoint()

    def checkout(self, name):
        self.disk()
        dest = self.path(name)
        dest.mkdir()
        archive = self.logs / f"{name}.tar"
        with archive.open("wb") as stream:
            subprocess.run(["git", "archive", "HEAD"], cwd=self.repo, stdout=stream, check=True)
        with tarfile.open(archive) as stream:
            stream.extractall(dest, filter="data")
        self.checkpoint()
        return dest

    def stats(self):
        return json.loads(output(["sccache", "--show-stats", "--stats-format", "json"],
                                 env=self.env))["stats"]

    def measure(self, name, argv, cwd, env):
        self.admit()
        logfile = self.logs / f"{name}.log"
        started_load = load()
        start = time.monotonic()
        peak_group_rss = 0
        with logfile.open("w") as stream:
            child = self.session.spawn(["/usr/bin/time", "-l", *argv], cwd=cwd,
                                     env=env, stdout=stream, stderr=stream)
            while True:
                try:
                    code = child.wait(timeout=1)
                    break
                except subprocess.TimeoutExpired:
                    processes = output(["ps", "-axo", "pgid=,rss="])
                    rss_kib = sum(int(fields[1]) for line in processes.splitlines()
                                  if len(fields := line.split()) == 2
                                  and int(fields[0]) == child.pid)
                    peak_group_rss = max(peak_group_rss, rss_kib * 1024)
                    if time.monotonic() - start >= 1800 or ("nix-four" in name and load() >= 10):
                        os.killpg(child.pid, signal.SIGKILL)
                        code = child.wait()
                        break
        elapsed = time.monotonic() - start
        text = logfile.read_text()
        rss = re.search(r"(\d+)\s+maximum resident set size", text)
        footprint = re.search(r"(\d+)\s+peak memory footprint", text)
        row = {"scenario": name, "exit": code, "wall_s": elapsed,
               "lifecycle_run": len(self.data["lifecycle_runs"]), "tmpdir_owned": True,
               "max_rss_bytes": int(rss[1]) if rss else None,
               "peak_footprint_bytes": int(footprint[1]) if footprint else None,
               "sampled_process_group_peak_rss_bytes": peak_group_rss,
               "load_start": started_load, "load_end": load(),
               "cargo_lock_messages": text.count("Blocking waiting for file lock"),
               "cargo_lock_kinds": lock_kinds(text),
               "command": [self.public_arg(arg) for arg in argv]}
        row.update(log_annotations(text))
        print(f"{name}: exit={code}, wall={elapsed:.2f}s", flush=True)
        return row

    def public_arg(self, arg):
        return str(arg).replace(str(self.root), "$SCRATCH").replace(str(self.repo), "$REPO")

    def archive(self, name, source, target, full_debug=False):
        env = self.env | {"CARGO_TARGET_DIR": str(target),
                          "FLOCK_GUI_DIST": str(source / "tests/fixtures/gui-dist")}
        argv = ["cargo", "nextest", "archive", "--locked", "--features", "web,ts",
                "--timings", "--archive-file", str(self.logs / f"{name}.tar.zst")]
        if full_debug:
            env.update(CARGO_PROFILE_DEV_DEBUG="true", CARGO_PROFILE_TEST_DEBUG="true")
            argv += ["--config", 'profile.dev.package."*".debug=true']
        return argv, env

    def record_archives(self, label, jobs, full_debug=False):
        self.admit()
        before = self.stats()
        start = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(jobs)) as pool:
            futures = []
            for name, source, target in jobs:
                argv, env = self.archive(name, source, target, full_debug)
                futures.append(pool.submit(self.measure, name, argv, source, env))
            rows = [future.result() for future in futures]
        clean = quarantine(rows)
        self.data["groups"].append({"scenario": label, "wall_s": time.monotonic() - start,
                                    "sccache_delta": delta(before, self.stats()),
                                    "gc_contaminated": not clean,
                                    "aggregate_eligible": clean and all(r["exit"] == 0 for r in rows)})
        for row, (_, _, target) in zip(rows, jobs):
            row["target_allocated_kib"] = int(output(["du", "-sk", str(target)]).split()[0])
            artifact = self.logs / f"{row['scenario']}.tar.zst"
            row["archive_bytes"] = artifact.stat().st_size if artifact.exists() else None
        self.data["rows"].extend(rows)
        self.checkpoint()
        if not clean:
            raise RuntimeError("GC contaminated archive; restart from its original target state")
        if any(row["exit"] for row in rows):
            raise RuntimeError(f"{label}: archive failed; inspect private logs")

    def run(self):
        source = self.checkout("source")
        module = source / "src/main.rs"
        source_times = (module.stat().st_atime_ns, module.stat().st_mtime_ns)
        self.admit()
        with (self.logs / "fetch.log").open("w") as stream:
            fetch = self.session.spawn(["cargo", "fetch", "--locked"], cwd=source,
                                       stdout=stream, stderr=stream)
            if fetch.wait(timeout=1800):
                raise RuntimeError("cargo fetch failed")
        self.env["CARGO_NET_OFFLINE"] = "true"
        self.record_archives("cold", [("cold", source, self.target)])
        seed = self.path("seed")
        self.copy(self.target, seed)
        # Restores preserve mtimes and the original absolute Cargo target path.
        for repeat in range(1, 3):
            os.utime(module, ns=source_times)
            self.target.rename(self.path(f"retired-{repeat}"))
            self.copy(seed, self.target)
            self.record_archives(f"same-{repeat}", [(f"same-{repeat}", source, self.target)])
            relocated = self.path(f"relocated-{repeat}")
            relocated_source = self.path(f"relocated-source-{repeat}")
            self.copy(source, relocated_source)
            self.copy(seed, relocated)
            self.record_archives(f"different-{repeat}",
                                 [(f"different-{repeat}", relocated_source, relocated)])
            module = source / "src/main.rs"
            module.touch()
            self.record_archives(f"touch-{repeat}", [(f"touch-{repeat}", source, self.target)])
            fresh = self.checkout(f"fresh-source-{repeat}")
            fresh_target = self.path(f"fresh-{repeat}")
            self.copy(seed, fresh_target)
            self.record_archives(f"fresh-{repeat}", [(f"fresh-{repeat}", fresh, fresh_target)])
        module = source / "src/main.rs"
        original = module.read_text()
        for repeat in range(1, 3):
            for cycle in range(1, 11):
                module.write_text(original + f"\n// Measurement edit {repeat}, cycle {cycle}.\n")
                name = f"edit-{repeat}-{cycle:02}"
                self.record_archives(name, [(name, source, self.target)])
        # Compare one/two independent edited targets, rather than Cargo's own
        # serialization of two clients sharing a writable target directory.
        paired = []
        for index in range(2):
            dest_source = self.path(f"parallel-source-{index}")
            dest_target = self.path(f"parallel-{index}")
            self.copy(source, dest_source)
            self.copy(self.target, dest_target)
            paired.append((dest_source, dest_target))
        for repeat in range(1, 3):
            for src, _ in paired:
                (src / "src/main.rs").write_text(original + f"\n// Serial edit {repeat}.\n")
            self.record_archives(f"serial-{repeat}",
                                 [(f"serial-{repeat}", *paired[0])])
            for src, _ in paired:
                (src / "src/main.rs").write_text(original + f"\n// Concurrent edit {repeat}.\n")
            self.record_archives(f"concurrent-{repeat}",
                                 [(f"concurrent-{repeat}-{i}", *pair)
                                  for i, pair in enumerate(paired)])
        full = self.path("full-debug")
        self.record_archives("full-debug-cold", [("full-debug-cold", source, full)], True)
        for repeat in range(1, 3):
            module.touch()
            self.record_archives(f"full-debug-touch-{repeat}",
                                 [(f"full-debug-touch-{repeat}", source, full)], True)
        self.nix_runs()
        print("results.json complete", flush=True)

    def nix_runs(self):
        self.data["gc_policy"] = {
            "attempt_limit": self.args.gc_attempts,
            "evidence": "captured auto-GC and GC-lock messages; entire batch quarantined",
            "limit": "daemon GC lock unreadable; unrelated silent GC not observable"}
        run_id = len(self.data["lifecycle_runs"])
        for label, count in (("nix-fresh-eval", 1), ("nix-warm", 2),
                             ("nix-four-a", 4), ("nix-four-b", 4)):
            self.data.get("unavailable", {}).pop(label, None)
            argv = ["nix"]
            if label == "nix-fresh-eval":
                argv += ["--option", "eval-cache", "false"]
            argv += ["develop", "--command", "true"]
            for attempt in range(1, self.args.gc_attempts + 1):
                self.admit()
                start = time.monotonic()
                with concurrent.futures.ThreadPoolExecutor(max_workers=4 if count == 4 else 1) as pool:
                    futures = [pool.submit(self.measure,
                                           f"{label}-fix1-r{run_id}-a{attempt}-{i+1}", argv, self.repo,
                                           self.env | {"CARGO_TARGET_DIR": str(self.target)})
                               for i in range(count)]
                    rows = [future.result() for future in futures]
                clean = quarantine(rows)
                group = {"scenario": label, "attempt": attempt,
                         "wall_s": time.monotonic() - start, "gc_contaminated": not clean,
                         "sample_ids": [row["scenario"] for row in rows],
                         "aggregate_eligible": clean and all(r["exit"] == 0 for r in rows)}
                self.data["rows"].extend(rows)
                self.data["groups"].append(group)
                self.checkpoint()
                if any(row["exit"] for row in rows):
                    raise RuntimeError(f"{label}: nix failed")
                if clean:
                    break
            if not clean:
                self.data.setdefault("unavailable", {})[label] = (
                    f"GC signals in all {self.args.gc_attempts} attempts; no clean aggregate")
        self.checkpoint()

    def rerun_nix(self):
        restore_quarantine(self.data)
        for row in self.data["rows"]:
            if row["scenario"].startswith("nix-"):
                row["aggregate_eligible"] = False
                row["exclusion_reason"] = "superseded by GC-controlled Nix rerun"
        for group in self.data["groups"]:
            if group["scenario"].startswith("nix-"):
                group["aggregate_eligible"] = False
                group["exclusion_reason"] = "superseded by GC-controlled Nix rerun"
        self.data["nix_rerun_source_sha"] = output(["git", "rev-parse", "HEAD"], cwd=self.repo)
        self.nix_runs()
        print("GC-controlled Nix rerun complete", flush=True)

    def controls(self):
        """Separate cache reuse and steady-path concurrency from relocation."""
        self.env["CARGO_NET_OFFLINE"] = "true"
        for repeat in range(1, 3):
            source = self.checkout(f"empty-source-{repeat}")
            name = f"empty-checkout-{repeat}"
            self.record_archives(name, [(name, source, self.path(name))])
        original = output(["git", "show", "HEAD:src/main.rs"], cwd=self.repo)
        paired = [(self.path(f"parallel-source-{i}"), self.path(f"parallel-{i}"))
                  for i in range(2)]
        for repeat in range(1, 3):
            for source, _ in paired:
                (source / "src/main.rs").write_text(original + f"\n// Steady serial {repeat}.\n")
            label = f"steady-serial-{repeat}"
            self.record_archives(label, [(label, *paired[0])])
            for source, _ in paired:
                (source / "src/main.rs").write_text(original + f"\n// Steady concurrent {repeat}.\n")
            label = f"steady-concurrent-{repeat}"
            self.record_archives(label, [(f"{label}-{i}", *pair)
                                         for i, pair in enumerate(paired)])
        for row in self.data["rows"]:
            text = (self.logs / f"{row['scenario']}.log").read_text()
            row["cargo_lock_kinds"] = lock_kinds(text)
            row.update(log_annotations(text))
        self.checkpoint()
        print("control results complete", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--slot", type=Path, required=True)
    parser.add_argument("--append-controls", action="store_true",
                        help="after the main run, append warm-cache and steady-path controls")
    parser.add_argument("--rerun-nix", action="store_true",
                        help="append GC-controlled Nix samples to an existing result file")
    parser.add_argument("--gc-attempts", type=int, choices=range(1, 4), default=3)
    args = parser.parse_args()
    if args.rerun_nix and args.append_controls:
        parser.error("choose one append mode")
    if platform.system() != "Darwin":
        parser.error("this harness requires macOS /usr/bin/time -l and APFS clone copies")
    measurement = Measurement(args)
    measurement.data.pop("failure", None)
    audit = {"owned_tmpdir": True, "mode": "rerun-nix" if args.rerun_nix else "archives",
             "controller_sha256": {name: hashlib.sha256((measurement.repo / "scripts" / name).read_bytes()).hexdigest()
                                   for name in ("measure_build_backend.py", "build_backend_control.py")}}
    measurement.data.setdefault("lifecycle_runs", []).append(audit)
    code = 0
    try:
        measurement.admit()
        with owned_cache(measurement.env, audit) as session:
            measurement.session = session
            if args.rerun_nix:
                measurement.rerun_nix()
            elif args.append_controls:
                measurement.controls()
            else:
                measurement.run()
    except (RuntimeError, subprocess.SubprocessError, RunInterrupted) as error:
        measurement.data["failure"] = str(error)
        audit["failure"] = str(error)
        code = 128 + error.signum if isinstance(error, RunInterrupted) else 1
    finally:
        measurement.checkpoint()
    raise SystemExit(code)


if __name__ == "__main__":
    main()
