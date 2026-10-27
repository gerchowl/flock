//! Real public start and browser wire through the owned structured dogfood sandbox.
//! The HTTP endpoint exists only in web-enabled builds; both CI legs enable it.
#![cfg(feature = "web")]

mod support;

fn proof(flag: &str) -> serde_json::Value {
    let output = support::environment::Command::new("python3")
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/probes/s7_conformance.py"
        ))
        .args(["--flk", env!("CARGO_BIN_EXE_flk"), flag])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("fixture tools"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("bounded real-server browser proof");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["status"], "passed");
    proof
}

#[test]
fn s7_public_server_browser_dogfood_and_causal_controls() {
    let proof = proof("--verify-controls");
    assert_eq!(proof["causal_controls"].as_array().unwrap().len(), 8);
}

#[test]
fn s7_send_and_answer_without_operate_have_typed_refusals() {
    let proof = proof("--verify-scopes");
    assert_eq!(proof["without_operate"]["send"], "not_authorized");
    assert_eq!(proof["without_operate"]["decide"], "not_authorized");
}
