use serde::{Deserialize, Serialize};
use std::collections::HashSet;
#[cfg(unix)]
use std::{fs, io, path::Path};
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
    #[cfg(test)]
    pub(crate) fn is_alive(&self) -> bool {
        lookup_start_time(self.pid) == Some(self.start)
    }

    /// True when the `is_alive` rule holds, or when the stamp was recorded
    /// with `start` 0 and its pid is still in use. A start-0 stamp under a
    /// live pid cannot be told from the process that wrote it, so callers
    /// that would destroy or overwrite its state treat it as possibly alive.
    /// Pid 0 is never ours on any host, including hosts that list it with a
    /// start time of 0 (where the `is_alive` rule holds for it): no process
    /// that wrote a stamp ran as pid 0, and it is what a record missing its
    /// pid field reads as.
    pub(crate) fn is_alive_or_start_unknown(&self) -> bool {
        self.pid != 0 && alive_or_start_unknown(*self, lookup_start_time(self.pid))
    }

    /// True when sysinfo lists a process under this pid, whatever its start
    /// time. Unlike [`Self::is_alive`] this cannot tell the original process
    /// from one that inherited the pid; it answers only whether the pid is
    /// in use.
    #[cfg(test)]
    pub(crate) fn pid_exists(pid: u32) -> bool {
        lookup_start_time(pid).is_some()
    }

    /// The subset of `stamps` whose process is alive by the `is_alive`
    /// rule, decided with one process-table refresh instead of one per
    /// stamp. An empty slice asks sysinfo nothing.
    pub(crate) fn alive_set(stamps: &[ProcessStamp]) -> HashSet<ProcessStamp> {
        let sys = Self::refreshed(stamps);
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

    /// The subset of `stamps` that hold by the `is_alive_or_start_unknown`
    /// rule, decided on the same single refresh as [`Self::alive_set`].
    pub(crate) fn alive_or_start_unknown_set(stamps: &[ProcessStamp]) -> HashSet<ProcessStamp> {
        let sys = Self::refreshed(stamps);
        stamps
            .iter()
            .copied()
            .filter(|stamp| {
                let start = sys
                    .process(Pid::from_u32(stamp.pid))
                    .map(|p| p.start_time());
                alive_or_start_unknown(*stamp, start)
            })
            .collect()
    }

    /// One process-table refresh covering the pids in `stamps`; an empty
    /// slice asks sysinfo nothing.
    fn refreshed(stamps: &[ProcessStamp]) -> System {
        let mut sys = System::new();
        if stamps.is_empty() {
            return sys;
        }
        #[cfg(test)]
        ALIVE_SET_CALLS.with(|calls| calls.set(calls.get() + 1));
        let pids: HashSet<Pid> = stamps
            .iter()
            .map(|stamp| Pid::from_u32(stamp.pid))
            .collect();
        let pids: Vec<Pid> = pids.into_iter().collect();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing(),
        );
        sys
    }
}

#[cfg(test)]
thread_local! {
    /// How many process-table refreshes `alive_set` has spent on this thread,
    /// so a test can pin how many one caller costs. Counted per thread because
    /// the mesh instance registry refreshes on every read, and tests running
    /// beside the one counting read it. The code being counted must therefore run
    /// on the calling thread, not on a spawned one.
    static ALIVE_SET_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn alive_set_calls() -> usize {
    ALIVE_SET_CALLS.with(std::cell::Cell::get)
}

/// The `is_alive_or_start_unknown` rule for `stamp`, given `start`, the start
/// time the process table reports for `stamp.pid` (`None`: no such process).
/// Callers choose how `start` is looked up, so a batched caller can decide
/// many stamps on one refresh.
pub(crate) fn alive_or_start_unknown(stamp: ProcessStamp, start: Option<u64>) -> bool {
    stamp.pid != 0 && (start == Some(stamp.start) || (stamp.start == 0 && start.is_some()))
}

/// The start time sysinfo reports for `pid`, on a refresh of that pid alone.
pub(crate) fn lookup_start_time(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(pid).map(|p| p.start_time())
}

/// Whether a process may still run under `pid`. Any failure to probe counts
/// as alive, so a caller that deletes what a dead pid leaves behind never
/// does so on doubt. `ps` rather than sysinfo answers when procfs is absent:
/// on macOS sysinfo omits processes it may not inspect (other users',
/// zombies) that `ps -p` still lists, and it cannot report a failed probe.
#[cfg(unix)]
pub(crate) fn pid_maybe_alive(pid: u32) -> bool {
    if let Some(alive) = pid_maybe_alive_via_proc(Path::new("/proc"), pid) {
        return alive;
    }
    pid_maybe_alive_via_ps("ps", pid)
}

#[cfg(unix)]
fn pid_maybe_alive_via_ps(program: &str, pid: u32) -> bool {
    match std::process::Command::new(program)
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
    {
        Ok(output) => output.status.success() && !output.stdout.is_empty(),
        Err(_) => true,
    }
}

/// `None` unless procfs is mounted at `proc_root` (its `self` entry exists):
/// in a sandbox without /proc every pid would otherwise look dead.
#[cfg(unix)]
fn pid_maybe_alive_via_proc(proc_root: &Path, pid: u32) -> Option<bool> {
    if !cfg!(target_os = "linux") || !proc_root.join("self").exists() {
        return None;
    }
    match fs::metadata(proc_root.join(pid.to_string())) {
        Ok(_) => Some(true),
        Err(err) => Some(err.kind() != io::ErrorKind::NotFound),
    }
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
    fn pid_zero_with_an_unknown_start_is_not_alive_or_start_unknown() {
        assert!(!ProcessStamp { pid: 0, start: 0 }.is_alive_or_start_unknown());
        assert!(!ProcessStamp { pid: 0, start: 1 }.is_alive_or_start_unknown());
    }

    /// Hosts differ on whether they list pid 0, so the behavioural pin above
    /// cannot fail everywhere when the guard goes; this one can.
    #[test]
    fn the_start_unknown_predicate_refuses_pid_zero_up_front() {
        let source = include_str!("process.rs").replace("\r\n", "\n");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests").unwrap()];
        let start = production
            .find("\n    pub(crate) fn is_alive_or_start_unknown(")
            .unwrap();
        let body = &production[start..];
        let body = &body[..body.find("\n    }\n").unwrap()];
        assert!(body.contains("self.pid != 0"), "{body}");
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
    fn alive_or_start_unknown_decides_on_the_looked_up_start() {
        let known = ProcessStamp { pid: 42, start: 7 };
        let unknown = ProcessStamp { pid: 42, start: 0 };
        assert!(alive_or_start_unknown(known, Some(7)));
        assert!(!alive_or_start_unknown(known, Some(8)), "a reused pid");
        assert!(!alive_or_start_unknown(known, None));
        assert!(alive_or_start_unknown(unknown, Some(8)));
        assert!(alive_or_start_unknown(unknown, Some(0)));
        assert!(!alive_or_start_unknown(unknown, None));
        for start in [None, Some(0), Some(7)] {
            for stamp_start in [0, 7] {
                let stamp = ProcessStamp {
                    pid: 0,
                    start: stamp_start,
                };
                assert!(!alive_or_start_unknown(stamp, start), "{stamp:?} {start:?}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn proc_probe_is_trusted_only_when_procfs_is_mounted() {
        let root = crate::utils::temp_file("-proc-probe-", "");
        fs::create_dir_all(root.join("123")).unwrap();
        assert_eq!(pid_maybe_alive_via_proc(&root, 123), None);
        assert_eq!(pid_maybe_alive_via_proc(&root, 456), None);
        if cfg!(target_os = "linux") {
            fs::create_dir_all(root.join("self")).unwrap();
            assert_eq!(pid_maybe_alive_via_proc(&root, 123), Some(true));
            assert_eq!(pid_maybe_alive_via_proc(&root, 456), Some(false));
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_proc_entry_that_fails_to_stat_for_another_reason_counts_as_alive() {
        let root = crate::utils::temp_file("-proc-probe-", "");
        fs::create_dir_all(root.join("self")).unwrap();
        let looping = root.join("123");
        std::os::unix::fs::symlink(&looping, &looping).unwrap();
        let err = fs::metadata(&looping).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::NotFound, "{err}");
        if cfg!(target_os = "linux") {
            assert_eq!(
                pid_maybe_alive_via_proc(&root, 123),
                Some(true),
                "a probe error other than NotFound is doubt, and doubt is alive"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_ps_that_cannot_be_spawned_counts_as_alive() {
        assert!(
            pid_maybe_alive_via_ps("coyote-no-such-ps-program", u32::MAX - 1),
            "a failed spawn is doubt, and doubt is alive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pid_maybe_alive_tracks_the_process_table() {
        assert!(pid_maybe_alive(std::process::id()));
        assert!(!pid_maybe_alive(u32::MAX - 1));
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
