//! Global registry of active PTY terminals used to send SIGTERM to every
//! foreground process group when the Zed process is about to abort due to a
//! panic. See `crashes::set_panic_cleanup` for the wiring.

#[cfg(unix)]
mod imp {
    use parking_lot::Mutex;
    use std::os::fd::RawFd;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy)]
    struct Entry {
        id: u64,
        pty_fd: RawFd,
        fallback_pid: libc::pid_t,
    }

    static REGISTRY: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| Mutex::new(Vec::new()));
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    pub struct Registration(u64);

    impl Drop for Registration {
        fn drop(&mut self) {
            REGISTRY.lock().retain(|e| e.id != self.0);
        }
    }

    pub fn register(pty_fd: RawFd, fallback_pid: u32) -> Registration {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        REGISTRY.lock().push(Entry {
            id,
            pty_fd,
            fallback_pid: fallback_pid as libc::pid_t,
        });
        Registration(id)
    }

    /// Called from the panic hook. Sends SIGTERM to the foreground process
    /// group of every registered terminal, then polls up to `max_wait` for
    /// those groups to drain. No GPUI access; only touches the internal
    /// mutex, the libc layer, and the current thread.
    pub fn broadcast_sigterm_and_wait(max_wait: Duration) {
        // Snapshot under lock, then drop the guard (temporary lifetime ends
        // at `;`) so signaling and polling below run unlocked.
        let entries: Vec<Entry> = REGISTRY.lock().clone();

        let mut pgids: Vec<libc::pid_t> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let pgid = unsafe { libc::tcgetpgrp(entry.pty_fd) };
            let target = if pgid > 0 { pgid } else { entry.fallback_pid };
            if target > 0 {
                unsafe {
                    libc::killpg(target, libc::SIGTERM);
                }
                pgids.push(target);
            }
        }

        if pgids.is_empty() {
            return;
        }

        let deadline = Instant::now() + max_wait;
        let poll = Duration::from_millis(200);
        while Instant::now() < deadline {
            let any_alive = pgids
                .iter()
                .any(|pgid| unsafe { libc::killpg(*pgid, 0) == 0 });
            if !any_alive {
                break;
            }
            std::thread::sleep(poll);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::time::Duration;

    pub struct Registration;

    pub fn register(_pty_fd: i32, _fallback_pid: u32) -> Registration {
        Registration
    }

    pub fn broadcast_sigterm_and_wait(_max_wait: Duration) {}
}

pub use imp::*;
