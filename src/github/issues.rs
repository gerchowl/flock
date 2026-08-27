//! Per-repo issue metadata, and the one mutation flock performs (#371).
//!
//! ## Why this is flock's first write path
//!
//! ADR-0010 decided that `flk report` never submits: it opens GitHub's real
//! form, prefilled, and a human presses the button. Two of the three reasons
//! for that still hold here and are preserved below — no write credential is
//! compiled into the binary (this uses the operator's own local token), and the
//! composed issue is previewed before anything leaves the machine.
//!
//! The third reason has gone stale. ADR-0010 rejected `gh issue create` because
//! it "bypasses issue-form validation … it can only produce something
//! template-shaped". That was true of `gh issue create --title --body`. It is
//! no longer true of the mutation: `CreateIssueInput` now carries
//! `issueTemplate` and `issueFields`, and the server runs the form's own
//! required-field validation on that path.
//!
//! Two measurements forced the change rather than preference:
//!
//! * #371's own body is 6 739 bytes raw and **9 791 URL-encoded**, against
//!   `report::url::MAX_URL_LEN` of 7 500. The prefilled-URL route would
//!   silently truncate the very issue that motivated the feature.
//! * There is no URL query parameter for an issue **type**, so that axis is
//!   unreachable by construction on the URL route.
//!
//! ## The template hole, stated rather than papered over
//!
//! Mapping a repo's form fields into `issueFields` means reading that repo's
//! `.github/ISSUE_TEMPLATE/*.yml`, and flock carries **no YAML parser** —
//! ADR-0010 declined to add one, and nothing since has changed that. So this
//! module can *detect* that a repo defines a template but cannot fill it in.
//!
//! Rather than post a raw body and silently bypass the structure the repo asked
//! for — precisely what ADR-0010 called out — detection produces an advisory and
//! the operator is offered the prefilled-URL route, which does run the real
//! form. That keeps the URL path load-bearing instead of vestigial, and leaves
//! `issueFields` as a clean follow-up for whenever a YAML parser is justified.

use super::graphql::{execute, GraphQlErrorKind};

/// A label as GitHub identifies it. The mutation takes ids, not names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Label {
    pub id: String,
    pub name: String,
}

/// An issue type. Separate axis from labels, configured per organisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueType {
    pub id: String,
    pub name: String,
}

/// What a repo's `.github/ISSUE_TEMPLATE/` implies for filing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TemplatePosture {
    /// No template directory, or nothing in it that is a template.
    None,
    /// The repo asks for structure this build cannot fill in.
    Defined(Vec<String>),
}

/// Everything the composer needs about one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoMeta {
    /// GitHub's node id, required by `createIssue`.
    pub repo_id: String,
    pub labels: Vec<Label>,
    /// `None` when the owning organisation has configured no types at all.
    ///
    /// This is `null` on the wire, **not** `[]` — verified against
    /// `gerchowl/flock` itself. Treating the two the same is the difference
    /// between hiding the axis and rendering an empty required picker on most
    /// repositories.
    pub issue_types: Option<Vec<IssueType>>,
    pub templates: TemplatePosture,
}

/// The chooser config, which sits alongside templates but is not one.
const TEMPLATE_CHOOSER_CONFIG: &str = "config.yml";

/// Metadata for one destination, in a single round trip.
///
/// `object(expression: "HEAD:.github/ISSUE_TEMPLATE")` is used rather than
/// `Repository.issueTemplates` because the latter reports only legacy **markdown**
/// templates: it returns `[]` for `gerchowl/flock`, which defines `bug.yml`, while
/// returning three entries for `cli/cli`, which uses `.md`. Detecting templates
/// through it would report "no template" for every modern issue-form repo and
/// bypass exactly the structure this is meant to respect.
pub(crate) fn meta_query() -> &'static str {
    r#"
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    id
    labels(first: 100) { nodes { id name } }
    issueTypes(first: 50) { nodes { id name } }
    object(expression: "HEAD:.github/ISSUE_TEMPLATE") {
      ... on Tree { entries { name type } }
    }
  }
}"#
}

/// Parse the metadata response.
pub(crate) fn parse_meta(body: &str) -> Option<RepoMeta> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let repo = value.pointer("/data/repository")?;
    let repo_id = repo
        .get("id")
        .and_then(serde_json::Value::as_str)?
        .to_string();

    let pairs = |node: Option<&serde_json::Value>| -> Vec<(String, String)> {
        node.and_then(|n| n.get("nodes"))
            .and_then(serde_json::Value::as_array)
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|n| {
                        Some((
                            n.get("id").and_then(serde_json::Value::as_str)?.to_string(),
                            n.get("name")
                                .and_then(serde_json::Value::as_str)?
                                .to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let labels = pairs(repo.get("labels"))
        .into_iter()
        .map(|(id, name)| Label { id, name })
        .collect();

    // `null` and `[]` mean different things here; only the former hides the axis.
    let issue_types = match repo.get("issueTypes") {
        None | Some(serde_json::Value::Null) => None,
        other => Some(
            pairs(other)
                .into_iter()
                .map(|(id, name)| IssueType { id, name })
                .collect(),
        ),
    };

    let template_names: Vec<String> = repo
        .pointer("/object/entries")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|e| e.get("type").and_then(serde_json::Value::as_str) == Some("blob"))
                .filter_map(|e| e.get("name").and_then(serde_json::Value::as_str))
                .filter(|name| !name.eq_ignore_ascii_case(TEMPLATE_CHOOSER_CONFIG))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Some(RepoMeta {
        repo_id,
        labels,
        issue_types,
        templates: if template_names.is_empty() {
            TemplatePosture::None
        } else {
            TemplatePosture::Defined(template_names)
        },
    })
}

/// Fetch metadata for `owner/name`.
pub(crate) fn fetch_meta(owner: &str, name: &str) -> Result<RepoMeta, GraphQlErrorKind> {
    let variables = serde_json::json!({ "owner": owner, "name": name });
    let body = execute(meta_query(), variables, "issue_drop_meta")?;
    parse_meta(&body).ok_or(GraphQlErrorKind::GraphQl)
}

/// What to file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewIssue {
    pub repo_id: String,
    pub title: String,
    pub body: String,
    pub label_ids: Vec<String>,
    pub issue_type_id: Option<String>,
}

/// The mutation.
///
/// Every operator-authored value travels as a **variable**, never interpolated
/// into the document. An issue body is arbitrary prose that routinely contains
/// quotes, backslashes and braces; interpolation would let it terminate a
/// GraphQL string and inject document text.
pub(crate) fn create_query() -> &'static str {
    r#"
mutation($input: CreateIssueInput!) {
  createIssue(input: $input) {
    issue { number url }
  }
}"#
}

/// The filed issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FiledIssue {
    pub number: u64,
    pub url: String,
}

pub(crate) fn parse_created(body: &str) -> Option<FiledIssue> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let issue = value.pointer("/data/createIssue/issue")?;
    Some(FiledIssue {
        number: issue.get("number").and_then(serde_json::Value::as_u64)?,
        url: issue
            .get("url")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
    })
}

/// Build the mutation variables.
///
/// Optional inputs are omitted rather than sent as `null`: `issueTypeId: null`
/// is accepted today, but omitting is what the schema documents for "no value"
/// and does not depend on that staying true.
pub(crate) fn create_variables(issue: &NewIssue) -> serde_json::Value {
    let mut input = serde_json::Map::new();
    input.insert("repositoryId".into(), issue.repo_id.clone().into());
    input.insert("title".into(), issue.title.clone().into());
    input.insert("body".into(), issue.body.clone().into());
    if !issue.label_ids.is_empty() {
        input.insert("labelIds".into(), issue.label_ids.clone().into());
    }
    if let Some(type_id) = issue.issue_type_id.as_ref() {
        input.insert("issueTypeId".into(), type_id.clone().into());
    }
    serde_json::json!({ "input": serde_json::Value::Object(input) })
}

/// File the issue.
pub(crate) fn create_issue(issue: &NewIssue) -> Result<FiledIssue, GraphQlErrorKind> {
    let body = execute(create_query(), create_variables(issue), "issue_drop_create")?;
    parse_created(&body).ok_or(GraphQlErrorKind::GraphQl)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_body(issue_types: serde_json::Value, entries: serde_json::Value) -> String {
        serde_json::json!({
            "data": { "repository": {
                "id": "R_kg1",
                "labels": { "nodes": [ {"id": "LA_1", "name": "bug"} ] },
                "issueTypes": issue_types,
                "object": entries,
            }}
        })
        .to_string()
    }

    #[test]
    fn null_issue_types_hide_the_axis_but_empty_does_not() {
        // Verified against gerchowl/flock, which returns null. Collapsing null
        // into [] renders an empty picker on most repositories.
        let hidden = parse_meta(&meta_body(serde_json::Value::Null, serde_json::Value::Null))
            .expect("parses");
        assert_eq!(hidden.issue_types, None);

        let configured = parse_meta(&meta_body(
            serde_json::json!({ "nodes": [ {"id": "IT_1", "name": "Bug"} ] }),
            serde_json::Value::Null,
        ))
        .expect("parses");
        assert_eq!(
            configured.issue_types,
            Some(vec![IssueType {
                id: "IT_1".into(),
                name: "Bug".into()
            }])
        );

        let empty = parse_meta(&meta_body(
            serde_json::json!({ "nodes": [] }),
            serde_json::Value::Null,
        ))
        .expect("parses");
        assert_eq!(empty.issue_types, Some(Vec::new()));
    }

    #[test]
    fn a_yaml_issue_form_counts_as_a_template() {
        // Repository.issueTemplates reports only legacy markdown and returns []
        // for this exact shape, which is why detection reads the tree.
        let meta = parse_meta(&meta_body(
            serde_json::Value::Null,
            serde_json::json!({ "entries": [
                {"name": "bug.yml", "type": "blob"},
                {"name": "config.yml", "type": "blob"},
            ]}),
        ))
        .expect("parses");
        assert_eq!(
            meta.templates,
            TemplatePosture::Defined(vec!["bug.yml".to_string()]),
            "config.yml is the chooser, not a template"
        );
    }

    #[test]
    fn a_repo_without_the_directory_has_no_template() {
        let meta = parse_meta(&meta_body(serde_json::Value::Null, serde_json::Value::Null))
            .expect("parses");
        assert_eq!(meta.templates, TemplatePosture::None);
        // A directory holding only the chooser is also no template.
        let only_config = parse_meta(&meta_body(
            serde_json::Value::Null,
            serde_json::json!({ "entries": [ {"name": "config.yml", "type": "blob"} ]}),
        ))
        .expect("parses");
        assert_eq!(only_config.templates, TemplatePosture::None);
    }

    #[test]
    fn malformed_metadata_is_none_rather_than_a_panic() {
        assert!(parse_meta("not json").is_none());
        assert!(parse_meta(r#"{"data":{"repository":null}}"#).is_none());
    }

    #[test]
    fn operator_prose_travels_as_a_variable_not_document_text() {
        // A body that would terminate a GraphQL string if interpolated.
        let hostile = r#"") { x } injected(input: {"#;
        let vars = create_variables(&NewIssue {
            repo_id: "R_kg1".into(),
            title: hostile.into(),
            body: hostile.into(),
            label_ids: vec![],
            issue_type_id: None,
        });
        assert_eq!(
            vars.pointer("/input/title").unwrap().as_str(),
            Some(hostile)
        );
        // The document itself is fixed text and never carries the prose.
        assert!(!create_query().contains("injected"));
    }

    #[test]
    fn absent_optionals_are_omitted_rather_than_sent_as_null() {
        let vars = create_variables(&NewIssue {
            repo_id: "R_kg1".into(),
            title: "t".into(),
            body: "b".into(),
            label_ids: vec![],
            issue_type_id: None,
        });
        let input = vars.get("input").and_then(|i| i.as_object()).unwrap();
        assert!(!input.contains_key("labelIds"));
        assert!(!input.contains_key("issueTypeId"));

        let with = create_variables(&NewIssue {
            repo_id: "R_kg1".into(),
            title: "t".into(),
            body: "b".into(),
            label_ids: vec!["LA_1".into()],
            issue_type_id: Some("IT_1".into()),
        });
        let input = with.get("input").and_then(|i| i.as_object()).unwrap();
        assert_eq!(input["labelIds"], serde_json::json!(["LA_1"]));
        assert_eq!(input["issueTypeId"], serde_json::json!("IT_1"));
    }

    #[test]
    fn created_issue_is_read_back() {
        let body = serde_json::json!({
            "data": { "createIssue": { "issue": { "number": 372, "url": "https://x/372" } } }
        })
        .to_string();
        assert_eq!(
            parse_created(&body),
            Some(FiledIssue {
                number: 372,
                url: "https://x/372".into()
            })
        );
        assert!(parse_created(r#"{"data":{"createIssue":null}}"#).is_none());
    }
}
