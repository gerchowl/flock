//! Real public start and browser wire through the owned structured dogfood sandbox.
//! The HTTP endpoint exists only in web-enabled builds; both CI legs enable it.
#![cfg(feature = "web")]

#[test]
#[allow(clippy::disallowed_methods)] // The bounded Python supervisor supplies the full isolated environment.
fn s7_public_server_browser_dogfood_and_causal_controls() {
    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/probes/s7_conformance.py"
        ))
        .args(["--flk", env!("CARGO_BIN_EXE_flk"), "--verify-controls"])
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
    assert_eq!(proof["causal_controls"].as_array().unwrap().len(), 8);
}
