//! Zeroizing JSON ownership for local credential envelopes (ADR-0036 §1a).
use serde::Serialize;
use std::io::{self, Write};
use zeroize::{Zeroize, Zeroizing};

/// Serialize without freeing secret-bearing growth buffers before scrubbing them.
pub(super) fn encode(value: &impl Serialize) -> serde_json::Result<Zeroizing<Vec<u8>>> {
    let mut buffer = Buffer::default();
    serde_json::to_writer(&mut buffer, value)?;
    Ok(buffer.0)
}

/// Growth scrubs the previous allocation before it can be freed.
#[derive(Default)]
pub(super) struct Buffer(pub(super) Zeroizing<Vec<u8>>);
impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.capacity() - self.0.len() < bytes.len() {
            let mut next = Zeroizing::new(Vec::with_capacity(
                (self.0.len() + bytes.len()).max(self.0.capacity().saturating_mul(2)),
            ));
            next.extend_from_slice(&self.0);
            self.0 = next;
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Parsed envelope remains owned until every credential string has been scrubbed.
pub(super) struct SecretValue(pub(super) serde_json::Value);
impl SecretValue {
    /// Parse without transferring ownership to consuming schema deserialization.
    pub(super) fn parse(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line).map(Self)
    }
}
impl Drop for SecretValue {
    fn drop(&mut self) {
        fn scrub(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::String(text) => text.zeroize(),
                serde_json::Value::Array(values) => values.iter_mut().for_each(scrub),
                serde_json::Value::Object(values) => {
                    for (mut key, mut value) in std::mem::take(values) {
                        key.zeroize();
                        scrub(&mut value);
                    }
                }
                _ => {}
            }
        }
        scrub(&mut self.0);
    }
}
