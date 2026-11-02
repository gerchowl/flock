//! Shared option classification for argv and shell command text.

use std::ops::Range;
use std::sync::OnceLock;

use regex::Regex;

const CURL_SECRET_LONG_OPTIONS: &[&str] =
    &["--user", "--cookie", "--proxy-user", "--oauth2-bearer"];
const CURL_SECRET_SHORT_OPTIONS: &str = "uUb";

/// An option's value is attached at this byte offset, or occupies the next word.
#[derive(Clone, Copy)]
pub(crate) struct OptionValue {
    pub(crate) attached: Option<usize>,
    pub(crate) secret: bool,
}

pub(crate) fn is_curl(program: &str) -> bool {
    std::path::Path::new(program)
        .file_name()
        .is_some_and(|name| name == "curl")
}

pub(crate) fn is_shell(program: &str) -> bool {
    std::path::Path::new(program)
        .file_name()
        .is_some_and(|name| matches!(name.to_str(), Some("sh" | "bash" | "dash" | "ksh" | "zsh")))
}

/// Named flags match whole segments. Bare key paths and boolean switches do not
/// carry credential values, while qualified keys such as api-key do.
pub(crate) fn named_secret_flag(flag: &str) -> bool {
    let Some(name) = flag.strip_prefix("--").or_else(|| flag.strip_prefix('-')) else {
        return false;
    };
    if CURL_SECRET_LONG_OPTIONS.contains(&flag) {
        return false;
    }
    if !name
        .chars()
        .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return false;
    }
    let name = name.to_ascii_lowercase();
    let parts: Vec<_> = name.split(['-', '_']).collect();
    if parts
        .first()
        .is_some_and(|part| matches!(*part, "no" | "disable" | "enable" | "show"))
    {
        return false;
    }
    static WORD: OnceLock<Regex> = OnceLock::new();
    let word = WORD.get_or_init(|| {
        Regex::new(&format!("^{}$", super::secret_label_pattern("")))
            .expect("static credential segment pattern")
    });
    parts
        .iter()
        .any(|part| *part != "key" && word.is_match(part))
        || name == "key"
        || parts
            .windows(2)
            .any(|pair| super::QUALIFIED_KEY_PREFIXES.contains(&pair[0]) && pair[1] == "key")
}

/// Curl short options are case-sensitive. A cluster stops at its FIRST
/// argument-taking letter, including public options such as A, o and H.
/// The inventory follows the curl man page's short option table.
pub(crate) fn option_value(arg: &str, curl: bool) -> Option<OptionValue> {
    let (flag, attached) = arg
        .split_once('=')
        .map_or((arg, None), |(flag, _)| (flag, Some(flag.len() + 1)));
    if CURL_SECRET_LONG_OPTIONS.contains(&flag) {
        return curl.then_some(OptionValue {
            attached,
            secret: true,
        });
    }
    if !curl || !arg.starts_with('-') || arg.starts_with("--") {
        return named_secret_flag(flag).then_some(OptionValue {
            attached,
            secret: true,
        });
    }
    // Non-value switches may precede an argument-taking switch in one word.
    const NO_VALUE: &str = "012346aBfgGiIjJklLMnNOpqRsSvVZ#:";
    const PUBLIC_VALUE: &str = "AcCdDeEFHhKmoPQrTtwXxYyz";
    for (offset, letter) in arg[1..].char_indices() {
        if PUBLIC_VALUE.contains(letter) || CURL_SECRET_SHORT_OPTIONS.contains(letter) {
            let start = 1 + offset + letter.len_utf8();
            return Some(OptionValue {
                attached: (start < arg.len()).then_some(start),
                secret: CURL_SECRET_SHORT_OPTIONS.contains(letter),
            });
        }
        if !NO_VALUE.contains(letter) {
            return None;
        }
    }
    None
}

struct Word {
    span: Range<usize>,
    value: String,
    boundary: bool,
}

/// Retain raw byte spans while decoding just quotes and backslash escapes for
/// option recognition. Escaped whitespace belongs to the current word.
fn words(text: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut boundary = true;
    while let Some(&(start, ch)) = chars.peek() {
        if ch.is_whitespace() || ";|&()".contains(ch) {
            boundary |= ch == '\n' || ";|&()".contains(ch);
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut quote = None;
        let mut end = start;
        while let Some(&(index, ch)) = chars.peek() {
            if quote.is_none() && (ch.is_whitespace() || ";|&()".contains(ch)) {
                break;
            }
            chars.next();
            end = index + ch.len_utf8();
            if ch == '\\' && quote != Some('\'') {
                if let Some((index, escaped)) = chars.next() {
                    value.push(escaped);
                    end = index + escaped.len_utf8();
                } else {
                    value.push(ch);
                }
            } else if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() && matches!(ch, '\'' | '"') {
                quote = Some(ch);
            } else {
                value.push(ch);
            }
        }
        words.push(Word {
            span: start..end,
            value,
            boundary,
        });
        boundary = false;
    }
    words
}

/// Only curl's command words enable its credential aliases. Other programs keep
/// their -u/-b/-U diagnostics, including when they occur in shell command text.
pub(crate) fn mask_options(text: &str) -> String {
    let mut output = String::new();
    let mut copied = 0;
    let mut command_start = true;
    let mut curl = false;
    let mut pending = None;
    let mut options = true;
    for word in words(text) {
        if word.boundary {
            command_start = true;
            curl = false;
            pending = None;
            options = true;
        }
        let option = options.then(|| option_value(&word.value, curl)).flatten();
        let replacement = if let Some(secret) = pending.take() {
            if secret {
                // Preserve masking through a chain of named secret switches.
                pending = option
                    .filter(|value| value.attached.is_none())
                    .map(|value| value.secret);
                Some("<redacted>".to_owned())
            } else {
                None
            }
        } else if let Some(option) = option {
            if let Some(start) = option.attached {
                option
                    .secret
                    .then(|| format!("{}<redacted>", &word.value[..start]))
            } else {
                pending = Some(option.secret);
                None
            }
        } else {
            if command_start && !word.value.starts_with('-') && !word.value.contains('=') {
                curl = is_curl(&word.value);
                command_start = false;
            }
            if word.value == "--" {
                options = false;
            }
            None
        };
        if let Some(replacement) = replacement {
            output.push_str(&text[copied..word.span.start]);
            output.push_str(&replacement);
            copied = word.span.end;
        }
    }
    output.push_str(&text[copied..]);
    output
}
