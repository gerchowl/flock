# Codex version eligibility (#1479 S1)

`version.stdout` scripts the exact `codex-cli 0.160.1` version-line contract.
It is paired with the existing recorded-format session metadata in
`../codex/finished.jsonl`; no new live model or Codex process was used.
`scripts/test_codex_version_pin.py`, discovered by `just script-tests` in CI,
checks both against `codex_version::TESTED_VERSION` and the reviewed CI pin.
A compatibility expansion must update the declaration, fixtures and tests.
The fake executable scenarios in the adjacent Rust tests are scripted failure
injections, not captures of Codex failures. They use the same real subprocess
probe as production; no production-only test hooks or shortened probe ceiling.
