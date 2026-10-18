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
fn fixture() -> (SessionConfigDir, std::path::PathBuf, LockFile) {
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
    let file = LockFile::new(file);
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
        LockFile::new(open(&path)).try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    drop(guard);
    assert!(LockFile::new(open(&path)).try_lock().is_ok());
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
    fs::write(&silent, "#!/bin/sh\nIFS= read -r ignored <&1\n").unwrap();
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
fn uncontended_finished_and_abandoned_pending_locks_release_before_inherited_descriptors_close() {
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
        let result = LockFile::new(open(&path)).try_lock();
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

#[test]
fn contended_handoff_keeps_lock_until_parent_drop_with_inherited_holder() {
    let (_root, path, owner) = fixture();
    let mut pending = PendingLock::begin(open(&path), &executable()).unwrap();
    let child = &mut pending.helper.as_mut().expect("real contended helper").0;
    let pid = child.id();
    let mut inherited = inherited_holder::Holder::spawn(pending.file.as_ref().unwrap()).unwrap();
    drop(owner);
    // Observe natural helper exit without reaping it: cancellation can still
    // kill only this owned child, whose pid cannot be reused before wait().
    let (tx, rx) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: observe this owned child and retain its waitable status.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        let _ = tx.send(result);
    });
    let exited = rx.recv_timeout(Duration::from_secs(5));
    if exited.is_err() {
        pending.helper.as_mut().unwrap().0.kill().unwrap();
    }
    observer.join().unwrap();
    assert_eq!(
        exited.unwrap(),
        0,
        "helper must exit after acknowledged transfer"
    );
    let guard = pending.finish(Duration::from_secs(5)).unwrap();
    reaped(pid);
    assert!(
        matches!(
            LockFile::new(open(&path)).try_lock(),
            Err(TryLockError::WouldBlock)
        ),
        "worker must not unlock the parent's transferred lock"
    );
    inherited.verify().unwrap();
    drop(guard);
    let acquired = LockFile::new(open(&path)).try_lock();
    inherited.verify().unwrap();
    assert!(
        acquired.is_ok(),
        "parent drop must unlock despite the inherited holder"
    );
}
