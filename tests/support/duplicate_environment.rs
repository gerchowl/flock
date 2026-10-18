//! Execute a prepared isolated command with a deliberate duplicate env key.
use super::environment::Command;
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    io,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    process::Child,
};

// The CString allocations own every pointer in these immutable tables. They
// stay live together across spawn, and the child only calls execve before exec.
struct Exec {
    program: CString,
    _arguments: Vec<CString>,
    _environment: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}
// SAFETY: tables point only into owned immutable CString allocations; moving
// the owner does not move those allocations, and no thread mutates either table.
unsafe impl Send for Exec {}
// SAFETY: the same immutable ownership rule permits shared access.
unsafe impl Sync for Exec {}
impl Exec {
    unsafe fn run(&self) -> io::Result<()> {
        // SAFETY: all tables are NUL terminated and their owned strings remain
        // live. execve is async-signal-safe; no allocation occurs before it.
        unsafe {
            libc::execve(
                self.program.as_ptr(),
                self.argv.as_ptr(),
                self.envp.as_ptr(),
            )
        };
        Err(io::Error::last_os_error())
    }
}

/// Spawn with the isolated builder's environment plus one raw duplicate entry.
/// Reject `env_clear`: `get_envs` cannot distinguish cleared inherited variables.
pub fn spawn(command: Command, key: &OsStr, duplicate: &OsStr) -> io::Result<Child> {
    if command.environment_was_cleared() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "duplicate environment does not support env_clear",
        ));
    }
    let mut command = command.into_std();
    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => {
                environment.insert(key.to_os_string(), value.to_os_string());
            }
            None => {
                environment.remove(key);
            }
        }
    }
    let entry = |key: &OsStr, value: &OsStr| -> io::Result<CString> {
        CString::new([key.as_bytes(), b"=", value.as_bytes()].concat()).map_err(io::Error::other)
    };
    let mut environment: Vec<_> = environment
        .iter()
        .map(|(key, value)| entry(key, value))
        .collect::<io::Result<_>>()?;
    environment.push(entry(key, duplicate)?);
    let program = CString::new(command.get_program().as_bytes()).map_err(io::Error::other)?;
    let arguments = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|arg| CString::new(arg.as_bytes()).map_err(io::Error::other))
        .collect::<io::Result<Vec<_>>>()?;
    let argv = arguments
        .iter()
        .map(|arg| arg.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp = environment
        .iter()
        .map(|entry| entry.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let exec = Exec {
        program,
        _arguments: arguments,
        _environment: environment,
        argv,
        envp,
    };
    // SAFETY: only the preconstructed execve call runs in the child before exec.
    unsafe {
        command.pre_exec(move || exec.run());
    }
    // The fixture needs CommandExt and retains the preparation above.
    #[allow(clippy::disallowed_methods)]
    command.spawn()
}

#[test]
fn duplicate_environment_rejects_env_clear_before_exec() {
    let mut command = Command::new("/bin/sh");
    command.env_clear().args(["-c", "exit 0"]);
    match spawn(command, OsStr::new("FIXTURE_KEY"), OsStr::new("duplicate")) {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
        Ok(mut child) => {
            child.wait().unwrap();
            panic!("env_clear must be refused before execution");
        }
    }
}
