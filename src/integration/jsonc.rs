//! Read-only JSONC inventory. Writers retain commented files for their owner.
use std::io;

pub(super) fn parse(raw: &str) -> io::Result<serde_json::Value> {
    let mut chars = raw.chars().peekable();
    let mut text = String::with_capacity(raw.len());
    let mut quoted = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if quoted {
            text.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
            text.push(ch);
        } else if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\n' {
                    break;
                }
            }
            text.push('\n');
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(ch) = chars.next() {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(io::Error::other("unterminated JSONC comment"));
            }
            text.push(' ');
        } else {
            text.push(ch);
        }
    }
    let mut result = String::with_capacity(text.len());
    quoted = false;
    escaped = false;
    for (index, ch) in text.char_indices() {
        if !quoted && ch == ',' && text[index + 1..].trim_start().starts_with(['}', ']']) {
            continue;
        }
        result.push(ch);
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        }
    }
    serde_json::from_str(&result).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    #[test]
    fn jsonc_inventory_preserves_strings_and_accepts_comments_and_trailing_commas() {
        let parsed = super::parse(
            r#"{
            // This pin is only a comment: /nix/store/comment/bin/flk
            "url": "https://example.com/*text*/",
            "quoted": "a\"b,}",
            "list": ["mcp", "serve",], /* keep the real command */
            "command": "/nix/store/fixture-flock/bin/flk",
        }"#,
        )
        .unwrap();
        assert_eq!(parsed["url"], "https://example.com/*text*/");
        assert_eq!(parsed["quoted"], "a\"b,}");
        assert_eq!(parsed["list"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["command"], "/nix/store/fixture-flock/bin/flk");
        assert!(super::parse("{/* unclosed").is_err());
    }
}
