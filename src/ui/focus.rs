//! Keyboard focus for modal dialogs (#326).
//!
//! Flock's overlays are mouse-first, which had become mouse-ONLY: buttons were
//! clickable, nothing tracked focus, and each dialog's key handler recognised a
//! fixed pair — on the kill dialog, `Enter` always meant the destructive
//! button, with no way to reach `cancel` from the keyboard at all.
//!
//! The model here is deliberately small. A dialog names its controls as an
//! ordered slice of a plain `Copy` enum, in the order they read on screen, and
//! this module moves between them. Nothing here knows what a control DOES —
//! activation stays in the dialog's own handler, where the state it mutates
//! lives. That keeps the shared piece to the part every dialog would otherwise
//! reimplement, and leaves per-dialog behaviour per-dialog.
//!
//! Storing focus as the control's own enum (not an index) matters: the kill
//! dialog's control list GROWS when its probe lands (#325 adds the force
//! toggle), and an index would silently point at a different control the moment
//! that happened.

/// Step to the next/previous control, wrapping at both ends.
///
/// `delta` is a direction, not a distance: any positive value moves forward.
/// A `current` that is not in `controls` — the control it named has gone away,
/// which is exactly what happens when a dialog's list shrinks — restarts at the
/// first control rather than leaving focus nowhere.
pub(crate) fn step<T: Copy + PartialEq>(controls: &[T], current: T, delta: isize) -> T {
    if controls.is_empty() {
        return current;
    }
    let Some(idx) = controls.iter().position(|control| *control == current) else {
        return controls[0];
    };
    let len = controls.len() as isize;
    let next = (idx as isize + delta.signum()).rem_euclid(len);
    controls[next as usize]
}

/// Clamp focus onto a control that still exists, for when a dialog's list
/// changes underneath it. Returns `fallback` when `current` has gone.
pub(crate) fn resolve<T: Copy + PartialEq>(controls: &[T], current: T, fallback: T) -> T {
    if controls.contains(&current) {
        current
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Control {
        Force,
        Remove,
        Cancel,
    }

    #[test]
    fn step_wraps_in_both_directions() {
        let all = [Control::Force, Control::Remove, Control::Cancel];
        assert_eq!(step(&all, Control::Force, 1), Control::Remove);
        assert_eq!(step(&all, Control::Cancel, 1), Control::Force);
        assert_eq!(step(&all, Control::Force, -1), Control::Cancel);
        assert_eq!(step(&all, Control::Remove, -1), Control::Force);
    }

    #[test]
    fn step_treats_delta_as_a_direction_not_a_distance() {
        // Callers pass whatever their key mapping produced; two keys meaning
        // "forward" must not move two controls.
        let all = [Control::Force, Control::Remove, Control::Cancel];
        assert_eq!(step(&all, Control::Force, 7), Control::Remove);
        assert_eq!(step(&all, Control::Force, -7), Control::Cancel);
    }

    #[test]
    fn a_control_that_no_longer_exists_lands_somewhere_real() {
        // The kill dialog's list grows when its probe lands and could shrink
        // again; focus must never be left pointing at nothing.
        let shrunk = [Control::Remove, Control::Cancel];
        assert_eq!(step(&shrunk, Control::Force, 1), Control::Remove);
        assert_eq!(
            resolve(&shrunk, Control::Force, Control::Remove),
            Control::Remove
        );
        assert_eq!(
            resolve(&shrunk, Control::Cancel, Control::Remove),
            Control::Cancel
        );
    }

    #[test]
    fn an_empty_dialog_is_a_no_op_rather_than_a_panic() {
        assert_eq!(step(&[] as &[Control], Control::Remove, 1), Control::Remove);
    }
}
