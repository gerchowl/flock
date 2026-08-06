//! The report form table, and the gate that keeps it honest (#233).
//!
//! ## Why this is checked in rather than parsed at build time
//!
//! The obvious design is `build.rs` reading `.github/ISSUE_TEMPLATE/*.yml`.
//! It does not work here: `nix/package.nix`'s `lib.fileset` unions only
//! `assets`, `src`, `vendor/…`, `build.rs`, `Cargo.lock` and `Cargo.toml`, and
//! `Cargo.toml`'s crates.io `include` list is likewise `src/**/*` plus assets —
//! **`.github` is absent from both**. Parsing at build time would silently
//! produce an empty table on the two paths most users install through, and
//! would additionally pull a YAML parser into `[build-dependencies]` (the lock
//! file has none today, and the maintained options are thin).
//!
//! So the table lives here, and [`tests::table_matches_the_checked_in_forms`]
//! is the drift gate: it re-reads the real YAML and fails if the two disagree.
//! The check degrades to a skip when `.github` is absent, which is exactly the
//! sandboxed-build case — CI and every developer checkout do have it.

/// What a report kind resolves to on GitHub.
///
/// Only [`Destination::Issue`] is reachable in this release. Discussion forms
/// accept only `category`/`title`/`body` — no per-field prefill and no
/// server-side required-field validation — so they need a separate builder,
/// and this fork has Discussions disabled outright (`hasDiscussionsEnabled:
/// false`) even though CONTRIBUTING.md routes people there. Both are tracked
/// as follow-ups rather than guessed at here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Issue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Textarea,
    /// Cannot be prefilled from a URL — GitHub has never supported query-param
    /// prefill for `checkboxes`/`dropdown`. That is load-bearing rather than a
    /// limitation to work around: the box is an attestation that the reporter
    /// has a real reproduction, and a tool that ticks it forges a human's
    /// statement.
    Checkboxes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormField {
    /// The YAML `id:`, which is also the URL query-parameter name.
    pub id: &'static str,
    pub label: &'static str,
    pub required: bool,
    pub kind: FieldKind,
    /// Filled by the tool rather than the human.
    pub machine_filled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Form {
    pub kind: ReportKind,
    pub template_file: &'static str,
    pub destination: Destination,
    pub fields: &'static [FormField],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Bug,
}

impl ReportKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "bug" => Some(Self::Bug),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
        }
    }
}

const BUG_FIELDS: &[FormField] = &[
    FormField {
        id: "bug-confirmation",
        label: "Is this a reproducible bug?",
        required: true,
        kind: FieldKind::Checkboxes,
        machine_filled: false,
    },
    FormField {
        id: "current-behavior",
        label: "Current behavior",
        required: true,
        kind: FieldKind::Textarea,
        machine_filled: false,
    },
    FormField {
        id: "expected-behavior",
        label: "Expected behavior",
        required: true,
        kind: FieldKind::Textarea,
        machine_filled: false,
    },
    FormField {
        id: "reproduction",
        label: "Reproduction",
        required: true,
        kind: FieldKind::Textarea,
        machine_filled: false,
    },
    FormField {
        id: "impact",
        label: "Impact",
        required: true,
        kind: FieldKind::Textarea,
        machine_filled: false,
    },
    FormField {
        id: "environment",
        label: "Environment",
        required: true,
        kind: FieldKind::Textarea,
        // The whole point: nobody should be typing their version by hand.
        machine_filled: true,
    },
];

const BUG_FORM: Form = Form {
    kind: ReportKind::Bug,
    template_file: "bug.yml",
    destination: Destination::Issue,
    fields: BUG_FIELDS,
};

pub fn form(kind: ReportKind) -> &'static Form {
    match kind {
        ReportKind::Bug => &BUG_FORM,
    }
}

/// The fields a human is expected to write, in template order.
pub fn prose_fields(kind: ReportKind) -> impl Iterator<Item = &'static FormField> {
    form(kind)
        .fields
        .iter()
        .filter(|field| !field.machine_filled && field.kind == FieldKind::Textarea)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift gate. Re-reads the real form and asserts the table matches, so a
    /// `bug.yml` edit that this table does not follow fails the suite instead
    /// of silently producing reports GitHub rejects.
    ///
    /// Deliberately a line scan rather than a YAML parse: it needs no
    /// dependency, and the only thing under test is the `id:` sequence.
    #[test]
    fn table_matches_the_checked_in_forms() {
        let github = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".github");
        if !github.is_dir() {
            // Sandboxed build (Nix fileset / crates.io package) — `.github` is
            // not shipped at all. Nothing to drift against.
            return;
        }
        // From here the tree DOES carry `.github`, so a missing or unreadable
        // form is a deleted template, not a sandbox. Skipping that case would
        // make this gate pass loudest exactly when it should fail.
        let path = github.join("ISSUE_TEMPLATE/bug.yml");
        let content = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "{} is missing or unreadable ({err}) but .github exists — the \
                 report form table has nothing to validate against",
                path.display()
            )
        });

        let ids: Vec<String> = content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                trimmed
                    .strip_prefix("id:")
                    .map(|value| value.trim().to_string())
            })
            .collect();

        let table: Vec<String> = BUG_FIELDS
            .iter()
            .map(|field| field.id.to_string())
            .collect();

        assert_eq!(
            ids, table,
            "bug.yml field ids drifted from the table in src/report/template.rs"
        );
    }

    #[test]
    fn prose_fields_exclude_the_checkbox_and_the_machine_block() {
        let ids: Vec<&str> = prose_fields(ReportKind::Bug)
            .map(|field| field.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "current-behavior",
                "expected-behavior",
                "reproduction",
                "impact"
            ]
        );
    }

    #[test]
    fn kind_round_trips() {
        assert_eq!(ReportKind::parse("bug"), Some(ReportKind::Bug));
        assert_eq!(ReportKind::parse("idea"), None);
        assert_eq!(ReportKind::Bug.as_str(), "bug");
    }
}
