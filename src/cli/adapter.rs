#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stderr usage/errors for humans and scripts"
)]
//! `flk adapter hello` (ADR-0028 §8, #1037): a channel adapter's start-up
//! check. It presents the binding's capability token through `channel.hello`
//! and prints what the binding may do. The request is state-changing for the
//! #868 gate, so against a server without `channel.hello` it exits 78
//! (`server_version_gap`) before sending it, and an adapter that runs this
//! first refuses to start.

use crate::api::schema::{ChannelHelloParams, Method, Request};
use crate::channels::ChannelToken;

pub(super) const HELLO_USAGE: &str = "flk adapter hello <binding> [--token-file PATH]";

pub(super) fn run_adapter_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("hello") => hello(&args[1..]),
        Some("help" | "--help" | "-h") => {
            eprintln!("usage: {HELLO_USAGE}");
            Ok(super::exit::SUCCESS)
        }
        _ => {
            eprintln!("usage: {HELLO_USAGE}");
            Ok(super::exit::USAGE)
        }
    }
}

fn hello(args: &[String]) -> std::io::Result<i32> {
    let (binding, token_file) = match args {
        [binding] => (binding, crate::channels::token_path(binding)),
        [binding, flag, path] if flag == "--token-file" => (binding, path.into()),
        _ => {
            eprintln!("usage: {HELLO_USAGE}");
            return Ok(super::exit::USAGE);
        }
    };
    let token = match std::fs::read_to_string(&token_file) {
        Ok(token) => ChannelToken(token.trim().to_owned()),
        Err(err) => {
            eprintln!("cannot read the token for {binding:?} at {}: {err}; the binding's owner_node issues it at startup and on reload", token_file.display());
            return Ok(super::exit::FAILURE);
        }
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:adapter:hello".into(),
        method: Method::ChannelHello(ChannelHelloParams {
            binding: binding.clone(),
            token,
        }),
    })?)
}
