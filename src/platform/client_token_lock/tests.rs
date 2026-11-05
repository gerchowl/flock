use super::*;
use crate::structured::launch::SessionConfigDir;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
fn executable() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("flk{}", std::env::consts::EXE_SUFFIX))
}
fn fixture() -> (SessionConfigDir, std::path::PathBuf, File) {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let path = root.path().join("lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.lock().unwrap();
    (root, path, file)
}
fn open(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}
fn reaped(pid: u32) {
    // SAFETY: nonblocking wait only checks the helper this test previously owned.
    assert_eq!(
        unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
#[test]
fn contended_worker_acquisition_keeps_the_parent_descriptor_locked() {
    let (_root, path, holder) = fixture();
    let pending = PendingLock::begin(open(&path), &executable()).unwrap();
    let pid = pending.helper.as_ref().unwrap().0.id();
    drop(holder);
    let guard = pending.finish(Duration::from_secs(5)).unwrap();
    reaped(pid);
    assert!(matches!(
        open(&path).try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    drop(guard);
    assert!(open(&path).try_lock().is_ok());
}
#[test]
fn repeated_timeouts_and_abandonment_reap_every_owned_worker() {
    let (_root, path, holder) = fixture();
    for _ in 0..3 {
        let pending = PendingLock::begin(open(&path), &executable()).unwrap();
        let pid = pending.helper.as_ref().unwrap().0.id();
        assert!(
            matches!(pending.finish(Duration::from_millis(20)),Err(e) if e.kind()==io::ErrorKind::TimedOut)
        );
        reaped(pid);
    }
    let pending = PendingLock::begin(open(&path), &executable()).unwrap();
    let pid = pending.helper.as_ref().unwrap().0.id();
    drop(pending);
    reaped(pid);
    drop(holder);
    assert!(PendingLock::begin(open(&path), &executable())
        .unwrap()
        .finish(Duration::from_secs(5))
        .is_ok());
}
#[test]
fn dead_and_live_silent_workers_fail_boundedly_and_are_reaped() {
    let (root, path, _holder) = fixture();
    let mut pending = PendingLock::begin(open(&path), &executable()).unwrap();
    let pid = pending.helper.as_ref().unwrap().0.id();
    pending.helper.as_mut().unwrap().0.kill().unwrap();
    assert!(pending.finish(Duration::from_secs(5)).is_err());
    reaped(pid);
    let silent = root.path().join("silent");
    fs::write(&silent, "#!/bin/sh\nprintf S\nIFS= read -r ignored <&1\n").unwrap();
    fs::set_permissions(&silent, fs::Permissions::from_mode(0o700)).unwrap();
    let pending = PendingLock::begin(open(&path), &silent).unwrap();
    let pid = pending.helper.as_ref().unwrap().0.id();
    assert!(
        matches!(pending.finish(Duration::from_millis(20)),Err(e) if e.kind()==io::ErrorKind::TimedOut)
    );
    reaped(pid);
}
#[test]
fn parent_ipc_disconnection_terminates_the_waiter_without_lock_release() {
    let (_root, path, _holder) = fixture();
    let mut pending = PendingLock::begin(open(&path), &executable()).unwrap();
    let (mut child, stream) = pending.helper.take().unwrap();
    let pid = child.id();
    drop(stream);
    let (tx, rx) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: observes this still-owned child without consuming its status.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        tx.send(result).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(5));
    if result.is_err() {
        child.kill().unwrap();
    }
    observer.join().unwrap();
    child.wait().unwrap();
    assert_eq!(result.unwrap(), 0);
    reaped(pid);
}

#[test]
fn finished_and_abandoned_pending_locks_release_before_inherited_descriptors_close() {
    for finish in [true, false] {
        let (_root, path, holder) = fixture();
        drop(holder);
        let pending = PendingLock::begin(open(&path), &executable()).unwrap();
        let mut inherited =
            inherited_holder::Holder::spawn(pending.file.as_ref().unwrap()).unwrap();
        if finish {
            drop(pending.finish(Duration::from_secs(5)).unwrap());
        } else {
            drop(pending);
        }
        let result = open(&path).try_lock();
        inherited.verify().unwrap();
        assert!(
            result.is_ok(),
            "pending finish={finish} lock retained by child pid={} descriptor={} inode={}",
            inherited.pid,
            inherited.descriptor,
            inherited.inode
        );
    }
}

/// Simulate a cold exec that needs longer than the lock budget before its first byte.
struct SlowStartup {
    timeout: std::cell::Cell<Duration>,
    started: std::rc::Rc<std::cell::Cell<bool>>,
    observed: std::rc::Rc<std::cell::Cell<bool>>,
}
impl HelperStream for SlowStartup {
    fn read_byte(&mut self, timeout: Duration) -> io::Result<u8> {
        self.timeout.set(timeout);
        if !self.started.get() {
            assert!(!self.observed.get(), "on_wait preceded helper startup");
            if self.timeout.get() < Duration::from_millis(60) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.started.set(true);
            Ok(b'S')
        } else {
            assert!(self.observed.get(), "lock read preceded on_wait");
            assert_eq!(self.timeout.get(), Duration::from_millis(20));
            Ok(b'L')
        }
    }
}
#[test]
fn slow_helper_start_does_not_consume_the_lock_read_budget() {
    let started = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut stream = SlowStartup {
        timeout: std::cell::Cell::new(Duration::ZERO),
        started: started.clone(),
        observed: observed.clone(),
    };
    let result = wait_for_lock(
        &mut stream,
        WaitBudget::Lock(Duration::from_millis(20)),
        || {
            assert!(started.get(), "on_wait preceded helper startup");
            observed.set(true);
            Ok(())
        },
    );
    assert!(
        result.is_ok(),
        "startup latency consumed lock budget: {result:?}"
    );
    assert!(observed.get());
}

#[test]
fn both_real_helpers_acknowledge_start_before_waiting_for_the_lock() {
    for shared in [false, true] {
        let (_root, path, holder) = fixture();
        let mut pending = PendingLock::begin_with(open(&path), &executable(), shared).unwrap();
        let (helper, stream) = pending.helper.as_mut().unwrap();
        let pid = helper.id();
        read_ack(stream, HELPER_STARTUP_TIMEOUT, b'S').unwrap();
        assert!(matches!(
            open(&path).try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(holder);
        read_ack(stream, Duration::from_secs(2), b'L').unwrap();
        drop(pending);
        reaped(pid);
        assert!(open(&path).try_lock().is_ok());
    }
}

/// Scripted IPC failures exercise both phases without delaying a real process.
struct Scripted {
    bytes: std::io::Cursor<Vec<u8>>,
    timeouts: std::cell::RefCell<Vec<Duration>>,
}
impl HelperStream for Scripted {
    fn read_byte(&mut self, timeout: Duration) -> io::Result<u8> {
        self.timeouts.borrow_mut().push(timeout);
        if self.bytes.position() == self.bytes.get_ref().len() as u64 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut byte = [0];
        self.bytes.read_exact(&mut byte)?;
        Ok(byte[0])
    }
}
#[test]
fn startup_and_lock_protocol_failures_keep_their_bounds_and_callback_order() {
    for (bytes, expected, callback) in [
        (vec![], io::ErrorKind::TimedOut, false),
        (vec![b'?'], io::ErrorKind::InvalidData, false),
        (vec![b'S'], io::ErrorKind::TimedOut, true),
        (vec![b'S', b'?'], io::ErrorKind::InvalidData, true),
    ] {
        let mut stream = Scripted {
            bytes: std::io::Cursor::new(bytes),
            timeouts: std::cell::RefCell::default(),
        };
        let observed = std::cell::Cell::new(false);
        let result = wait_for_lock(
            &mut stream,
            WaitBudget::Lock(Duration::from_secs(2)),
            || {
                observed.set(true);
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().kind(), expected);
        assert_eq!(observed.get(), callback);
        let timeouts = stream.timeouts.borrow();
        assert_eq!(timeouts[0], HELPER_STARTUP_TIMEOUT);
        if callback {
            assert_eq!(timeouts[1], Duration::from_secs(2));
        }
    }
}
#[test]
fn expired_absolute_budget_never_starts_another_helper_read() {
    let mut stream = Scripted {
        bytes: std::io::Cursor::new(vec![b'S', b'L']),
        timeouts: std::cell::RefCell::default(),
    };
    let result = wait_for_lock(&mut stream, WaitBudget::Until(Instant::now()), || {
        panic!("expired hello called on_wait")
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(stream.timeouts.borrow().is_empty());
}

#[test]
fn expired_absolute_budget_also_refuses_an_uncontended_lock() {
    let (_root, path, holder) = fixture();
    drop(holder);
    let pending = PendingLock::begin(open(&path), &executable()).unwrap();
    assert!(pending.helper.is_none());
    let result = pending.finish_until(Instant::now(), || panic!("uncontended callback"));
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    assert!(open(&path).try_lock().is_ok());
}

#[test]
fn closed_peer_buffered_lock_ack_is_still_accepted() {
    let (mut parent, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"SL").unwrap();
    let result = wait_for_lock(
        &mut parent,
        WaitBudget::Lock(Duration::from_secs(2)),
        move || {
            peer.shutdown(std::net::Shutdown::Both)?;
            drop(peer);
            Ok(())
        },
    );
    assert!(
        result.is_ok(),
        "closed helper lost its buffered L: {result:?}"
    );
}

#[test]
fn peer_closed_before_start_read_keeps_both_acknowledgements() {
    let (mut parent, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"SL").unwrap();
    peer.shutdown(std::net::Shutdown::Both).unwrap();
    drop(peer);
    assert!(wait_for_lock(
        &mut parent,
        WaitBudget::Lock(Duration::from_secs(2)),
        || Ok(())
    )
    .is_ok());
}

#[test]
fn real_socket_silence_and_early_eof_remain_distinct() {
    let (mut parent, mut peer) = UnixStream::pair().unwrap();
    assert_eq!(
        parent
            .read_byte(Duration::from_millis(20))
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    peer.write_all(b"S").unwrap();
    drop(peer);
    let result = wait_for_lock(
        &mut parent,
        WaitBudget::Lock(Duration::from_secs(2)),
        || Ok(()),
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}
