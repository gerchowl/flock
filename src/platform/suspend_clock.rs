//! Suspend-inclusive, non-settable clock and absolute reactor timers (#2489).
//! Timers become ready after resume; they do not request a hardware wakeup.
#[cfg(any(feature = "web", test))]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::{io, time::Duration};

fn invalid() -> io::Error {
    io::Error::other("suspend-inclusive clock conversion failed")
}

#[cfg(target_os = "linux")]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_BOOTTIME;
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}
#[cfg(target_os = "macos")]
fn timebase() -> io::Result<Timebase> {
    let mut info = Timebase { numer: 0, denom: 0 };
    // SAFETY: the kernel writes exactly one live ABI-compatible timebase struct.
    if unsafe { mach_timebase_info(&mut info) } != 0 || info.numer == 0 || info.denom == 0 {
        return Err(invalid());
    }
    Ok(info)
}

/// Native source identifier, passed unchanged to the syscall boundary.
#[cfg(target_os = "linux")]
pub(crate) type ReadSource = libc::clockid_t;
/// Native tick reader, passed unchanged to the syscall boundary.
#[cfg(target_os = "macos")]
pub(crate) type ReadSource = unsafe extern "C" fn() -> u64;
/// Syscall boundary shared by production and deterministic source witnesses.
pub(crate) trait Read: Send + Sync {
    /// Read the selected source, without choosing a different clock.
    fn read(&self, source: ReadSource) -> io::Result<Duration>;
}
/// Native kernel implementation of the injectable syscall boundary.
#[derive(Default)]
pub(crate) struct Kernel;
impl Read for Kernel {
    fn read(&self, source: ReadSource) -> io::Result<Duration> {
        #[cfg(target_os = "linux")]
        {
            let mut value = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime writes one live timespec for the selected clock.
            if unsafe { libc::clock_gettime(source, &mut value) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Duration::new(
                value.tv_sec.try_into().map_err(|_| invalid())?,
                value.tv_nsec.try_into().map_err(|_| invalid())?,
            ))
        }
        #[cfg(target_os = "macos")]
        {
            let info = timebase()?;
            // SAFETY: mach_continuous_time takes no pointers and returns continuous ticks.
            let nanos =
                u128::from(unsafe { source() }) * u128::from(info.numer) / u128::from(info.denom);
            Ok(Duration::from_nanos(
                nanos.try_into().map_err(|_| invalid())?,
            ))
        }
    }
}
/// Time since boot including suspend, immune to wall-clock corrections.
/// Unsupported kernel calls and conversion overflow fail closed without a clock fallback.
pub(crate) fn now(reader: &impl Read) -> io::Result<Duration> {
    #[cfg(target_os = "linux")]
    {
        reader.read(CLOCK_ID)
    }
    #[cfg(target_os = "macos")]
    {
        reader.read(mach_continuous_time)
    }
}

#[cfg(any(feature = "web", test))]
fn timer(deadline: Duration) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    // SAFETY: timerfd_create consumes scalar flags and returns a fresh owned descriptor.
    let raw = unsafe { libc::timerfd_create(CLOCK_ID, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK) };
    #[cfg(target_os = "macos")]
    // SAFETY: kqueue takes no pointers and returns a fresh owned descriptor.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh valid descriptor with exactly one owner.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(target_os = "linux")]
    {
        let value = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: deadline.as_secs().try_into().map_err(|_| invalid())?,
                tv_nsec: deadline.subsec_nanos().into(),
            },
        };
        // SAFETY: the single initialized input is live; no old timer output is requested.
        if unsafe {
            libc::timerfd_settime(
                fd.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &value,
                std::ptr::null_mut(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: F_SETFD consumes a scalar flag for this owned descriptor.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let info = timebase()?;
        let ticks = deadline
            .as_nanos()
            .checked_mul(u128::from(info.denom))
            .ok_or_else(invalid)?
            .div_ceil(u128::from(info.numer));
        let event = libc::kevent {
            ident: 1,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
            fflags: libc::NOTE_ABSOLUTE | libc::NOTE_MACHTIME | libc::NOTE_MACH_CONTINUOUS_TIME,
            data: ticks.try_into().map_err(|_| invalid())?,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: the one initialized change is live; no event output is requested.
        if unsafe {
            libc::kevent(
                fd.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fd)
}

/// Await an absolute suspend-inclusive deadline without polling or blocking a worker.
#[cfg(any(feature = "web", test))]
pub(crate) async fn until(deadline: Duration) -> io::Result<()> {
    if deadline.is_zero() {
        return Ok(());
    }
    let descriptor = timer(deadline)?;
    // SAFETY: AsyncFd owns the immutable descriptor for the entire registration.
    let fd = unsafe {
        tokio::io::unix::AsyncFd::register_with_interest(descriptor, tokio::io::Interest::READABLE)?
    };
    drop(fd.readable().await?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ObservedKernel(std::sync::atomic::AtomicUsize);
    impl Read for ObservedKernel {
        fn read(&self, source: ReadSource) -> io::Result<Duration> {
            #[cfg(target_os = "linux")]
            assert_eq!(source, libc::CLOCK_BOOTTIME);
            #[cfg(target_os = "macos")]
            assert_eq!(source as usize, mach_continuous_time as *const () as usize);
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Kernel.read(source)
        }
    }
    #[test]
    fn platform_source_is_suspend_inclusive_and_nondecreasing() {
        use crate::structured::permissions::auth::tokens::app_sessions::{Clock, SystemClock};
        let clock = SystemClock(ObservedKernel(std::sync::atomic::AtomicUsize::new(0)));
        let first = clock.now().unwrap();
        let second = clock.now().unwrap();
        assert!(second >= first);
        assert_eq!(clock.0 .0.load(std::sync::atomic::Ordering::SeqCst), 2);
        #[cfg(target_os = "macos")]
        {
            let info = timebase().unwrap();
            // SAFETY: the independent source sample takes no pointers.
            let before = unsafe { mach_continuous_time() };
            let observed = clock.now().unwrap().as_nanos();
            // SAFETY: the independent source sample takes no pointers.
            let after = unsafe { mach_continuous_time() };
            assert!(
                observed >= u128::from(before) * u128::from(info.numer) / u128::from(info.denom)
            );
            assert!(
                observed <= u128::from(after) * u128::from(info.numer) / u128::from(info.denom)
            );
        }
    }

    #[test]
    fn future_continuous_timer_stays_pending_without_sleeping() {
        let deadline = now(&Kernel).unwrap() + Duration::from_secs(3600);
        let descriptor = timer(deadline).unwrap();
        let mut probe = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the borrowed descriptor and single output live through this nonblocking call.
        assert_eq!(unsafe { libc::poll(&mut probe, 1, 0) }, 0);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let waiting = until(deadline);
                tokio::pin!(waiting);
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                // Drive the reactor using an expired timer, without waiting for the future one.
                tokio::time::timeout(Duration::from_secs(5), until(now(&Kernel).unwrap()))
                    .await
                    .unwrap()
                    .unwrap();
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            });
    }

    #[test]
    fn future_continuous_timer_fires_through_production_reactor() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let deadline = now(&Kernel).unwrap() + Duration::from_millis(1);
                tokio::time::timeout(Duration::from_secs(5), until(deadline))
                    .await
                    .expect("native timer must become readable through the reactor")
                    .unwrap();
                assert!(now(&Kernel).unwrap() >= deadline);
            });
    }

    #[test]
    fn absolute_continuous_timer_is_kernel_ready_without_sleeping() {
        let deadline = now(&Kernel).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), until(deadline))
                    .await
                    .unwrap()
                    .unwrap();
            });
    }
}
