//! Child processes that own a process group, so that stopping one stops its
//! descendants too (the C9 lesson for `cmd` inputs, reused by the credit probe
//! and the credit hook, docs/specs/auto-interval.md S7).
//!
//! Invariants: a [`GroupGuard`] that is armed SIGKILLs the whole group when it
//! drops, so dropping the future that owns a child never leaves a descendant
//! behind. The child is its own session leader (`setsid`), so `pgid == pid`.

use std::os::unix::process::CommandExt;

mod sys {
    unsafe extern "C" {
        pub fn setsid() -> i32;
        pub fn kill(pid: i32, signal: i32) -> i32;
    }
    pub const SIGKILL: i32 = 9;
}

/// Make `command`'s child a session and process-group leader.
pub(crate) fn lead_own_group(command: &mut tokio::process::Command) {
    // SAFETY: the closure runs between fork and exec and only calls setsid(2),
    // which is async-signal-safe.
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if sys::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// SIGKILL a whole process group. Errors (group already gone) are ignored.
pub(crate) fn kill_group(pgid: i32) {
    // SAFETY: kill(2) takes plain integers; a negative pid addresses the
    // group `pgid`, which the child created for itself with setsid.
    unsafe { sys::kill(-pgid, sys::SIGKILL) };
}

/// Kills the child's process group when dropped, unless disarmed (V13, S7).
pub(crate) struct GroupGuard {
    pgid: i32,
    armed: bool,
}

impl GroupGuard {
    /// Guard the group led by `child`. Unarmed when the child has no pid.
    pub(crate) fn of(child: &tokio::process::Child) -> GroupGuard {
        let pgid = child.id().and_then(|pid| i32::try_from(pid).ok()).unwrap_or(0);
        GroupGuard { pgid, armed: pgid > 0 }
    }

    /// Kill the group now and stop guarding it.
    pub(crate) fn kill_now(&mut self) {
        if self.armed {
            kill_group(self.pgid);
            self.armed = false;
        }
    }

    /// Stop guarding (the group was killed or left on purpose).
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}
