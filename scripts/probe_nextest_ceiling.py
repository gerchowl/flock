"""Inject a live silent test and assert the real default/CI ceilings terminate it.

Run inside nix develop. Takes about seven minutes, with both profiles in parallel.
Uses an isolated Cargo project, no flock server, and no timing overrides.
"""
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import re
import subprocess
import tempfile


def probe(root: Path, profile: str, config: Path) -> None:
    env = {key: value for key, value in os.environ.items() if not key.startswith("FLOCK_")}
    result = subprocess.run([
        "cargo", "nextest", "run", "--manifest-path", str(root / "Cargo.toml"),
        "--config-file", str(config), "--profile", profile,
        "-E", "binary(=ceiling-probe)",
    ], env=env, capture_output=True, text=True)
    output = result.stdout + result.stderr
    print(f"{profile}: exit {result.returncode}\n{output}", flush=True)
    if result.returncode != 100 or "TIMEOUT" not in output or "producer_stays_alive_and_silent" not in output:
        raise RuntimeError(f"{profile}: expected a named timeout failure")


def main() -> None:
    config = Path(__file__).resolve().parents[1] / ".config/nextest.toml"
    with tempfile.TemporaryDirectory(prefix="nextest-ceiling-") as directory:
        root = Path(directory)
        (root / "src").mkdir()
        (root / "src/main.rs").write_text(
            "fn main() {}\n#[test]\nfn producer_stays_alive_and_silent() { "
            "loop { std::thread::park(); } }\n"
        )
        # Nextest validates every binary selector even when running one binary.
        manifest = '[package]\nname="ceiling-probe"\nversion="0.0.0"\nedition="2021"\n[workspace]\n'
        for binary in sorted(set(re.findall(r"binary\(=?([a-z_0-9]+)\)", config.read_text()))):
            manifest += f'\n[[test]]\nname="{binary}"\npath="empty.rs"\n'
        (root / "Cargo.toml").write_text(manifest)
        (root / "empty.rs").write_text("")
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(probe, root, profile, config) for profile in ("default", "ci")]
            for future in futures:
                future.result()


if __name__ == "__main__":
    main()
