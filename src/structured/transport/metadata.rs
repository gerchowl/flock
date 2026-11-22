//! Bounded correlation metadata for a line whose tool input exceeds the cap.
//! This scanner never authorizes or decodes a harness event. S1 still owns event
//! parsing. It retains only root/request string headers so an oversized tool
//! request can be denied even when request_id follows its large input.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Default)]
struct Frame {
    object: bool,
    key_expected: bool,
    key: Option<String>,
}
#[derive(Default)]
pub(super) struct Line {
    depth: usize,
    frames: [Frame; 2],
    request_object: bool,
    string: bool,
    escaped: bool,
    capture: bool,
    overflow: bool,
    token: Vec<u8>,
    kind: Option<String>,
    id: Option<String>,
    subtype: Option<String>,
    tool: Option<String>,
    tool_id: Option<String>,
    digest: Sha256,
}
impl Line {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.digest.update(bytes);
        for &byte in bytes {
            self.byte(byte);
        }
    }
    fn relevant(&self) -> bool {
        (self.depth == 1 && self.frames[0].object) || (self.depth == 2 && self.request_object)
    }
    fn byte(&mut self, byte: u8) {
        if self.string {
            if self.capture {
                if self.token.len() < 2048 {
                    self.token.push(byte);
                } else {
                    self.overflow = true;
                }
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.string = false;
                if self.capture {
                    self.finish_string();
                }
            }
            return;
        }
        match byte {
            b'"' => {
                self.string = true;
                self.escaped = false;
                self.overflow = false;
                self.token.clear();
                self.capture = self.relevant();
                if self.capture {
                    self.token.push(byte);
                }
            }
            b'{' | b'[' => {
                if self.depth == 1 {
                    self.request_object = byte == b'{'
                        && self.frames[0].object
                        && self.frames[0].key.as_deref() == Some("request");
                }
                self.depth = self.depth.saturating_add(1);
                if self.depth <= 2 {
                    self.frames[self.depth - 1] = Frame {
                        object: byte == b'{',
                        key_expected: true,
                        key: None,
                    };
                }
            }
            b'}' | b']' => {
                if self.depth == 2 {
                    self.request_object = false;
                }
                self.depth = self.depth.saturating_sub(1);
            }
            b':' if self.relevant() => self.frames[self.depth - 1].key_expected = false,
            b',' if self.relevant() => {
                let frame = &mut self.frames[self.depth - 1];
                frame.key_expected = true;
                frame.key = None;
            }
            _ => (),
        }
    }
    fn finish_string(&mut self) {
        let value = if self.overflow {
            None
        } else {
            serde_json::from_slice::<String>(&self.token)
                .ok()
                .filter(|v| v.len() <= 256)
        };
        let frame = &mut self.frames[self.depth - 1];
        if frame.key_expected {
            frame.key = value;
            match (self.depth, frame.key.as_deref()) {
                (1, Some("type")) => self.kind = None,
                (1, Some("request_id")) => self.id = None,
                (1, Some("request")) => {
                    self.subtype = None;
                    self.tool = None;
                    self.tool_id = None;
                }
                (2, Some("subtype")) => self.subtype = None,
                (2, Some("tool_name")) => self.tool = None,
                (2, Some("tool_use_id")) => self.tool_id = None,
                _ => (),
            }
        } else {
            match (self.depth, frame.key.as_deref()) {
                (1, Some("type")) => self.kind = value,
                (1, Some("request_id")) => self.id = value,
                (2, Some("subtype")) => self.subtype = value,
                (2, Some("tool_name")) => self.tool = value,
                (2, Some("tool_use_id")) => self.tool_id = value,
                _ => (),
            }
        }
    }
    pub fn oversized_request(
        self,
        bytes: u64,
    ) -> io::Result<Option<(String, super::super::claude::ToolRequest)>> {
        if self.kind.as_deref() != Some("control_request")
            || self.subtype.as_deref() != Some("can_use_tool")
        {
            return Ok(None);
        }
        let Some(id) = self.id.filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        let hash = self
            .digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let request = serde_json::from_value(serde_json::json!({
            "subtype":"can_use_tool","tool_name":self.tool.unwrap_or_else(||"truncated".into()),
            "tool_use_id":self.tool_id.unwrap_or_else(||"truncated".into()),
            "input":{"truncated":true,"wire_bytes":bytes,"wire_sha256":hash}
        }))?;
        Ok(Some((id, request)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_headers_survive_split_escapes_and_input_before_id() {
        let mut line = Line::default();
        let raw=br#"{"request":{"input":{"x":"not-a-header","request_id":"spoof"},"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example"},"request_id":"request-\u0061","type":"control_request"}"#;
        for byte in raw {
            line.feed(&[*byte]);
        }
        assert!(line.token.capacity() <= 4096);
        let (id, request) = line.oversized_request(raw.len() as u64).unwrap().unwrap();
        assert_eq!(id, "request-a");
        assert_eq!(request.tool_name, "Bash");
        assert_eq!(
            request.input["wire_sha256"],
            Sha256::digest(raw)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
    }
    #[test]
    fn nested_text_arrays_and_unbounded_header_strings_do_not_correlate() {
        for raw in [
            serde_json::json!({"type":"assistant","request":{"subtype":"can_use_tool"},"request_id":"spoof"}),
            serde_json::json!({"type":"control_request","request":[{"subtype":"can_use_tool"}],"request_id":"spoof"}),
            serde_json::json!({"type":"control_request","request":{"subtype":"can_use_tool"},"request_id":"x".repeat(4096)}),
        ] {
            let mut line = Line::default();
            line.feed(raw.to_string().as_bytes());
            assert!(line.oversized_request(9999).unwrap().is_none());
        }
    }
}
