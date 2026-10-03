//! Portable reads of the live process table, for the reap sweep in `super`.
//!
//! #521. The sweep exists to reap `flk server` daemons the harness never
//! learned a pid for — most importantly the DETACHED one the product spawns
//! itself during auto-detect launch (`src/server/autodetect.rs`). It worked by
//! walking `/proc`, which does not exist on Darwin, so on a Mac it enumerated
//! nothing and reported success: `read_dir("/proc")` is `NotFound`, the `?`
//! became `Ok(vec![])`, and four daemons leaked per `just check` run while
//! every test passed. nextest cannot see them either, because they redirect
//! stdio to `/dev/null` and so never hold the test's output pipes open.
//!
//! Two `/proc` dependencies had to go, and this module is where they went:
//!
//! * **enumerating pids** — `/proc` on Linux, `proc_listallpids` on Darwin.
//!   The issue nominated `sysctl KERN_PROC_ALL`; `proc_listallpids` reads the
//!   same kernel table and is used instead on purpose. `KERN_PROC_ALL` returns
//!   a packed array of `struct kinfo_proc` and hands back only a byte count, so
//!   reading it means hardcoding the struct's size — 648 bytes on 64-bit
//!   Darwin — and trusting that number forever. `libc` publishes
//!   `KERN_PROC_ALL` but not `kinfo_proc`, precisely because that layout is a
//!   private kernel detail. A magic stride that silently desynchronises is the
//!   exact failure mode this fix exists to remove.
//! * **reading one process** — `/proc/<pid>/{cmdline,environ}` on Linux,
//!   `sysctl KERN_PROCARGS2` on Darwin, which carries the executable path, the
//!   argv and the environment in one blob.
//!
//! Everything here is deliberately read-only. The one thing that kills lives in
//! `super`, behind the `is_test_flock_binary` filter, which requires the
//! executable to be this checkout's own `target/debug/flk` — so a sweep can
//! only ever match daemons built from the running worktree, never another
//! session's installed `flock`.

use std::io;
use std::path::PathBuf;

/// The identity of one process: what it is running, and nothing else.
///
/// Deliberately excludes the environment. The reap sweep runs on a 1 Hz
/// watchdog over every pid on the machine, calls this for all of them, and
/// reads the environment only for the handful that pass the executable-path and
/// `server` gates. Bundling the environment in here — which the first draft of
/// this module did — meant reading `/proc/<pid>/environ` for every process in
/// existence on Linux, which is a real syscall per pid per sweep for a file that
/// does not even exist for some of them. See [`process_environment`] for the
/// second call.
///
/// Each field degrades independently. A process whose argv could not be read
/// reports an empty `argv` rather than an error, which is what makes the
/// sweep's `argv.contains("server")` test reject it — the same thing
/// `/proc/<pid>/cmdline` failing used to do.
#[derive(Debug, Default, Clone)]
pub struct ProcessInfo {
    /// The executable the process was launched from, when the OS will say.
    pub exe_path: Option<PathBuf>,
    /// The argument vector, excluding the executable path.
    pub argv: Vec<String>,
}

/// The raw `KEY=VALUE` entries of one process's environment.
///
/// A separate call from [`process_info`] because it is only worth making once a
/// pid has been identified as a candidate: on Linux this is a second `/proc`
/// read, and on Darwin the blob is refetched — a syscall per *candidate*, not
/// per process.
///
/// An `Err` means the same thing it does there: the pid exited, or the kernel
/// will not say. Callers skip the pid.
pub fn process_environment(pid: u32) -> io::Result<Vec<String>> {
    imp::process_environment(pid)
}

/// Every process id visible to this process, excluding this process.
///
/// Returns an empty list where the platform offers no way to enumerate — the
/// sweep's callers already treat "nothing found" as "nothing to reap", so a
/// platform that cannot answer is not a sweep failure.
pub fn list_process_ids() -> io::Result<Vec<u32>> {
    imp::list_process_ids()
}

/// Reads the executable path and argv of one process.
///
/// An `Err` is always per-process and never fatal to a sweep: the pid exited
/// between enumeration and inspection, or the kernel will not tell us about it
/// (`ENOENT`, `EIO`, `EPERM`). Callers skip the pid and keep going.
pub fn process_info(pid: u32) -> io::Result<ProcessInfo> {
    imp::process_info(pid)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn list_process_ids() -> io::Result<Vec<u32>> {
        let entries = match std::fs::read_dir("/proc") {
            Ok(entries) => entries,
            // No procfs at all: nothing to enumerate, and every caller treats
            // an empty list as "nothing to reap".
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut pids = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            pids.push(pid);
        }
        Ok(pids)
    }

    /// Exactly the two reads the sweep used to make per pid, in the same order
    /// and no more: the exe link for every pid, and `cmdline` only once the exe
    /// has been resolved by the caller. `environ` is a separate call so it stays
    /// on the candidate side of that filter, where it was before this moved here.
    pub fn process_info(pid: u32) -> io::Result<ProcessInfo> {
        Ok(ProcessInfo {
            // A process we may not read the exe of (it exited, or it is not
            // ours) is simply one the exe filter will reject.
            exe_path: std::fs::read_link(format!("/proc/{pid}/exe")).ok(),
            argv: split_nul_separated(&read_best_effort(format!("/proc/{pid}/cmdline"))),
        })
    }

    pub fn process_environment(pid: u32) -> io::Result<Vec<String>> {
        Ok(split_nul_separated(&read_best_effort(format!(
            "/proc/{pid}/environ"
        ))))
    }

    fn read_best_effort(path: String) -> Vec<u8> {
        std::fs::read(path).unwrap_or_default()
    }

    fn split_nul_separated(bytes: &[u8]) -> Vec<String> {
        bytes
            .split(|byte| *byte == 0)
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    /// Generous first guess at the pid table. A Mac does not get anywhere near
    /// this many processes; the growth loop below covers the rest.
    const INITIAL_PID_CAPACITY: usize = 1 << 14;
    /// Bound on the growth loop. A pid table larger than this is not a machine
    /// this suite should be enumerating on.
    const MAX_PID_CAPACITY: usize = 1 << 20;

    pub fn list_process_ids() -> io::Result<Vec<u32>> {
        let mut capacity = INITIAL_PID_CAPACITY;

        loop {
            let mut buffer = vec![0_u8; capacity * std::mem::size_of::<libc::c_int>()];
            // SAFETY: the buffer is `capacity` zeroed pid-sized slots, and the
            // length handed to the kernel is exactly that many bytes.
            let written = unsafe {
                libc::proc_listallpids(buffer.as_mut_ptr().cast(), buffer.len() as libc::c_int)
            };

            if written < 0 {
                return Err(io::Error::last_os_error());
            }

            let written = written as usize;
            if written > buffer.len() {
                // The kernel is telling us how much room it needed. Resize and
                // ask again rather than truncating the table.
                if capacity >= MAX_PID_CAPACITY {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "proc_listallpids wanted more space than this sweep will allocate",
                    ));
                }
                capacity = capacity.saturating_mul(2);
                continue;
            }

            // `as_chunks` hands back the whole slots plus a remainder, so the
            // `..written` slice can never yield a short final chunk.
            let (slots, _) = buffer[..written].as_chunks::<4>();
            let pids: Vec<u32> = slots.iter().map(|slot| u32::from_ne_bytes(*slot)).collect();
            // `proc_listallpids` omits the calling process; the sweep skips its
            // own pid anyway, so nothing depends on that.
            return Ok(pids);
        }
    }

    /// One `KERN_PROCARGS2` fetch per call. The identity call and the
    /// environment call each fetch the whole blob, so a candidate costs two
    /// syscalls where the old bundled shape cost one — the price of not reading
    /// the environment for every pid on the machine, which is the more expensive
    /// mistake by a factor of the process count.
    pub fn process_info(pid: u32) -> io::Result<ProcessInfo> {
        Ok(parse_procargs(&read_procargs2(pid)?).identity)
    }

    pub fn process_environment(pid: u32) -> io::Result<Vec<String>> {
        Ok(parse_procargs(&read_procargs2(pid)?).environment)
    }

    /// `sysctl KERN_PROCARGS2` for one pid.
    ///
    /// The buffer layout is: a 4-byte little-endian `MAXARG` (the number of
    /// argv strings that follow the executable path), then the NUL-terminated
    /// executable path, then `MAXARG` NUL-terminated argv strings, then the
    /// NUL-terminated environment.
    ///
    /// Every failure here is per-process and the caller skips the pid. The
    /// kernel returns `ENOENT`/`EIO` for a process that exited underneath the
    /// enumeration and `EPERM` for one this uid may not inspect — none of which
    /// is a reason to abandon a sweep over the other six hundred processes.
    fn read_procargs2(pid: u32) -> io::Result<Vec<u8>> {
        if pid > i32::MAX as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pid does not fit in the sysctl mib",
            ));
        }

        // `libc` types the mib as `*mut` even though the kernel only reads it.
        let mut mib = [
            libc::CTL_KERN as libc::c_int,
            libc::KERN_PROCARGS2,
            pid as libc::c_int,
        ];

        let mut needed: libc::size_t = 0;
        // SAFETY: a null buffer with a real `len` is the documented way to ask
        // the kernel how large a `KERN_PROCARGS2` result is.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut needed,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if needed == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "KERN_PROCARGS2 reported an empty result",
            ));
        }

        let mut capacity = needed;
        for _ in 0..4 {
            let mut buffer = vec![0_u8; capacity];
            let mut len = buffer.len();
            // SAFETY: `buffer` is a live allocation of exactly `len` bytes and
            // the kernel is told that is all it may write.
            let rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as libc::c_uint,
                    buffer.as_mut_ptr().cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };

            if rc == 0 {
                // The kernel reports what it wrote, which can be less than the
                // capacity asked for. Truncating to it is what keeps the parser
                // below inside the bytes the kernel actually returned.
                buffer.truncate(len.min(buffer.len()));
                return Ok(buffer);
            }

            let err = io::Error::last_os_error();
            // The process grew its argv/environ between the two calls. Ask for
            // more room once more rather than losing the process.
            if err.raw_os_error() == Some(libc::ENOMEM) {
                capacity = capacity.saturating_mul(2);
                continue;
            }
            return Err(err);
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KERN_PROCARGS2 kept outgrowing the buffer",
        ))
    }
}

/// Parses a `KERN_PROCARGS2` blob into executable path, argv and environment.
///
/// Platform-independent and side-effect-free on purpose: the interesting
/// failures here are malformed blobs, which is something a test can construct
/// but the kernel never usefully produces, so the parse is written against
/// slices and bounds everywhere.
///
/// The one structural fact it leans on is `MAXARG`, the count of argv strings
/// that follow the executable path. Measured on Darwin 25.4.0: `sleep 20` has
/// `MAXARG == 2` for an argv of `["sleep", "20"]`, so the executable path is
/// NOT counted.
///
/// `MAXARG` comes from another process, so it is used as a *cap* on a walk that
/// is already bounded by the blob, never as an index into it. An over-large
/// count therefore cannot read past what the kernel returned: the argv region
/// simply absorbs the remaining strings and the environment comes back empty,
/// which makes the sweep skip the process rather than mis-attribute it. A
/// count that is too small loses the environment the same way. Both directions
/// cost the sweep a process it might have reaped and buy it no wrong answer,
/// which is the trade worth making when the number is not ours.
///
/// "No wrong answer" is specific to the environment, which is a keyed lookup on
/// `XDG_RUNTIME_DIR`. The argv side is looser: with an over-large count the
/// `server` gate is applied to environment strings too, so a process carrying
/// a bare `server` entry in its environment would pass it. That cannot cause a
/// wrong kill — the executable-path filter runs first and admits only this
/// checkout's own `flk` — but it is why this is described as skipping rather
/// than as closing.
fn parse_procargs(blob: &[u8]) -> ParsedProcargs {
    const HEADER: usize = std::mem::size_of::<u32>();

    if blob.len() < HEADER {
        return ParsedProcargs::default();
    }

    let max_arg = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;

    // Split the body on NULs once. Every string the blob contains is in here,
    // so no later step can read a byte the kernel did not return, and no step
    // can be tricked by a `MAXARG` that disagrees with the blob's length.
    let strings: Vec<&[u8]> = blob[HEADER..]
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .collect();

    // strings[0] is the executable path; `strings[1..]` starts at argv[0].
    // A blob with no executable path has no argv worth trusting either.
    let Some((exe_path, rest)) = strings.split_first() else {
        return ParsedProcargs::default();
    };

    let argv_end = rest.len().min(max_arg);
    let (argv, environment) = rest.split_at(argv_end);

    ParsedProcargs {
        identity: ProcessInfo {
            exe_path: Some(PathBuf::from(
                String::from_utf8_lossy(exe_path).into_owned(),
            )),
            argv: to_strings(argv),
        },
        environment: to_strings(environment),
    }
}

/// Both halves of a parsed blob.
///
/// The Darwin caller wants the identity for every pid but the environment only
/// for candidates, and a blob is all-or-nothing: it arrives with both halves in
/// it. So the parse returns both and the two public entry points pick. The
/// platform-independent parser stays one function rather than growing a
/// boundary argument whose only job would be to be threaded through.
#[derive(Debug, Default)]
struct ParsedProcargs {
    identity: ProcessInfo,
    environment: Vec<String>,
}

fn to_strings(chunks: &[&[u8]]) -> Vec<String> {
    chunks
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `KERN_PROCARGS2` blob the way the kernel does: a little-endian
    /// `MAXARG`, then the executable path, argv and environment as
    /// NUL-terminated strings.
    fn blob(max_arg: u32, exec_path: &str, argv: &[&str], environment: &[&str]) -> Vec<u8> {
        let mut bytes = max_arg.to_le_bytes().to_vec();
        for part in exec_path
            .bytes()
            .chain([0])
            .chain(argv.iter().flat_map(|arg| arg.bytes().chain([0])))
            .chain(environment.iter().flat_map(|var| var.bytes().chain([0])))
        {
            bytes.push(part);
        }
        bytes.push(0);
        bytes
    }

    /// Look a key up in a parsed blob's environment, the way the sweep's
    /// `runtime_dir_for` does, without going back to the kernel.
    fn env_value(parsed: &ParsedProcargs, key: &str) -> Option<String> {
        parsed.environment.iter().find_map(|entry| {
            entry
                .split_once('=')
                .filter(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn parses_exec_path_argv_and_environment() {
        let bytes = blob(
            2,
            "/checkout/target/debug/flk",
            &["flk", "server"],
            &[
                "XDG_RUNTIME_DIR=/run/flock",
                "FLOCK_SOCKET_PATH=/run/flock/flock.sock",
            ],
        );

        let parsed = parse_procargs(&bytes);

        assert_eq!(
            parsed.identity.exe_path,
            Some(PathBuf::from("/checkout/target/debug/flk"))
        );
        assert_eq!(
            parsed.identity.argv,
            vec!["flk".to_string(), "server".to_string()]
        );
        assert_eq!(
            env_value(&parsed, "XDG_RUNTIME_DIR"),
            Some("/run/flock".to_string())
        );
        assert_eq!(
            env_value(&parsed, "FLOCK_SOCKET_PATH"),
            Some("/run/flock/flock.sock".to_string())
        );
        assert_eq!(env_value(&parsed, "PATH"), None);
    }

    /// The layout a process that has NOT finished `execve` presents, measured
    /// on Darwin 25.4.0: `argv` is already the final argv and `MAXARG` already
    /// equals its length, but the environment is absent from the blob entirely.
    ///
    /// It matters because it is what the reap sweep sees for a process in the
    /// microseconds after spawning. The environment comes back empty, which is
    /// the safe direction: the sweep cannot attribute the process to a runtime
    /// dir, skips it, and catches it on the next pass. A parser that guessed
    /// the boundary differently here would instead read the first environment
    /// entry as an argv element.
    #[test]
    fn pre_exec_shaped_blob_reports_no_environment() {
        let bytes = blob(3, "/bin/sh", &["/bin/sh", "-c", "sleep 30"], &[]);

        let parsed = parse_procargs(&bytes);

        assert_eq!(parsed.identity.argv, vec!["/bin/sh", "-c", "sleep 30"]);
        assert!(
            parsed.environment.is_empty(),
            "a process mid-exec has no environment to attribute it with"
        );
    }

    /// An over-large `MAXARG` — the blob is shorter than the count claims — must
    /// be clamped to the blob. It cannot read past the bytes the kernel
    /// returned, and it fails closed: the strings it does report are the ones
    /// present, with the environment reported empty rather than reconstructed
    /// out of bounds.
    #[test]
    fn oversized_max_arg_is_clamped_to_the_blob() {
        let bytes = blob(
            4_000,
            "/flk",
            &["flk", "server"],
            &["XDG_RUNTIME_DIR=/run/flock"],
        );

        let parsed = parse_procargs(&bytes);

        assert_eq!(parsed.identity.exe_path, Some(PathBuf::from("/flk")));
        assert_eq!(
            parsed.identity.argv.len() + parsed.environment.len(),
            3,
            "every string in the blob is reported exactly once"
        );
        assert_eq!(
            env_value(&parsed, "XDG_RUNTIME_DIR"),
            None,
            "an over-large MAXARG hides the environment instead of inventing one"
        );
    }

    #[test]
    fn zero_max_arg_reports_no_argv_and_leaves_the_environment_intact() {
        let bytes = blob(0, "/flk", &[], &["XDG_RUNTIME_DIR=/run/flock"]);

        let parsed = parse_procargs(&bytes);

        assert!(parsed.identity.argv.is_empty());
        assert_eq!(
            env_value(&parsed, "XDG_RUNTIME_DIR"),
            Some("/run/flock".to_string())
        );
    }

    /// Truncation at every length, which is what a process exiting mid-`sysctl`
    /// looks like. Nothing may panic.
    #[test]
    fn truncated_blobs_never_panic() {
        let bytes = blob(
            2,
            "/checkout/target/debug/flk",
            &["flk", "server"],
            &["XDG_RUNTIME_DIR=/run/flock", "TERM=xterm-256color"],
        );

        for len in 0..=bytes.len() {
            let _ = parse_procargs(&bytes[..len]);
        }
    }

    #[test]
    fn header_only_and_empty_blobs_are_not_parsed() {
        assert_eq!(parse_procargs(&[]).identity.argv, Vec::<String>::new());
        assert_eq!(parse_procargs(&[]).identity.exe_path, None);
        assert_eq!(parse_procargs(&[1, 2, 3]).identity.exe_path, None);
        assert_eq!(parse_procargs(&[1, 0, 0, 0]).identity.exe_path, None);
    }

    /// Non-UTF-8 must not lose the rest of the blob — the environment is
    /// recovered lossily and the sweep still gets its keys.
    #[test]
    fn non_utf8_strings_do_not_swallow_the_environment() {
        let mut bytes = blob(1, "/flk", &["flk"], &["XDG_RUNTIME_DIR=/run/flock"]);
        bytes.extend_from_slice(&[0xff, 0xfe, 0]);

        let parsed = parse_procargs(&bytes);

        assert_eq!(
            env_value(&parsed, "XDG_RUNTIME_DIR"),
            Some("/run/flock".to_string())
        );
    }

    #[test]
    fn environment_value_ignores_a_bare_key_and_a_key_prefix() {
        let parsed = ParsedProcargs {
            identity: ProcessInfo::default(),
            environment: vec![
                "XDG_RUNTIME_DIR".to_string(),
                "XDG_RUNTIME_DIR_EXTRA=nope".to_string(),
                "=leading-equals".to_string(),
            ],
        };

        assert_eq!(env_value(&parsed, "XDG_RUNTIME_DIR"), None);
    }

    /// The environment of THIS process, read back the way the sweep reads a
    /// daemon's.
    ///
    /// Self, rather than a spawned child, and the reason is a Darwin fact that
    /// cost this test a rewrite to find: macOS withholds the environment from
    /// process introspection for **Apple platform binaries**. Measured on
    /// Darwin 25.4.0 — `/bin/sh`, `/bin/sleep` and everything else under
    /// `/bin`, `/usr/bin`, `/usr/sbin` and `/usr/libexec` report zero
    /// environment entries from `KERN_PROCARGS2`, while third-party binaries
    /// (`/nix/store/...`, `node`, python, and this repository's own
    /// `target/debug/flk`) report theirs. `ps eww` agrees, so it is the kernel
    /// withholding the block rather than a decoding bug. A full scan of a Mac
    /// splits cleanly along the same line: the pids with no environment are
    /// exactly the Apple agents (`launchd`, `distnoted`, `lsd`, `Finder`,
    /// `Dock`).
    ///
    /// A Rust test binary is not a platform binary, so introspecting self works
    /// here and on Linux identically — no `#[cfg]`, and no dependence on which
    /// binaries happen to be installed on the developer's machine.
    #[test]
    fn reads_back_this_process_environment() {
        let me = std::process::id();
        let environment = process_environment(me).expect("introspect this process's environment");

        assert!(
            !environment.is_empty(),
            "the sweep reads a server's environment to decide which test owns it, and \
             this process reported none"
        );

        // Every variable this process can see in-process is reported by the
        // kernel for the same pid, with the same value. This is the direction
        // that has to hold: the kernel adds entries of its own on Darwin
        // (`th_port`, `security_config`, `arm64e_abi`, `executable_cdhash`), so
        // the kernel set is a superset and set equality would be the wrong
        // assertion.
        let lookup = |key: &str| {
            environment.iter().find_map(|entry| {
                entry
                    .split_once('=')
                    .filter(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_string())
            })
        };

        let mut disagreements = Vec::new();
        for (key, value) in std::env::vars_os() {
            let key = key.to_string_lossy().into_owned();
            match lookup(&key) {
                Some(seen) if seen == value.to_string_lossy() => {}
                Some(_) => disagreements.push(format!("{key} (value differs)")),
                None => disagreements.push(format!("{key} (absent)")),
            }
        }
        assert!(
            disagreements.is_empty(),
            "the environment read disagreed with the process's own view of it: \
             {disagreements:?}"
        );
    }

    /// The pid this test spawned is visible to enumeration, and its identity —
    /// executable path and argv — reads back. These are the two calls the sweep
    /// makes per pid before it decides anything about that pid.
    ///
    /// No environment assertion here, and that is a measured constraint rather
    /// than an omission: `/bin/sh` is an Apple platform binary and reports none.
    /// The environment half of the sweep is covered by
    /// `reads_back_this_process_environment`, against a process that reports
    /// one.
    ///
    /// `/bin/sh` is also the only interpreter POSIX guarantees, which is why
    /// this spawns it rather than something more convenient — NixOS ships
    /// nothing else in `/bin`.
    #[allow(clippy::disallowed_methods)]
    // Test harness spawn — TracedCommand polices product code, and crate::process is unreachable from an integration test binary.
    #[test]
    fn enumerates_and_inspects_a_process_this_test_spawned() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let pid = child.id();

        let listed = list_process_ids().expect("enumerate processes");
        assert!(
            listed.contains(&pid),
            "the sweep's enumeration missed the pid this test spawned"
        );

        // Polled, not read once. A process that has not finished `execve` has no
        // argv to report — Linux's `/proc/<pid>/cmdline` is empty until the exec
        // lands, and on Darwin the blob carries no argv either (see
        // `pre_exec_shaped_blob_reports_no_environment`). The sweep is a
        // snapshot, so it simply sees a process it cannot identify and moves on;
        // a single-shot read here would be testing spawn-versus-inspect timing
        // rather than the read.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut observed: Option<ProcessInfo> = None;
        while std::time::Instant::now() < deadline {
            let info = process_info(pid).expect("inspect spawned process");
            if !info.argv.is_empty() && info.exe_path.is_some() {
                observed = Some(info);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let Some(info) = observed else {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the sweep reads argv and the executable path to decide what a \
                     process is, and this one never reported either"
            );
        };

        let exe = info.exe_path.expect("checked above");
        assert!(
            exe.is_absolute() && exe.exists(),
            "the reported executable should be a real absolute path, got {}",
            exe.display()
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A pid that is not running must never be attributable to a test. On
    /// Darwin that arrives as an `Err` (`ENOENT`/`EIO`/`EPERM` from
    /// `KERN_PROCARGS2`) and on Linux as a `ProcessInfo` with nothing in it,
    /// because the `/proc` reads fail one at a time. Both are safe: the sweep
    /// skips the pid either way.
    #[test]
    fn a_pid_that_is_not_running_is_never_attributable() {
        // A pid well past any plausible pid_max, so this does not race another
        // process appearing. Skip rather than assert if the machine disagrees.
        let candidate = u32::MAX - 1;
        let Ok(listed) = list_process_ids() else {
            return;
        };
        if listed.contains(&candidate) {
            return;
        }

        let Ok(info) = process_info(candidate) else {
            return;
        };
        assert!(
            info.argv.is_empty() && info.exe_path.is_none(),
            "a pid with no process behind it reported {:?}",
            info
        );
    }
}
