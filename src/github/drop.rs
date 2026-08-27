//! Composing a cross-repo issue drop (#371).
//!
//! Pure, in the shape ADR-0010 decision 1 established for `src/report/`: the
//! impure edges — enumerating repositories, spawning the operator's editor,
//! performing the mutation — live with their callers, so filter behaviour,
//! skeleton text and the template advisory are testable without a network or a
//! PTY.
//!
//! This is deliberately NOT `report::compose::ReportInputs`. That type is
//! bug-report-shaped: it carries `ReportProvenance` (this binary's version,
//! channel, build commit, the running server's protocol) and a redacted log
//! tail, and its field table is gated against **flock's own**
//! `.github/ISSUE_TEMPLATE/bug.yml`. None of that generalises to filing an idea
//! into a third-party repo whose templates flock has never seen, and threading
//! it through would drag diagnostics into a feature that has nothing to do with
//! them. The two share `report::url`'s destination validation, which is the
//! part that is genuinely about "which repo, spelled correctly".

use super::issues::{RepoMeta, TemplatePosture};
use super::repos::Provenance;

/// Why a drop cannot be filed yet. Advisory and blocking cases are separate
/// types — see [`Advisory`] — so a warning can never accidentally stop a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropError {
    NoDestination,
    /// `owner/name` that does not parse. Carries what was typed so the message
    /// can quote it back rather than saying "invalid".
    MalformedDestination(String),
    EmptyTitle,
    EmptyBody,
}

impl std::fmt::Display for DropError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDestination => write!(f, "pick a repository first"),
            Self::MalformedDestination(value) => {
                write!(f, "expected owner/name, got {value:?}")
            }
            Self::EmptyTitle => write!(f, "an issue needs a title"),
            Self::EmptyBody => write!(f, "the body is empty — nothing was written"),
        }
    }
}

/// Something the operator should know before this is filed.
///
/// Advisory rather than blocking, following `report::compose::Advisory`: the
/// operator is filing into their own repositories and a tool that refuses
/// outright just teaches people to work around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Advisory {
    /// The repo defines issue templates this build cannot fill in.
    BypassesTemplate(Vec<String>),
    /// The destination is not a repo this machine or fleet has seen.
    UnfamiliarDestination,
}

impl Advisory {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::BypassesTemplate(names) => format!(
                "{} asks for a template ({}) that flk cannot fill in — filing raw \
                 would bypass the structure the repo asked for. Use the browser \
                 form instead to answer it properly.",
                "this repository",
                names.join(", ")
            ),
            Self::UnfamiliarDestination => {
                "you have no checkout of this repo and nobody in the fleet is on it \
                 — worth re-reading the owner/name before filing"
                    .to_string()
            }
        }
    }
}

/// Validate a typed destination through `report::url`'s existing rules.
///
/// Reused rather than reimplemented: that function already enforces GitHub's
/// real 39/100-character owner and name limits and rejects the shapes that make
/// a destination ambiguous. A second spelling of "is this a repo" is exactly
/// how two surfaces start disagreeing about where an issue went.
pub(crate) fn normalize_destination(value: &str) -> Result<String, DropError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(DropError::NoDestination);
    }
    crate::report::url::resolve(Some(trimmed))
        .map_err(|_| DropError::MalformedDestination(trimmed.to_string()))
}

/// Everything the operator has assembled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    pub repo: String,
    pub title: String,
    pub body: String,
    pub label_names: Vec<String>,
    pub issue_type: Option<String>,
}

/// Check a draft, returning the blocking reason if there is one.
pub(crate) fn validate(draft: &Draft) -> Result<(), DropError> {
    normalize_destination(&draft.repo)?;
    if draft.title.trim().is_empty() {
        return Err(DropError::EmptyTitle);
    }
    if body_is_empty(&draft.body) {
        return Err(DropError::EmptyBody);
    }
    Ok(())
}

/// Advisories for a draft against what is known about its destination.
pub(crate) fn advisories(meta: Option<&RepoMeta>, provenance: Option<Provenance>) -> Vec<Advisory> {
    let mut out = Vec::new();
    if let Some(RepoMeta {
        templates: TemplatePosture::Defined(names),
        ..
    }) = meta
    {
        out.push(Advisory::BypassesTemplate(names.clone()));
    }
    if provenance == Some(Provenance::Reachable) || provenance.is_none() {
        out.push(Advisory::UnfamiliarDestination);
    }
    out
}

/// Guidance lines, which are stripped before the body is filed.
const GUIDE_PREFIX: &str = "<!--";

/// The markdown skeleton handed to the operator's editor.
///
/// Guidance rides in HTML comments so it can be left in place while writing and
/// disappears on the way out — the same device `flk report template` uses
/// (`src/cli/report.rs`), and the reason a skeleton beats a form: the operator
/// writes markdown in their own editor, which is the shape they already write
/// issues in.
pub(crate) fn skeleton(repo: &str, title: &str, meta: Option<&RepoMeta>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{GUIDE_PREFIX} flk issue drop -> {repo}\n\
         \x20    title: {title}\n\
         \x20    Write the issue body below. Lines in HTML comments are stripped.\n\
         \x20    Save and quit when done; quit without saving to abandon. -->\n\n"
    ));
    if let Some(RepoMeta {
        templates: TemplatePosture::Defined(names),
        ..
    }) = meta
    {
        out.push_str(&format!(
            "{GUIDE_PREFIX} NOTE: this repo defines {} — flk cannot fill a template\n\
             \x20    in and will file this as a plain body. -->\n\n",
            names.join(", ")
        ));
    }
    out
}

/// Strip guidance and decide whether anything real was written.
///
/// A body of only comments and whitespace is empty: the operator opened the
/// editor and quit, and filing that produces an issue with no content in a repo
/// they may not be watching.
pub(crate) fn parse_body(contents: &str) -> String {
    let mut out = String::new();
    let mut in_comment = false;
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if !in_comment && trimmed.starts_with(GUIDE_PREFIX) {
            // A single-line comment opens and closes on the same line.
            if !line.contains("-->") {
                in_comment = true;
            }
            continue;
        }
        if in_comment {
            if line.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

fn body_is_empty(body: &str) -> bool {
    parse_body(body).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::issues::{IssueType, Label};

    fn meta(templates: TemplatePosture) -> RepoMeta {
        RepoMeta {
            repo_id: "R_1".into(),
            labels: vec![Label {
                id: "L".into(),
                name: "bug".into(),
            }],
            issue_types: Some(vec![IssueType {
                id: "T".into(),
                name: "Bug".into(),
            }]),
            templates,
        }
    }

    #[test]
    fn destinations_reuse_the_report_url_rules() {
        assert_eq!(normalize_destination(" owner/name ").unwrap(), "owner/name");
        // A full URL reduces to owner/name, same as `flk report --repo`.
        assert_eq!(
            normalize_destination("https://github.com/owner/name").unwrap(),
            "owner/name"
        );
        assert_eq!(normalize_destination(""), Err(DropError::NoDestination));
        assert!(matches!(
            normalize_destination("not-a-repo"),
            Err(DropError::MalformedDestination(_))
        ));
        assert!(matches!(
            normalize_destination("a/b/c"),
            Err(DropError::MalformedDestination(_))
        ));
    }

    #[test]
    fn a_body_of_only_guidance_counts_as_empty() {
        // Opening the editor and quitting must not file a contentless issue
        // into a repo the operator may not be watching.
        let only_guide = skeleton("o/n", "t", None);
        assert_eq!(parse_body(&only_guide), "");
        assert_eq!(
            validate(&Draft {
                repo: "o/n".into(),
                title: "t".into(),
                body: only_guide,
                ..Default::default()
            }),
            Err(DropError::EmptyBody)
        );
    }

    #[test]
    fn guidance_is_stripped_but_prose_survives_verbatim() {
        let written = format!(
            "{}Real prose.\n\nWith a blank line and `code`.\n",
            skeleton("o/n", "t", None)
        );
        assert_eq!(
            parse_body(&written),
            "Real prose.\n\nWith a blank line and `code`."
        );
    }

    #[test]
    fn a_multi_line_comment_block_is_stripped_whole() {
        let body = "<!-- one\ntwo\nthree -->\nkept\n";
        assert_eq!(parse_body(body), "kept");
    }

    #[test]
    fn markdown_html_comments_the_operator_wrote_are_also_stripped() {
        // Honest limitation, asserted so it is a known behaviour rather than a
        // surprise: the stripper cannot tell flk's guidance from the operator's
        // own comment. It only ever removes comments, never prose.
        let body = "kept\n<!-- operator's own note -->\nalso kept\n";
        assert_eq!(parse_body(body), "kept\nalso kept");
    }

    #[test]
    fn validation_reports_the_first_blocking_reason() {
        let good = Draft {
            repo: "o/n".into(),
            title: "t".into(),
            body: "b".into(),
            ..Default::default()
        };
        assert_eq!(validate(&good), Ok(()));
        assert_eq!(
            validate(&Draft {
                title: String::new(),
                ..good.clone()
            }),
            Err(DropError::EmptyTitle)
        );
        assert_eq!(
            validate(&Draft {
                repo: String::new(),
                ..good.clone()
            }),
            Err(DropError::NoDestination)
        );
        assert_eq!(
            validate(&Draft {
                title: "   ".into(),
                ..good
            }),
            Err(DropError::EmptyTitle),
            "whitespace is not a title"
        );
    }

    #[test]
    fn a_repo_with_a_template_warns_rather_than_silently_bypassing_it() {
        // ADR-0010's invariant: posting a raw body past a form the repo asked
        // for is the failure, and flk carries no YAML parser to answer one.
        let defined = meta(TemplatePosture::Defined(vec!["bug.yml".into()]));
        let out = advisories(Some(&defined), Some(Provenance::LocalCheckout));
        assert_eq!(
            out,
            vec![Advisory::BypassesTemplate(vec!["bug.yml".into()])]
        );
        assert!(out[0].message().contains("bug.yml"));

        let none = meta(TemplatePosture::None);
        assert!(advisories(Some(&none), Some(Provenance::LocalCheckout)).is_empty());
    }

    #[test]
    fn an_unfamiliar_destination_is_flagged_but_not_blocked() {
        let none = meta(TemplatePosture::None);
        assert_eq!(
            advisories(Some(&none), Some(Provenance::Reachable)),
            vec![Advisory::UnfamiliarDestination]
        );
        // Still filable — this is a warning, not a refusal.
        assert_eq!(
            validate(&Draft {
                repo: "o/n".into(),
                title: "t".into(),
                body: "b".into(),
                ..Default::default()
            }),
            Ok(())
        );
    }

    #[test]
    fn the_skeleton_names_the_destination_and_the_template_hole() {
        let with_template = meta(TemplatePosture::Defined(vec!["bug.yml".into()]));
        let text = skeleton("acme/widgets", "a title", Some(&with_template));
        assert!(text.contains("acme/widgets"));
        assert!(text.contains("a title"));
        assert!(text.contains("bug.yml"));
        // Every guidance line must survive the round trip as nothing.
        assert_eq!(parse_body(&text), "");
    }
}
