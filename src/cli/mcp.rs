#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI subcommand: usage/help text is the product surface"
)]

//! `flk mcp <subcommand>` — thin CLI dispatch for the MCP stdio server.
//!
//! The wire implementation (JSON-RPC framing, tool table, serve loop) lives
//! in [`crate::mcp`]. This module is the CLI-argv edge only: parse the
//! subcommand, print help, or hand off to [`crate::mcp::serve_over_stdio`].

pub(super) fn run_mcp_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("serve") => crate::mcp::serve_over_stdio(),
        Some("help" | "--help" | "-h") => {
            print_help();
            Ok(0)
        }
        None => {
            print_help();
            Ok(2)
        }
        Some(other) => {
            eprintln!("unknown mcp subcommand: {other}");
            print_help();
            Ok(2)
        }
    }
}

fn print_help() {
    println!("usage: flk mcp <serve|help>");
    println!();
    println!("subcommands:");
    println!(
        "  serve   run the MCP stdio server (JSON-RPC 2.0 over newline-delimited stdin/stdout)"
    );
    println!("  help    print this message");
    println!();
    println!("environment:");
    println!("  FLOCK_SOCKET_PATH   flock server socket (defaults to the current session's)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_flag_returns_zero() {
        assert_eq!(run_mcp_command(&["help".into()]).unwrap(), 0);
        assert_eq!(run_mcp_command(&["--help".into()]).unwrap(), 0);
    }

    #[test]
    fn no_subcommand_prints_usage_and_errors() {
        assert_eq!(run_mcp_command(&[]).unwrap(), 2);
    }

    #[test]
    fn unknown_subcommand_errors() {
        assert_eq!(run_mcp_command(&["explode".into()]).unwrap(), 2);
    }
}
