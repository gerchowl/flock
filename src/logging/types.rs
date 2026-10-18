//! V1a.0 telemetry types (ADR-0039 §1 and §4).
//!
//! Input metadata has no field capable of carrying a character or paste body.
use crossterm::event::{KeyCode, KeyModifiers as TerminalModifiers};
use serde::Serialize;

/// Closed class of a terminal input event.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InputClass {
    Key,
    Mouse,
    Paste,
    Focus,
    // The canonical schema includes resize, though the byte parser emits none.
    #[allow(dead_code)]
    Resize,
    Unknown,
}

/// Character keys are classified without retaining their codepoint.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyClass {
    Character,
    Named,
    Function,
}

/// Non-content key names. Media and modifier subtypes deliberately collapse.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NamedKey {
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Null,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    KeypadBegin,
    Media,
    Modifier,
    Other,
}

/// Modifiers serialize as an object containing only enabled flags.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct KeyModifiers {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shift: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    control: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    alt: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    super_key: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hyper: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    meta: bool,
}

impl From<TerminalModifiers> for KeyModifiers {
    fn from(value: TerminalModifiers) -> Self {
        Self {
            shift: value.contains(TerminalModifiers::SHIFT),
            control: value.contains(TerminalModifiers::CONTROL),
            alt: value.contains(TerminalModifiers::ALT),
            super_key: value.contains(TerminalModifiers::SUPER),
            hyper: value.contains(TerminalModifiers::HYPER),
            meta: value.contains(TerminalModifiers::META),
        }
    }
}

/// Canonical input field set. Inapplicable fields are absent, never null.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct InputMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    input_len: Option<usize>,
    input_class: InputClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_class: Option<KeyClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    named_key: Option<NamedKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_key: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modifiers: Option<KeyModifiers>,
}

impl InputMetadata {
    /// Classify a key, discarding character values at this boundary.
    pub(crate) fn key(code: KeyCode, modifiers: TerminalModifiers) -> Self {
        let mut result = Self::class(InputClass::Key);
        result.modifiers = Some(modifiers.into());
        match code {
            KeyCode::Char(_) => result.key_class = Some(KeyClass::Character),
            KeyCode::F(number) => {
                result.key_class = Some(KeyClass::Function);
                result.function_key = Some(number);
            }
            code => {
                result.key_class = Some(KeyClass::Named);
                result.named_key = Some(match code {
                    KeyCode::Enter => NamedKey::Enter,
                    KeyCode::Tab => NamedKey::Tab,
                    KeyCode::BackTab => NamedKey::BackTab,
                    KeyCode::Backspace => NamedKey::Backspace,
                    KeyCode::Esc => NamedKey::Esc,
                    KeyCode::Up => NamedKey::Up,
                    KeyCode::Down => NamedKey::Down,
                    KeyCode::Left => NamedKey::Left,
                    KeyCode::Right => NamedKey::Right,
                    KeyCode::Home => NamedKey::Home,
                    KeyCode::End => NamedKey::End,
                    KeyCode::PageUp => NamedKey::PageUp,
                    KeyCode::PageDown => NamedKey::PageDown,
                    KeyCode::Insert => NamedKey::Insert,
                    KeyCode::Delete => NamedKey::Delete,
                    KeyCode::Null => NamedKey::Null,
                    KeyCode::CapsLock => NamedKey::CapsLock,
                    KeyCode::ScrollLock => NamedKey::ScrollLock,
                    KeyCode::NumLock => NamedKey::NumLock,
                    KeyCode::PrintScreen => NamedKey::PrintScreen,
                    KeyCode::Pause => NamedKey::Pause,
                    KeyCode::Menu => NamedKey::Menu,
                    KeyCode::KeypadBegin => NamedKey::KeypadBegin,
                    KeyCode::Media(_) => NamedKey::Media,
                    KeyCode::Modifier(_) => NamedKey::Modifier,
                    _ => NamedKey::Other,
                });
            }
        }
        result
    }

    fn class(input_class: InputClass) -> Self {
        Self {
            input_len: None,
            input_class,
            key_class: None,
            named_key: None,
            function_key: None,
            modifiers: None,
        }
    }

    /// Classify parsed raw input without accepting its content into telemetry.
    pub(crate) fn raw(input_len: usize, event: &crate::raw_input::RawInputEvent) -> Self {
        use crate::raw_input::RawInputEvent;
        let mut result = match event {
            RawInputEvent::Key(key) => Self::key(key.code, key.modifiers),
            RawInputEvent::Mouse(mouse) => {
                let mut result = Self::class(InputClass::Mouse);
                if !mouse.modifiers.is_empty() {
                    result.modifiers = Some(mouse.modifiers.into());
                }
                result
            }
            RawInputEvent::Paste(_) => Self::class(InputClass::Paste),
            RawInputEvent::OuterFocusGained | RawInputEvent::OuterFocusLost => {
                Self::class(InputClass::Focus)
            }
            _ => Self::class(InputClass::Unknown),
        };
        result.input_len = Some(input_len);
        result
    }
}

impl std::fmt::Display for InputMetadata {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let json = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        formatter.write_str(&json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_input_schema_omits_content_and_inapplicable_fields() {
        let character = InputMetadata::key(
            KeyCode::Char('🦩'),
            TerminalModifiers::CONTROL | TerminalModifiers::ALT,
        );
        assert_eq!(
            serde_json::to_value(character).unwrap(),
            json!({
                "input_class":"key", "key_class":"character", "modifiers":{"control":true,"alt":true}
            })
        );
        assert_eq!(
            serde_json::to_value(InputMetadata::key(
                KeyCode::F(5),
                TerminalModifiers::empty()
            ))
            .unwrap(),
            json!({"input_class":"key", "key_class":"function", "function_key":5, "modifiers":{}})
        );
        let paste = crate::raw_input::RawInputEvent::Paste("secret".into());
        assert_eq!(
            serde_json::to_value(InputMetadata::raw(18, &paste)).unwrap(),
            json!({"input_len":18, "input_class":"paste"})
        );
        for (class, name) in [
            (InputClass::Focus, "focus"),
            (InputClass::Mouse, "mouse"),
            (InputClass::Resize, "resize"),
            (InputClass::Unknown, "unknown"),
        ] {
            assert_eq!(
                serde_json::to_value(InputMetadata::class(class)).unwrap(),
                json!({"input_class":name})
            );
        }
        let all = KeyModifiers::from(TerminalModifiers::all());
        assert_eq!(
            serde_json::to_value(all).unwrap(),
            json!({"shift":true,"control":true,
            "alt":true,"super_key":true,"hyper":true,"meta":true})
        );
    }

    #[test]
    fn named_keys_have_only_closed_names() {
        use crossterm::event::{MediaKeyCode, ModifierKeyCode};
        let keys = [
            (KeyCode::Enter, "enter"),
            (KeyCode::Tab, "tab"),
            (KeyCode::BackTab, "back_tab"),
            (KeyCode::Backspace, "backspace"),
            (KeyCode::Esc, "esc"),
            (KeyCode::Up, "up"),
            (KeyCode::Down, "down"),
            (KeyCode::Left, "left"),
            (KeyCode::Right, "right"),
            (KeyCode::Home, "home"),
            (KeyCode::End, "end"),
            (KeyCode::PageUp, "page_up"),
            (KeyCode::PageDown, "page_down"),
            (KeyCode::Insert, "insert"),
            (KeyCode::Delete, "delete"),
            (KeyCode::Null, "null"),
            (KeyCode::CapsLock, "caps_lock"),
            (KeyCode::ScrollLock, "scroll_lock"),
            (KeyCode::NumLock, "num_lock"),
            (KeyCode::PrintScreen, "print_screen"),
            (KeyCode::Pause, "pause"),
            (KeyCode::Menu, "menu"),
            (KeyCode::KeypadBegin, "keypad_begin"),
            (KeyCode::Media(MediaKeyCode::Play), "media"),
            (KeyCode::Modifier(ModifierKeyCode::LeftShift), "modifier"),
        ];
        for (key, name) in keys {
            assert_eq!(
                serde_json::to_value(InputMetadata::key(key, TerminalModifiers::empty())).unwrap(),
                json!({"input_class":"key", "key_class":"named", "named_key":name, "modifiers":{}})
            );
        }
    }
}
