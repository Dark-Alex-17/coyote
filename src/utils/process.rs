use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// A (pid, start time) pair identifying one incarnation of an OS process.
/// A bare pid is recycled by the kernel; pairing it with the process start
/// time lets a stamp recorded on disk be checked later without mistaking a
/// newer process that inherited the pid for the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessStamp {
    pub pid: u32,
    /// Process start time in seconds since the Unix epoch, as reported by
    /// sysinfo. Zero when sysinfo cannot report the current process.
    pub start: u64,
}

impl ProcessStamp {
    pub(crate) fn current() -> Self {
        let pid = std::process::id();
        let start = lookup_start_time(pid).unwrap_or(0);
        Self { pid, start }
    }

    /// True only when a process with this pid exists AND its start time
    /// matches. An unknown pid is dead because sysinfo positively reports no
    /// such process; a start-time mismatch is dead by design, since it means
    /// the pid was reused by an unrelated process. A stamp recorded with
    /// `start` 0 (sysinfo could not list the own process at spawn) reads as
    /// dead unless sysinfo later lists that pid with a start time of 0 as
    /// well, so such a child is listed as crashed. When that pid is also
    /// absent the child is dead everywhere. When the pid still exists it
    /// reads dead here as well (and is listed crashed), but a resume refuses
    /// it, since the process under that pid may be the live child.
    pub(crate) fn is_alive(&self) -> bool {
        lookup_start_time(self.pid) == Some(self.start)
    }

    /// True when sysinfo lists a process under this pid, whatever its start
    /// time. Unlike [`Self::is_alive`] this cannot tell the original process
    /// from one that inherited the pid; it answers only whether the pid is
    /// in use.
    pub(crate) fn pid_exists(pid: u32) -> bool {
        lookup_start_time(pid).is_some()
    }
}

fn lookup_start_time(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(pid).map(|p| p.start_time())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        assert!(ProcessStamp::current().is_alive());
    }

    #[test]
    fn unknown_pid_is_dead() {
        let stamp = ProcessStamp {
            pid: u32::MAX - 1,
            start: 0,
        };
        assert!(!stamp.is_alive());
    }

    #[test]
    fn start_time_mismatch_is_dead() {
        let current = ProcessStamp::current();
        let stale = ProcessStamp {
            pid: current.pid,
            start: current.start + 1,
        };
        assert!(!stale.is_alive());
    }

    #[test]
    fn pid_exists_tracks_the_process_table() {
        assert!(ProcessStamp::pid_exists(std::process::id()));
        assert!(!ProcessStamp::pid_exists(u32::MAX - 1));
    }

    #[test]
    fn serde_round_trip() {
        let stamp = ProcessStamp {
            pid: 4242,
            start: 1_700_000_000,
        };
        let text = serde_yaml::to_string(&stamp).unwrap();
        let back: ProcessStamp = serde_yaml::from_str(&text).unwrap();
        assert_eq!(back, stamp);
    }
}
