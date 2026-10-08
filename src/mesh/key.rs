//! Message identity is server-minted, never a caller correlation id.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageKey {
    pub origin_node: String,
    pub message_id: String,
}

impl MessageKey {
    /// ULID's 48-bit Unix millisecond timestamp and 80 bits of OS entropy.
    /// Persist the returned key once and reuse it on transport retries.
    pub fn mint(origin_node: String, unix_ms: u64) -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes[6..])?;
        bytes[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
        let mut value = u128::from_be_bytes(bytes);
        let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut encoded = ['0'; 26];
        for digit in encoded.iter_mut().rev() {
            *digit = alphabet[(value & 31) as usize] as char;
            value >>= 5;
        }
        Ok(Self {
            origin_node,
            message_id: encoded.iter().collect(),
        })
    }

    pub fn is_valid(&self) -> bool {
        !self.origin_node.is_empty()
            && self.message_id.len() == 26
            && self.message_id.as_bytes()[0] <= b'7'
            && self
                .message_id
                .bytes()
                .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
    }
}
