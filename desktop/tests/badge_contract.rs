// Exercise the production native dispatcher with a recording Tauri window.
extern crate self as tauri;

use std::cell::RefCell;

#[path = "../src/platform/badge.rs"]
mod badge;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Debug, PartialEq)]
enum Call {
    Count(Option<i64>),
    Label(Option<String>),
}

#[derive(Default)]
pub struct Window {
    calls: RefCell<Vec<Call>>,
}

impl Window {
    pub fn set_badge_count(&self, count: Option<i64>) -> Result<()> {
        self.calls.borrow_mut().push(Call::Count(count));
        Ok(())
    }

    pub fn set_badge_label(&self, label: Option<String>) -> Result<()> {
        self.calls.borrow_mut().push(Call::Label(label));
        Ok(())
    }
}

fn assert_cleared(window: &Window) {
    let mut expected = Vec::new();
    if cfg!(target_os = "macos") {
        expected.push(Call::Label(None));
    }
    expected.push(Call::Count(None));
    assert_eq!(*window.calls.borrow(), expected);
}

#[test]
fn clear_passes_none_to_native_setters() {
    let window = Window::default();
    badge::set(&window, 0, None).expect("clear succeeds");
    assert_cleared(&window);
}

#[test]
fn partial_and_unknown_labels_transition_to_known_zero() {
    for (count, label) in [(2, "2+"), (0, "0+"), (0, "?")] {
        let window = Window::default();
        badge::set(&window, count, Some(label.into())).expect("badge succeeds");
        let expected = if cfg!(target_os = "macos") {
            Call::Label(Some(label.into()))
        } else {
            Call::Count((count > 0).then_some(i64::from(count)))
        };
        assert_eq!(*window.calls.borrow(), [expected]);
        window.calls.borrow_mut().clear();
        badge::set(&window, 0, None).expect("clear succeeds");
        assert_cleared(&window);
    }
}
