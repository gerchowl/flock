use std::io::Write;
use std::os::unix::net::UnixStream;

// Reply to the CLI's version probe without counting it as the command under test.
pub fn answer_probe(stream: &mut UnixStream, line: &str) -> bool {
    let request: serde_json::Value = serde_json::from_str(line).unwrap();
    if request["method"] != "ping" {
        return false;
    }
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": request["id"],
            "result": {"type":"pong", "version":"fixture", "protocol":27}
        })
    )
    .unwrap();
    true
}
