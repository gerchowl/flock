//! Native badge projection: macOS supports labels; other platforms retain the count.

#[derive(Debug, PartialEq)]
pub enum Badge {
    Count(u32),
    Label(String),
}

pub fn action(count: u32, label: Option<String>, macos: bool) -> Badge {
    match label {
        Some(label) if macos => Badge::Label(label),
        _ => Badge::Count(count),
    }
}

pub fn set(window: &tauri::Window, count: u32, label: Option<String>) -> tauri::Result<()> {
    match action(count, label, cfg!(target_os = "macos")) {
        Badge::Count(0) => {
            #[cfg(target_os = "macos")]
            window.set_badge_label(None)?;
            window.set_badge_count(None)
        }
        Badge::Count(count) => window.set_badge_count(Some(count.into())),
        #[cfg(target_os = "macos")]
        Badge::Label(label) => window.set_badge_label(Some(label)),
        // The action chooses a count on these platforms, whose Tauri API has no label setter.
        #[cfg(not(target_os = "macos"))]
        Badge::Label(_) => window.set_badge_count(Some(count.into())),
    }
}
