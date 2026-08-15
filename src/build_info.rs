//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn channel() -> &'static str {
    non_empty(option_env!("FLOCK_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("FLOCK_BUILD_ID"))
}

/// The commit this binary was built from, when the build set it.
///
/// Declared as a rerun trigger in `build.rs` and set by the release and
/// preview workflows and by `nix/package.nix`. This is the field that answers
/// "which code actually ran" during triage: on the preview/`latest` channels
/// many revisions share one `BASE_VERSION`, so [`version`] alone cannot
/// distinguish them. Consumed by the report provenance block (#233).
pub fn commit() -> Option<&'static str> {
    non_empty(option_env!("FLOCK_BUILD_COMMIT"))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

/// The target triple this binary was built for, e.g. `aarch64-apple-darwin`.
///
/// Not derivable at runtime: `std::env::consts` gives OS and arch but not the
/// libc flavour, so it cannot tell a musl build from a gnu one.
pub fn target() -> &'static str {
    non_empty(option_env!("FLOCK_BUILD_TARGET")).unwrap_or("unknown")
}

/// `debug` or `release`. A debug build explains a whole class of "flock is
/// slow" reports without any further investigation.
pub fn profile() -> &'static str {
    non_empty(option_env!("FLOCK_BUILD_PROFILE")).unwrap_or("unknown")
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }
}
