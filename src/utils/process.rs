use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// A (pid, start time) pair identifying one incarnation of an OS process.
/// A bare pid is recycled by the kernel; pairing it with the process start
/// time lets a stamp recorded on disk be checked later without mistaking a
/// newer process that inherited the pid for the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// it and the orphan sweep keeps it, through
    /// [`Self::is_alive_or_start_unknown`], since the process under that pid
    /// may be the live child.
    pub(crate) fn is_alive(&self) -> bool {
        lookup_start_time(self.pid) == Some(self.start)
    }

    /// True when [`Self::is_alive`] holds, or when the stamp was recorded
    /// with `start` 0 and its pid is still in use. A start-0 stamp under a
    /// live pid cannot be told from the process that wrote it, so callers
    /// that would destroy or overwrite its state treat it as possibly alive.
    pub(crate) fn is_alive_or_start_unknown(&self) -> bool {
        self.is_alive() || (self.start == 0 && Self::pid_exists(self.pid))
    }

    /// True when sysinfo lists a process under this pid, whatever its start
    /// time. Unlike [`Self::is_alive`] this cannot tell the original process
    /// from one that inherited the pid; it answers only whether the pid is
    /// in use.
    pub(crate) fn pid_exists(pid: u32) -> bool {
        lookup_start_time(pid).is_some()
    }

    /// The subset of `stamps` whose process is alive by the `is_alive`
    /// rule, decided with one process-table refresh instead of one per
    /// stamp. An empty slice asks sysinfo nothing.
    pub(crate) fn alive_set(stamps: &[ProcessStamp]) -> HashSet<ProcessStamp> {
        if stamps.is_empty() {
            return HashSet::new();
        }
        #[cfg(test)]
        ALIVE_SET_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pids: HashSet<Pid> = stamps
            .iter()
            .map(|stamp| Pid::from_u32(stamp.pid))
            .collect();
        let pids: Vec<Pid> = pids.into_iter().collect();
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing(),
        );
        stamps
            .iter()
            .copied()
            .filter(|stamp| {
                sys.process(Pid::from_u32(stamp.pid))
                    .map(|p| p.start_time())
                    == Some(stamp.start)
            })
            .collect()
    }
}

/// How many process-table refreshes `alive_set` has spent in this process,
/// so a test can pin how many one caller costs.
#[cfg(test)]
pub(crate) static ALIVE_SET_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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
    use serial_test::serial;

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

    /// Serial with the sweep tests that count `alive_set` calls.
    #[test]
    #[serial]
    fn alive_set_keeps_only_the_live_self() {
        let current = ProcessStamp::current();
        let dead = ProcessStamp {
            pid: u32::MAX - 1,
            start: 0,
        };
        if current.start == 0 {
            eprintln!(
                "sysinfo cannot report this process's start time; the start discriminator goes untested"
            );
            let alive = ProcessStamp::alive_set(&[current, dead]);
            assert_eq!(alive, HashSet::from([current]));
            return;
        }
        let recycled = ProcessStamp {
            pid: current.pid,
            start: current.start + 1,
        };
        let unreported = ProcessStamp {
            pid: current.pid,
            start: 0,
        };

        let alive = ProcessStamp::alive_set(&[current, dead, recycled, unreported]);

        assert_eq!(alive, HashSet::from([current]));
    }

    #[test]
    #[serial]
    fn alive_set_of_nothing_is_empty() {
        assert!(ProcessStamp::alive_set(&[]).is_empty());
    }

    #[test]
    fn pid_exists_tracks_the_process_table() {
        assert!(ProcessStamp::pid_exists(std::process::id()));
        assert!(!ProcessStamp::pid_exists(u32::MAX - 1));
    }

    #[test]
    fn current_process_is_alive_or_start_unknown() {
        assert!(ProcessStamp::current().is_alive_or_start_unknown());
    }

    #[test]
    fn unknown_pid_with_an_unknown_start_is_not_alive_or_start_unknown() {
        let stamp = ProcessStamp {
            pid: u32::MAX - 1,
            start: 0,
        };
        assert!(!stamp.is_alive_or_start_unknown());
    }

    #[test]
    fn live_pid_with_an_unknown_start_is_alive_or_start_unknown() {
        let unreported = ProcessStamp {
            pid: std::process::id(),
            start: 0,
        };
        assert!(unreported.is_alive_or_start_unknown());
    }

    #[test]
    fn recycled_pid_with_a_known_start_is_not_alive_or_start_unknown() {
        let current = ProcessStamp::current();
        if current.start == 0 {
            eprintln!(
                "sysinfo cannot report this process's start time; the start discriminator goes untested"
            );
            return;
        }
        let recycled = ProcessStamp {
            pid: current.pid,
            start: current.start + 1,
        };
        assert!(!recycled.is_alive_or_start_unknown());
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
