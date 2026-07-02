#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::IntegrationTarget;

pub(super) fn run_integration_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_integration_help();
        return Ok(2);
    };

    match subcommand {
        "install" => integration_install(&args[1..]),
        "uninstall" => integration_uninstall(&args[1..]),
        "status" => integration_status(&args[1..]),
        "manifest" => integration_manifest(&args[1..]),
        "help" | "--help" | "-h" => {
            print_integration_help();
            Ok(0)
        }
        _ => {
            print_integration_help();
            Ok(2)
        }
    }
}

fn integration_status(args: &[String]) -> std::io::Result<i32> {
    let outdated_only = match args {
        [] => false,
        [flag] if flag == "--outdated-only" => true,
        _ => {
            eprintln!("usage: flock integration status [--outdated-only]");
            return Ok(2);
        }
    };

    if outdated_only {
        crate::integration::print_outdated_update_notice();
        return Ok(0);
    }

    for status in crate::integration::installed_integration_statuses() {
        let target = crate::integration::integration_target_label(status.target);
        let version = match status.installed_version {
            Some(version) => format!("v{version}"),
            None => "legacy".to_string(),
        };
        let state = match status.state {
            crate::integration::IntegrationStatusKind::NotInstalled => "not installed".to_string(),
            crate::integration::IntegrationStatusKind::Current => {
                format!("current ({version})")
            }
            crate::integration::IntegrationStatusKind::Outdated => {
                format!("outdated ({version} < v{})", status.expected_version)
            }
        };
        println!("{target}: {state} ({})", status.path.display());
    }

    Ok(0)
}

fn integration_manifest(args: &[String]) -> std::io::Result<i32> {
    let json = args.iter().any(|arg| arg == "--json");
    let rest: Vec<String> = args
        .iter()
        .filter(|arg| arg.as_str() != "--json")
        .cloned()
        .collect();
    let Some(target) = parse_integration_target(&rest, "manifest")? else {
        return Ok(2);
    };

    let manifest = match crate::integration::integration_manifest(target) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("{err}");
            return Ok(1);
        }
    };

    if json {
        match serde_json::to_string_pretty(&manifest) {
            Ok(rendered) => println!("{rendered}"),
            Err(err) => {
                eprintln!("failed to serialize manifest: {err}");
                return Ok(1);
            }
        }
    } else {
        print_integration_manifest_summary(&manifest);
    }
    Ok(0)
}

fn print_integration_manifest_summary(manifest: &serde_json::Value) {
    let field = |key: &str| manifest.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let version = manifest
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("target:       {}", field("target"));
    println!("version:      v{version}");
    println!("hook script:  {}", field("hookScript"));
    println!("settings:     {}", field("settingsPath"));
    println!("hooks fragment to merge into settings.json:");
    if let Some(hooks) = manifest.get("hooks") {
        let fragment = serde_json::json!({ "hooks": hooks });
        match serde_json::to_string_pretty(&fragment) {
            Ok(rendered) => {
                for line in rendered.lines() {
                    println!("  {line}");
                }
            }
            Err(err) => eprintln!("failed to render hooks fragment: {err}"),
        }
    }
}

fn integration_install(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "install")? else {
        return Ok(2);
    };

    match crate::integration::install_target(target) {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn integration_uninstall(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "uninstall")? else {
        return Ok(2);
    };

    match crate::integration::uninstall_target(target) {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn print_integration_messages(messages: Vec<String>) {
    for message in messages {
        println!("{message}");
    }
}

fn parse_integration_target(
    args: &[String],
    action: &str,
) -> std::io::Result<Option<IntegrationTarget>> {
    let Some(target) = args.first().map(|arg| arg.as_str()) else {
        eprintln!(
            "usage: flock integration {action} <pi|omp|claude|codex|copilot|kimi|opencode|hermes|qodercli>"
        );
        return Ok(None);
    };
    if args.len() != 1 {
        eprintln!(
            "usage: flock integration {action} <pi|omp|claude|codex|copilot|kimi|opencode|hermes|qodercli>"
        );
        return Ok(None);
    }

    let parsed = match target {
        "pi" => IntegrationTarget::Pi,
        "omp" => IntegrationTarget::Omp,
        "claude" => IntegrationTarget::Claude,
        "codex" => IntegrationTarget::Codex,
        "copilot" => IntegrationTarget::Copilot,
        "kimi" => IntegrationTarget::Kimi,
        "opencode" => IntegrationTarget::Opencode,
        "hermes" => IntegrationTarget::Hermes,
        "qodercli" => IntegrationTarget::Qodercli,
        _ => {
            eprintln!("unknown integration target: {target}");
            eprintln!(
                "currently supported: pi, omp, claude, codex, copilot, kimi, opencode, hermes, qodercli"
            );
            return Ok(None);
        }
    };

    Ok(Some(parsed))
}

fn print_integration_help() {
    eprintln!("flock integration commands:");
    eprintln!("  flock integration install pi");
    eprintln!("  flock integration install omp");
    eprintln!("  flock integration install claude");
    eprintln!("  flock integration install codex");
    eprintln!("  flock integration install copilot");
    eprintln!("  flock integration install kimi");
    eprintln!("  flock integration install opencode");
    eprintln!("  flock integration install hermes");
    eprintln!("  flock integration install qodercli");
    eprintln!("  flock integration uninstall pi");
    eprintln!("  flock integration uninstall omp");
    eprintln!("  flock integration uninstall claude");
    eprintln!("  flock integration uninstall codex");
    eprintln!("  flock integration uninstall copilot");
    eprintln!("  flock integration uninstall kimi");
    eprintln!("  flock integration uninstall opencode");
    eprintln!("  flock integration uninstall hermes");
    eprintln!("  flock integration uninstall qodercli");
    eprintln!("  flock integration status [--outdated-only]");
    eprintln!("  flock integration manifest <target> [--json]");
}
