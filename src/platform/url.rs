//! Browser-launch policy for pane-derived links (#234).
use std::{fmt, io};

/// Why a link was refused before any platform opener was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRefusal {
    /// A leading dash could be interpreted as an opener option.
    LeadingOption,
    /// Raw whitespace or control characters, including NUL, are forbidden.
    UnsafeCharacter,
    /// Only absolute HTTP and HTTPS links are eligible.
    Scheme,
    /// An absolute web link must contain an authority.
    MissingAuthority,
}
impl fmt::Display for UrlRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::LeadingOption => "link cannot begin with an option",
            Self::UnsafeCharacter => "link contains whitespace or control characters",
            Self::Scheme => "only http:// and https:// links can be opened",
            Self::MissingAuthority => "link must include a host",
        })
    }
}
impl std::error::Error for UrlRefusal {}

/// A refused link or a failure to start the system opener.
#[derive(Debug)]
pub enum OpenUrlError {
    /// Policy refusal, before spawning anything.
    Refused(UrlRefusal),
    /// The opener could not be started. Later navigation belongs to the OS.
    Launch(io::Error),
}
impl fmt::Display for OpenUrlError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(reason) => reason.fmt(out),
            Self::Launch(error) => write!(out, "could not start browser opener: {error}"),
        }
    }
}
impl std::error::Error for OpenUrlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Refused(reason) => reason,
            Self::Launch(error) => error,
        })
    }
}

/// Enforce #234's scheme/raw-character gate without normalizing the input.
/// There is no `url` dependency. Complete URL interpretation belongs to the
/// system opener; encoded spaces remain valid, and raw whitespace does not.
pub fn validate_url(url: &str) -> Result<(), UrlRefusal> {
    if url.starts_with('-') {
        return Err(UrlRefusal::LeadingOption);
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(UrlRefusal::UnsafeCharacter);
    }
    let (scheme, rest) = url.split_once("://").ok_or(UrlRefusal::Scheme)?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(UrlRefusal::Scheme);
    }
    if rest.split(['/', '?', '#']).next().is_none_or(str::is_empty) {
        return Err(UrlRefusal::MissingAuthority);
    }
    Ok(())
}

/// Validate before starting the platform opener. Navigation remains asynchronous.
pub fn open_url(url: &str) -> Result<(), OpenUrlError> {
    open_with(url, super::open_validated_url)
}
fn open_with(url: &str, launch: impl FnOnce(&str) -> io::Result<()>) -> Result<(), OpenUrlError> {
    validate_url(url).map_err(OpenUrlError::Refused)?;
    launch(url).map_err(OpenUrlError::Launch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    macro_rules! refuses {
        ($name:ident, $url:expr, $reason:ident) => {
            #[test]
            fn $name() {
                let called = Cell::new(false);
                let result = open_with($url, |_| {
                    called.set(true);
                    Ok(())
                });
                assert!(matches!(
                    result,
                    Err(OpenUrlError::Refused(UrlRefusal::$reason))
                ));
                assert!(!called.get(), "refused URL reached the opener");
            }
        };
    }
    refuses!(leading_option, "-a", LeadingOption);
    refuses!(file_scheme, "file://example.com/document", Scheme);
    refuses!(javascript_scheme, "javascript:alert(1)", Scheme);
    refuses!(man_page_scheme, "x-man-page:open", Scheme);
    refuses!(
        embedded_newline,
        "https://example.com/a\nb",
        UnsafeCharacter
    );
    refuses!(embedded_space, "https://example.com/a b", UnsafeCharacter);
    refuses!(embedded_nul, "https://example.com/a\0b", UnsafeCharacter);
    refuses!(embedded_tab, "https://example.com/a\tb", UnsafeCharacter);
    refuses!(
        unicode_whitespace,
        "https://example.com/a\u{a0}b",
        UnsafeCharacter
    );
    refuses!(
        control_character,
        "https://example.com/a\u{7f}b",
        UnsafeCharacter
    );
    refuses!(empty_authority, "https:///document", MissingAuthority);
    #[test]
    fn valid_web_urls_reach_the_opener_unchanged() {
        for url in [
            "https://example.com/path?q=two%20words",
            "http://example.com/",
            "HTTPS://example.com/",
        ] {
            let called = Cell::new(false);
            open_with(url, |actual| {
                assert_eq!(actual, url);
                called.set(true);
                Ok(())
            })
            .unwrap();
            assert!(called.get());
        }
    }
    #[test]
    fn launch_error_remains_distinct_from_policy_refusal() {
        assert!(matches!(
            open_with("https://example.com/", |_| Err(
                io::ErrorKind::NotFound.into()
            )),
            Err(OpenUrlError::Launch(_))
        ));
    }
}
