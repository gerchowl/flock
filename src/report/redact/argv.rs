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
    rule: ValueRule,
}

#[derive(Clone, Copy)]
enum ValueRule {
    Always,
    User,
    Cookie,
    Public,
}

impl OptionValue {
    /// Ambiguous aliases carry secrets only when their value has credential
    /// syntax. This works through wrappers and aliases without naming a program.
    pub(crate) fn masks(self, value: &str) -> bool {
        match self.rule {
            ValueRule::Always => true,
            ValueRule::User => value.contains(':') || value.starts_with('@'),
            ValueRule::Cookie => value.contains('=') || value.starts_with('@'),
            ValueRule::Public => false,
        }
    }
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
    let rule = match flag {
        "--user" | "--proxy-user" => Some(ValueRule::User),
        "--cookie" => Some(ValueRule::Cookie),
        "--oauth2-bearer" => Some(ValueRule::Always),
        _ => None,
    };
    if let Some(rule) = rule {
        return Some(OptionValue {
            attached,
            rule: if curl { ValueRule::Always } else { rule },
        });
    }
    if named_secret_flag(flag) {
        return Some(OptionValue {
            attached,
            rule: ValueRule::Always,
        });
    }
    if !arg.starts_with('-') || arg.starts_with("--") {
        return None;
    }
    // Non-value switches may precede an argument-taking switch in one word.
    const NO_VALUE: &str = "012346aBfgGiIjJklLMnNOpqRsSvVZ#:";
    const PUBLIC_VALUE: &str = "AcCdDeEFHhKmoPQrTtwXxYyz";
    for (offset, letter) in arg[1..].char_indices() {
        if PUBLIC_VALUE.contains(letter) || CURL_SECRET_SHORT_OPTIONS.contains(letter) {
            let mut start = 1 + offset + letter.len_utf8();
            // Accept both attached spellings: -uuser:pass and -u=user:pass.
            if arg[start..].starts_with('=') {
                start += 1;
            }
            let rule = match letter {
                'u' | 'U' => {
                    if curl {
                        ValueRule::Always
                    } else {
                        ValueRule::User
                    }
                }
                'b' => {
                    if curl {
                        ValueRule::Always
                    } else {
                        ValueRule::Cookie
                    }
                }
                _ => ValueRule::Public,
            };
            return Some(OptionValue {
                attached: (start < arg.len()).then_some(start),
                rule,
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

/// Apply credential value shapes to every command word, independently of the
/// executable. Curl recognition only adds coverage for its unambiguous options.
pub(crate) fn mask_options(text: &str) -> String {
    mask_options_inner(text, true)
}

/// Quoted remote commands and nested shell scripts need the same shape rules.
/// Do not infer an executable's semantics from arbitrary diagnostic text.
pub(crate) fn mask_nested_options(text: &str) -> String {
    mask_options_inner(text, false)
}

fn mask_options_inner(text: &str, curl_coverage: bool) -> String {
    let mut output = String::new();
    let mut copied = 0;
    let mut command_start = true;
    let mut curl = false;
    let mut pending: Option<OptionValue> = None;
    let mut options = true;
    for word in words(text) {
        if word.boundary {
            command_start = true;
            curl = false;
            pending = None;
            options = true;
        }
        let option = option_value(&word.value, curl && options);
        let replacement = if let Some(previous) = pending.take() {
            if previous.masks(&word.value) {
                // Preserve masking through a chain of named secret switches.
                pending = option.filter(|value| value.attached.is_none());
                Some("<redacted>".to_owned())
            } else {
                None
            }
        } else if let Some(option) = option {
            if let Some(start) = option.attached {
                option
                    .masks(&word.value[start..])
                    .then(|| format!("{}<redacted>", &word.value[..start]))
            } else {
                pending = Some(option);
                None
            }
        } else {
            if command_start && !word.value.starts_with('-') && !word.value.contains('=') {
                curl = curl_coverage && is_curl(&word.value);
                command_start = false;
            }
            if word.value == "--" {
                options = false;
            }
            None
        };
        let replacement = replacement.or_else(|| {
            if word.value.chars().any(char::is_whitespace) {
                let nested = mask_nested_options(&word.value);
                (nested != word.value).then_some(nested)
            } else {
                None
            }
        });
        if let Some(replacement) = replacement {
            output.push_str(&text[copied..word.span.start]);
            output.push_str(&replacement);
            copied = word.span.end;
        }
    }
    output.push_str(&text[copied..]);
    output
}
