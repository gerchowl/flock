#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! `flk report` — file a bug against the repo this binary was built from (#233).
//!
//! Thin adapter. It gathers inputs (prose from a file or flags, provenance from
//! the status seam, diagnostics from the log files), hands them to the pure
//! `crate::report::compose`, and decides what to do with the result. All the
//! rules live in `crate::report`; a second entry point reuses them rather than
//! reimplementing them.
//!
//! Nothing is transmitted without an explicit `--open`. The default prints.

use std::collections::BTreeMap;

use crate::report::compose::{compose, ReportInputs};
use crate::report::schema::ReportProvenance;
use crate::report::template::{form, prose_fields, FieldKind, ReportKind};
use crate::report::{self, url};

/// Records attached by default. Enough to carry a failure and its lead-up,
/// small enough that a reporter can actually read the preview.
const DEFAULT_RECORDS: usize = 40;

pub(super) fn run_report_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("template") => run_template(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print_report_help();
            Ok(i32::from(args.is_empty()) * 2)
        }
        Some(kind) => match ReportKind::parse(kind) {
            Some(_) if asks_for_help(&args[1..]) => {
                print_report_help();
                Ok(0)
            }
            Some(kind) => run_report(kind, &args[1..]),
            None => {
                eprintln!("unknown report kind: {kind}");
                eprintln!();
                print_report_help();
                Ok(2)
            }
        },
    }
}

/// `flk report bug --help` reached the option parser and came back
/// "unknown option: --help", pointing the reporter at `flk report help` (#365).
/// Every other `flk <verb> --help` prints help, so this one read as a bug
/// rather than a convention. Checked before the options are parsed, because
/// help is a request to explain the flags, not to satisfy them.
fn asks_for_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

fn run_template(args: &[String]) -> std::io::Result<i32> {
    let kind = match args.first().map(|arg| arg.as_str()) {
        None | Some("bug") => ReportKind::Bug,
        Some(other) => {
            eprintln!("unknown report kind: {other}");
            return Ok(2);
        }
    };
    print!("{}", skeleton(kind));
    Ok(0)
}

/// The editable skeleton. Markdown with `## <field-id>` headings rather than
/// JSON: it is the shape a human already writes in, and it round-trips through
/// any editor. Guidance rides in HTML comments so it can be left in place and
/// still be stripped on the way back.
fn skeleton(kind: ReportKind) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<!-- flk report {} — fill in each section, then:  flk report {} --file <this-file> --open -->\n",
        kind.as_str(),
        kind.as_str()
    ));
    for field in form(kind).fields {
        if field.kind == FieldKind::Checkboxes {
            out.push_str(&format!(
                "\n<!-- \"{}\" is confirmed by you in the browser — flk cannot tick it for you. -->\n",
                field.label
            ));
            continue;
        }
        if field.machine_filled {
            out.push_str(&format!(
                "\n<!-- \"{}\" is filled in automatically. -->\n",
                field.label
            ));
            continue;
        }
        out.push_str(&format!("\n## {}\n<!-- {} -->\n\n", field.id, field.label));
    }
    out
}

#[derive(Default)]
struct Options {
    file: Option<String>,
    repo: Option<String>,
    open: bool,
    records: Option<usize>,
    no_diagnostics: bool,
    all_levels: bool,
    prose: BTreeMap<String, String>,
}

fn run_report(kind: ReportKind, args: &[String]) -> std::io::Result<i32> {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("run 'flk report help' for usage");
            return Ok(2);
        }
    };

    let repo = match url::resolve(options.repo.as_deref()) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("error: {err}");
            return Ok(2);
        }
    };

    let prose = match gather_prose(kind, &options) {
        Ok(prose) => prose,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    // Server may be down — that is the normal case for a bug report, and
    // `read_server_runtime_status` already models it rather than erroring.
    let server = crate::cli::status::read_server_runtime_status()?;
    let provenance = ReportProvenance::collect(&server);

    let diagnostics = if options.no_diagnostics {
        Vec::new()
    } else {
        let records = report::collect_diagnostics(options.records.unwrap_or(DEFAULT_RECORDS) * 8);
        let records = if options.all_levels {
            records
        } else {
            report::only_problems(records)
        };
        let keep = options.records.unwrap_or(DEFAULT_RECORDS);
        if records.len() < keep && !options.all_levels {
            // The pre-filter window is a multiple of the ask, so a quiet period
            // dominated by INFO can yield fewer. Silence here would read as
            // "that is all there was".
            eprintln!(
                "note: found {} warning/error records, fewer than the {keep} requested — \
                 pass --all-levels to include routine activity",
                records.len()
            );
        }
        let start = records.len().saturating_sub(keep);
        records[start..].to_vec()
    };

    let composed = compose(&ReportInputs {
        kind,
        repo,
        prose,
        provenance,
        diagnostics,
    });

    println!("{}", composed.preview);

    for advisory in &composed.advisories {
        eprintln!("note: {}", advisory.message());
    }
    if composed.url.truncated {
        eprintln!(
            "note: some text was shortened to fit GitHub's URL limit — the full text is NOT in the link"
        );
    }

    if !options.open {
        println!("destination: {}", composed.destination);
        println!("{}", composed.url.url);
        println!();
        println!("nothing has been sent. re-run with --open to review it on GitHub.");
        return Ok(0);
    }

    open_for_review(&composed)
}

fn open_for_review(composed: &crate::report::compose::Composed) -> std::io::Result<i32> {
    if !url::is_safe_to_open(&composed.url.url) {
        eprintln!("error: refusing to open a URL that is not an https://github.com/ link");
        return Ok(1);
    }

    if let Some(block) = &composed.diagnostics_block {
        crate::selection::write_osc52_bytes(block.as_bytes());
        println!("diagnostics copied to your clipboard — paste them into the issue body.");
    }

    println!("destination: {}", composed.destination);

    // On a host reached over SSH there is no browser worth launching, and the
    // clipboard write above already travelled back to the local terminal.
    if crate::selection::should_prefer_osc52() {
        println!("open this on your local machine:");
        println!("{}", composed.url.url);
        return Ok(0);
    }

    match crate::platform::open_url(&composed.url.url) {
        Ok(()) => {
            println!("opened GitHub's bug form. review it, tick the confirmation box, and submit.");
            Ok(0)
        }
        Err(err) => {
            eprintln!("could not open a browser ({err}); open this yourself:");
            println!("{}", composed.url.url);
            Ok(0)
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |name: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {name}"))
        };
        match arg {
            "--open" => {
                options.open = true;
                index += 1;
            }
            "--print" => {
                options.open = false;
                index += 1;
            }
            "--no-diagnostics" => {
                options.no_diagnostics = true;
                index += 1;
            }
            "--all-levels" => {
                options.all_levels = true;
                index += 1;
            }
            "--file" => {
                options.file = Some(take_value("--file")?);
                index += 2;
            }
            "--repo" => {
                options.repo = Some(take_value("--repo")?);
                index += 2;
            }
            "--last" => {
                let value = take_value("--last")?;
                options.records = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --last value: {value}"))?,
                );
                index += 2;
            }
            "--current" | "--expected" | "--repro" | "--impact" => {
                let value = take_value(arg)?;
                options.prose.insert(flag_to_field(arg).to_string(), value);
                index += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(options)
}

fn flag_to_field(flag: &str) -> &'static str {
    match flag {
        "--current" => "current-behavior",
        "--expected" => "expected-behavior",
        "--repro" => "reproduction",
        "--impact" => "impact",
        other => unreachable!("unmapped prose flag {other}"),
    }
}

fn gather_prose(kind: ReportKind, options: &Options) -> Result<BTreeMap<String, String>, String> {
    let mut prose = BTreeMap::new();
    if let Some(path) = &options.file {
        let content =
            std::fs::read_to_string(path).map_err(|err| format!("could not read {path}: {err}"))?;
        prose.extend(parse_skeleton(&content));
    }
    // Flags win over the file, so a one-off override doesn't need an edit.
    prose.extend(options.prose.clone());

    let missing: Vec<&str> = prose_fields(kind)
        .filter(|field| {
            prose
                .get(field.id)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        })
        .map(|field| field.id)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "these sections are still empty: {}\n\
             write them with `flk report {} template > bug.md`, edit, then \
             `flk report {} --file bug.md`",
            missing.join(", "),
            kind.as_str(),
            kind.as_str()
        ));
    }
    Ok(prose)
}

/// Read a filled skeleton back. `## <field-id>` starts a section; HTML comment
/// lines are guidance and never content.
///
/// Headings inside a fenced code block are content, not structure. Reproduction
/// steps routinely paste shell transcripts and markdown, and treating a `##`
/// inside a fence as a new section silently truncated the rest of the report.
fn parse_skeleton(content: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence {
            if let Some(heading) = trimmed.strip_prefix("## ") {
                current = Some(heading.trim().to_string());
                continue;
            }
            if trimmed.starts_with("<!--") {
                continue;
            }
        }
        if let Some(field) = &current {
            let section = out.entry(field.clone()).or_default();
            section.push_str(line);
            section.push('\n');
        }
    }
    out.into_iter()
        .map(|(key, value)| (key, value.trim().to_string()))
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

fn print_report_help() {
    eprintln!("flk report — file a bug with version, environment and logs already filled in");
    eprintln!();
    eprintln!("  flk report template bug > bug.md    write the form to edit");
    eprintln!("  flk report bug --file bug.md        compose and preview it");
    eprintln!("  flk report bug --file bug.md --open review it on GitHub and submit");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --file <path>      read a filled-in form");
    eprintln!("  --current <text>   set a section directly instead of editing a file");
    eprintln!("  --expected <text>");
    eprintln!("  --repro <text>");
    eprintln!("  --impact <text>");
    eprintln!("  --open             open GitHub's form, prefilled, for you to submit");
    eprintln!("  --print            preview only, send nothing (default)");
    eprintln!("  --last <n>         attach n log records (default {DEFAULT_RECORDS})");
    eprintln!("  --all-levels       attach INFO records too, not just WARN/ERROR");
    eprintln!("  --no-diagnostics   attach no logs at all");
    eprintln!("  --repo <owner/name>  file somewhere other than this build's repo");
    eprintln!();
    eprintln!("Logs are redacted before they are shown: paths, hostnames, tokens and");
    eprintln!("usernames are stripped. The preview is exactly what leaves your machine,");
    eprintln!("and diagnostics go to your clipboard, never into the link.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_is_recognised_after_the_report_kind() {
        // `flk report bug --help` used to be an "unknown option" error.
        assert!(asks_for_help(&["--help".to_string()]));
        assert!(asks_for_help(&["-h".to_string()]));
        assert!(asks_for_help(&[
            "--file".to_string(),
            "bug.md".to_string(),
            "--help".to_string(),
        ]));
        assert!(!asks_for_help(&[
            "--file".to_string(),
            "bug.md".to_string()
        ]));
    }

    #[test]
    fn skeleton_round_trips_through_the_parser() {
        let mut filled = skeleton(ReportKind::Bug);
        filled = filled.replace(
            "## current-behavior\n<!-- Current behavior -->\n",
            "## current-behavior\nit crashes\n",
        );
        let parsed = parse_skeleton(&filled);
        assert_eq!(
            parsed.get("current-behavior").map(String::as_str),
            Some("it crashes")
        );
    }

    #[test]
    fn skeleton_omits_the_checkbox_and_environment_sections() {
        let out = skeleton(ReportKind::Bug);
        assert!(!out.contains("## bug-confirmation"), "{out}");
        assert!(!out.contains("## environment"), "{out}");
        assert!(out.contains("## reproduction"), "{out}");
    }

    #[test]
    fn comments_are_never_content() {
        let parsed = parse_skeleton("## impact\n<!-- guidance -->\n\n");
        assert!(!parsed.contains_key("impact"));
    }

    #[test]
    fn multiline_sections_survive() {
        let parsed = parse_skeleton("## reproduction\nstep one\nstep two\n\n## impact\nbad\n");
        assert_eq!(
            parsed.get("reproduction").map(String::as_str),
            Some("step one\nstep two")
        );
        assert_eq!(parsed.get("impact").map(String::as_str), Some("bad"));
    }

    #[test]
    fn headings_inside_a_fence_are_content_not_structure() {
        let parsed = parse_skeleton(
            "## reproduction\nRun these:\n```sh\n## installing\nmake test\n```\nThen it dies.\n",
        );
        let repro = parsed.get("reproduction").expect("reproduction survived");
        assert!(repro.contains("## installing"), "{repro}");
        assert!(repro.contains("Then it dies."), "{repro}");
        assert!(!parsed.contains_key("installing"));
    }

    #[test]
    fn flags_override_the_file() {
        let options = parse_options(&[
            "--repro".to_string(),
            "do the thing".to_string(),
            "--open".to_string(),
        ])
        .expect("parse");
        assert_eq!(
            options.prose.get("reproduction").map(String::as_str),
            Some("do the thing")
        );
        assert!(options.open);
    }

    #[test]
    fn rejects_unknown_options_and_missing_values() {
        assert!(parse_options(&["--nope".to_string()]).is_err());
        assert!(parse_options(&["--file".to_string()]).is_err());
        assert!(parse_options(&["--last".to_string(), "x".to_string()]).is_err());
    }

    #[test]
    fn missing_sections_are_named() {
        let err = gather_prose(ReportKind::Bug, &Options::default()).expect_err("should refuse");
        assert!(err.contains("current-behavior"), "{err}");
        assert!(err.contains("reproduction"), "{err}");
    }
}
