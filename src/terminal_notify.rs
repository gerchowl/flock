use std::io::{self, Write as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalNotificationBackend {
    Ghostty,
    Iterm2,
    Kitty,
    WezTerm,
}

pub fn detect_backend() -> Option<TerminalNotificationBackend> {
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let term = std::env::var("TERM").ok();

    match term_program.as_deref() {
        Some("ghostty") => return Some(TerminalNotificationBackend::Ghostty),
        Some("iTerm.app") => return Some(TerminalNotificationBackend::Iterm2),
        Some("WezTerm") => return Some(TerminalNotificationBackend::WezTerm),
        _ => {}
    }

    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return Some(TerminalNotificationBackend::Kitty);
    }

    match term.as_deref() {
        Some("xterm-ghostty") => Some(TerminalNotificationBackend::Ghostty),
        Some("xterm-kitty") => Some(TerminalNotificationBackend::Kitty),
        Some(term) if term.contains("wezterm") => Some(TerminalNotificationBackend::WezTerm),
        _ => None,
    }
}

pub fn show_notification(title: &str, body: Option<&str>) -> io::Result<bool> {
    let Some(backend) = detect_backend() else {
        return Ok(false);
    };

    let sequence = match backend {
        TerminalNotificationBackend::Ghostty
        | TerminalNotificationBackend::Iterm2
        | TerminalNotificationBackend::WezTerm => build_osc9_notification(title, body),
        TerminalNotificationBackend::Kitty => build_osc99_notification(title, body),
    };

    let sequence = if std::env::var_os("TMUX").is_some() {
        wrap_tmux_passthrough(&sequence)
    } else {
        sequence
    };

    let mut stdout = io::stdout();
    stdout.write_all(&sequence)?;
    stdout.flush()?;
    Ok(true)
}

pub fn split_message(message: &str) -> (&str, Option<&str>) {
    match message.split_once(": ") {
        Some((title, body)) if !title.is_empty() && !body.is_empty() => (title, Some(body)),
        _ => (message, None),
    }
}

fn build_osc9_notification(title: &str, body: Option<&str>) -> Vec<u8> {
    let message = sanitize_text(match body {
        Some(body) if !body.is_empty() => format!("{title}: {body}"),
        _ => title.to_string(),
    });
    format!("\x1b]9;{message}\x1b\\").into_bytes()
}

fn build_osc99_notification(title: &str, body: Option<&str>) -> Vec<u8> {
    let title = sanitize_text(title);
    match body {
        Some(body) if !body.is_empty() => {
            let body = sanitize_text(body);
            format!("\x1b]99;i=1:d=0;{title}\x1b\\\x1b]99;i=1:p=body;{body}\x1b\\").into_bytes()
        }
        _ => format!("\x1b]99;;{title}\x1b\\").into_bytes(),
    }
}

pub(crate) fn sanitize_text(text: impl AsRef<str>) -> String {
    text.as_ref()
        .chars()
        .filter(|ch| *ch != '\u{1b}' && *ch != '\u{7}' && *ch != '\u{9c}')
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            _ => ch,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Host window title (#361)
// ---------------------------------------------------------------------------

/// Save the host terminal's window title on its title stack (XTerm `CSI 22;0t`).
/// Paired with [`POP_WINDOW_TITLE`] so a flock session hands the title back on
/// the way out. Unsupported in some terminals, where it silently no-ops — which
/// is why the exit path ALSO writes an empty title.
pub(crate) const PUSH_WINDOW_TITLE: &[u8] = b"\x1b[22;0t";

/// Restore the host terminal's window title from its title stack
/// (XTerm `CSI 23;0t`).
pub(crate) const POP_WINDOW_TITLE: &[u8] = b"\x1b[23;0t";

/// OSC 2 — set the WINDOW title only.
///
/// Deliberately not OSC 0: that also sets the icon name, which some tiling
/// window managers still match windows on, so a live-updating icon name would
/// break their rules. Alacritty (and every terminal flock is used on) honours
/// OSC 2 whenever dynamic titles are enabled, which is the default.
pub(crate) fn set_window_title_sequence(title: &str) -> Vec<u8> {
    format!("\x1b]2;{}\x1b\\", sanitize_text(title)).into_bytes()
}

/// The bytes that hand the window title back to whoever owned it before flock:
/// an empty OSC 2 AND a title-stack pop. Both, on purpose — the stack is
/// unsupported in some terminals and silently no-ops there, and the empty write
/// is the only thing that lands; where the stack DOES work, the pop puts the
/// shell's own title back and the empty write in front of it is invisible.
pub(crate) fn restore_window_title_sequence() -> Vec<u8> {
    let mut bytes = set_window_title_sequence("");
    bytes.extend_from_slice(POP_WINDOW_TITLE);
    bytes
}

/// Writes host-terminal control bytes to stdout, wrapping them in tmux's DCS
/// passthrough when running inside tmux.
///
/// Without `allow-passthrough on`, tmux swallows the sequence and the host
/// title stays exactly as it is today — no worse than not publishing at all.
pub(crate) fn write_host_sequence(sequence: &[u8]) -> io::Result<()> {
    let sequence = if std::env::var_os("TMUX").is_some() {
        wrap_tmux_passthrough(sequence)
    } else {
        sequence.to_vec()
    };
    let mut stdout = io::stdout();
    stdout.write_all(&sequence)?;
    stdout.flush()
}

pub(crate) fn wrap_tmux_passthrough(sequence: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(sequence.len() + 16);
    wrapped.extend_from_slice(b"\x1bPtmux;");
    for &byte in sequence {
        if byte == 0x1b {
            wrapped.push(0x1b);
        }
        wrapped.push(byte);
    }
    wrapped.extend_from_slice(b"\x1b\\");
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_message_splits_title_and_body() {
        assert_eq!(
            split_message("agent done: ws · 1"),
            ("agent done", Some("ws · 1"))
        );
    }

    #[test]
    fn split_message_leaves_plain_message_alone() {
        assert_eq!(split_message("agent done"), ("agent done", None));
    }

    #[test]
    fn sanitize_text_strips_control_bytes() {
        assert_eq!(sanitize_text("a\n\tb\u{1b}c\u{7}"), "a  bc");
    }

    #[test]
    fn kitty_notification_uses_structured_title_and_body() {
        let sequence = String::from_utf8(build_osc99_notification("pi finished", Some("ws · 1")))
            .expect("utf8");
        assert!(sequence.contains("]99;i=1:d=0;pi finished"));
        assert!(sequence.contains("]99;i=1:p=body;ws · 1"));
    }

    #[test]
    fn window_title_sequence_is_osc_2_not_osc_0() {
        // OSC 0 would also set the ICON name, which some tiling WMs match
        // windows on.
        assert_eq!(
            set_window_title_sequence("main \u{00b7} mba22 \u{2014} flk"),
            b"\x1b]2;main \xc2\xb7 mba22 \xe2\x80\x94 flk\x1b\\".to_vec()
        );
    }

    #[test]
    fn window_title_sequence_strips_a_sequence_terminator_from_the_payload() {
        let bytes = set_window_title_sequence("a\u{7}b\u{1b}c");
        assert_eq!(bytes, b"\x1b]2;abc\x1b\\".to_vec());
    }

    #[test]
    fn restore_window_title_writes_an_empty_title_and_pops_the_stack() {
        // Both, because the stack silently no-ops on terminals that lack it.
        assert_eq!(
            restore_window_title_sequence(),
            b"\x1b]2;\x1b\\\x1b[23;0t".to_vec()
        );
    }

    #[test]
    fn tmux_passthrough_wraps_and_escapes() {
        let wrapped = wrap_tmux_passthrough(b"\x1b]9;hi\x1b\\");
        assert_eq!(wrapped, b"\x1bPtmux;\x1b\x1b]9;hi\x1b\x1b\\\x1b\\");
    }
}
