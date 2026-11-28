//! Black-box coverage of `mesh.envoy_memory.prune_unused_after_days` through the real
//! `coyote` binary, written from the configuration contract (README, config template,
//! Mesh-Configuration page) rather than from the implementation:
//!
//! - `prune_unused_after_days: N` has a session that joins the mesh sweep every instance
//!   store under `<cache dir>/mesh/envoy-sessions/` and delete a whole store whose
//!   last-used stamp is more than N days old, even with `envoy_memory.enabled: false`;
//!   a store with no stamp is stamped "now" and kept; a store used within N days is
//!   kept; the running session's own store is never deleted, however stale.
//! - `null` (the default, also when the key is absent) leaves every other store alone.
//! - `0` is refused at config load with the documented line.
//!
//! The REPL is driven through a pty against a private-relay interface pointing at a
//! loopback listener this test owns (accepted and held silent), like
//! `mesh_autostart_repl.rs`. Every wait is bounded.
//!
//! **Unix only**, for the same reasons as `mesh_autostart_repl.rs`.

#![cfg(unix)]

mod support;

use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use expectrl::{Eof, Expect};

use support::bin::{DryRunConfig, coyote_command};
use support::pty::{Probe, Target, spawn_repl, wait_for, wait_for_text};

const SESSION_PROMPT: &str = r"dry-model\)(\x1b\[[0-9;]*m)*probe(\x1b\[[0-9;]*m)*\)";
const JOINED_PATTERN: &str = r"Joined the mesh as you / probe \([0-9a-f]{8}\)";
const DAY: u64 = 24 * 60 * 60;

fn silent_relay() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut sink = [0u8; 1024];
                while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
            });
        }
    });
    port
}

fn rfc3339_days_ago(days: u64) -> String {
    let at = SystemTime::now() - Duration::from_secs(days * DAY);
    chrono::DateTime::<chrono::Utc>::from(at)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn uuid_simple() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

impl Probe {
    /// `envoy_memory` is the yaml body under `mesh.envoy_memory:` (two-space indented
    /// lines), or empty for no block at all.
    fn new(label: &str, relay_port: u16, envoy_memory: &str) -> Self {
        let probe = Self::fresh("coyote-envoy-prune", label);
        let envoy_block = if envoy_memory.is_empty() {
            String::new()
        } else {
            format!("  envoy_memory:\n{envoy_memory}")
        };
        DryRunConfig::default()
            .vault_password_file(&probe.vault_pass())
            .setting("function_calling_support: true")
            .append(&format!(
                "save_session: false\n\
                 mesh:\n\
                 \x20 enabled: true\n\
                 \x20 announce: false\n\
                 \x20 interfaces:\n\
                 \x20   - {{type: private, host: 127.0.0.1, port: {relay_port}}}\n\
                 {envoy_block}"
            ))
            .write(&probe.config_dir);
        probe
    }

    fn cache_dir(&self) -> PathBuf {
        self.dirs().cache
    }

    fn stores_dir(&self) -> PathBuf {
        self.cache_dir().join("mesh").join("envoy-sessions")
    }

    /// A saved session `probe` bound to `instance_id`, so the store of the session that
    /// runs is known before it starts.
    fn with_session_instance(self, instance_id: &str) -> Self {
        let dir = self.config_dir.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("probe.yaml"),
            format!("model: dryrun:dry-model\nmesh_instance_id: {instance_id}\nmessages: []\n"),
        )
        .unwrap();
        self
    }

    /// An instance store holding no threads, its index stamped `stamp` (or unstamped).
    fn seed_store(&self, instance_id: &str, stamp: Option<&str>) -> PathBuf {
        let dir = self.stores_dir().join(instance_id);
        fs::create_dir_all(&dir).unwrap();
        for d in [
            self.cache_dir().join("mesh"),
            self.stores_dir(),
            dir.clone(),
        ] {
            fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let index = match stamp {
            Some(stamp) => {
                format!(r#"{{"version":1,"entries":[],"store_last_used":"{stamp}"}}"#)
            }
            None => r#"{"version":1,"entries":[]}"#.to_string(),
        };
        let path = dir.join("index.json");
        fs::write(&path, index).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        dir
    }

    fn command(&self) -> Command {
        let mut cmd = coyote_command(&self.dirs());
        cmd.env("XDG_CACHE_HOME", &self.temp_root)
            .env("TERM", "xterm-256color")
            .env_remove("IS_SANDBOX")
            .env_remove("NO_COLOR")
            .current_dir(&self.home_dir);
        cmd
    }

    fn registered_instances(&self) -> String {
        fs::read_to_string(self.cache_dir().join("mesh").join("instances.json")).unwrap_or_default()
    }
}

fn exit_clean(session: &mut Target) {
    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    let _ = session.get_process().wait();
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn index_of(store: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(store.join("index.json")).unwrap()).unwrap()
}

/// Joins with the four kinds of store on disk and `envoy_memory.enabled: false`: only the
/// other instance's store unused for longer than N days goes.
#[test]
fn a_join_with_prune_unused_after_days_deletes_only_other_stores_unused_past_n_days_even_with_memory_off()
 {
    let own = uuid_simple();
    let probe = Probe::new(
        "set",
        silent_relay(),
        "    enabled: false\n    prune_unused_after_days: 7\n",
    )
    .with_session_instance(&own);
    let stale = probe.seed_store(&uuid_simple(), Some(&rfc3339_days_ago(30)));
    let used_yesterday = probe.seed_store(&uuid_simple(), Some(&rfc3339_days_ago(1)));
    let unstamped = probe.seed_store(&uuid_simple(), None);
    let own_stale = probe.seed_store(&own, Some(&rfc3339_days_ago(30)));

    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);
    let startup = wait_for_text(&mut session, JOINED_PATTERN);
    wait_for(&mut session, SESSION_PROMPT);
    assert!(
        probe.registered_instances().contains(&own),
        "the session did not run as the seeded instance {own}; startup: {startup}\ninstances: {}",
        probe.registered_instances()
    );

    wait_until("the stale store to be deleted", || !stale.exists());
    // The unstamped store is stamped at that same sweep.
    wait_until("the unstamped store to be stamped", || {
        index_of(&unstamped)["store_last_used"].is_string()
    });
    exit_clean(&mut session);

    assert!(
        used_yesterday.join("index.json").exists(),
        "a store used 1 day ago was deleted under prune_unused_after_days: 7"
    );
    assert!(
        own_stale.exists(),
        "the running session's own store was deleted by the sweep"
    );
    let stamp = index_of(&unstamped)["store_last_used"]
        .as_str()
        .unwrap()
        .to_string();
    let stamped_at = chrono::DateTime::parse_from_rfc3339(&stamp)
        .unwrap_or_else(|err| panic!("{stamp:?} is not RFC 3339: {err}"));
    let age = chrono::Utc::now().signed_duration_since(stamped_at);
    assert!(
        age.num_seconds().abs() < 600,
        "the unstamped store was stamped {stamp}, not now"
    );
    assert_eq!(
        index_of(&unstamped)["version"],
        1,
        "stamping changed the index version"
    );
}

/// The default (key absent) and an explicit `null` leave every other store where it is,
/// stale or unstamped, untouched.
#[test]
fn a_join_without_prune_unused_after_days_leaves_other_stores_alone() {
    for (label, block) in [
        ("absent", ""),
        (
            "null",
            "    enabled: false\n    prune_unused_after_days: null\n",
        ),
    ] {
        let probe = Probe::new(label, silent_relay(), block);
        let stale_stamp = rfc3339_days_ago(400);
        let stale = probe.seed_store(&uuid_simple(), Some(&stale_stamp));
        let unstamped = probe.seed_store(&uuid_simple(), None);
        let stale_before = fs::read_to_string(stale.join("index.json")).unwrap();
        let unstamped_before = fs::read_to_string(unstamped.join("index.json")).unwrap();

        let mut command = probe.command();
        command.args(["--session", "probe"]);
        let mut session = spawn_repl(command);
        wait_for(&mut session, JOINED_PATTERN);
        wait_for(&mut session, SESSION_PROMPT);
        // A set key sweeps right at join; give an unwanted sweep the same chance.
        thread::sleep(Duration::from_secs(3));
        exit_clean(&mut session);

        assert_eq!(
            fs::read_to_string(stale.join("index.json")).ok().as_deref(),
            Some(stale_before.as_str()),
            "{label}: a stale store was touched or deleted"
        );
        assert_eq!(
            fs::read_to_string(unstamped.join("index.json"))
                .ok()
                .as_deref(),
            Some(unstamped_before.as_str()),
            "{label}: an unstamped store was touched or deleted"
        );
    }
}

/// `prune_unused_after_days: 0` is refused at load with the line the
/// Mesh-Configuration page quotes, and the process exits non-zero.
#[test]
fn prune_unused_after_days_zero_is_refused_at_load_with_the_documented_line() {
    let probe = Probe::new("zero", silent_relay(), "    prune_unused_after_days: 0\n");
    let output = probe
        .command()
        .args(["--info"])
        .output()
        .expect("run coyote --info");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "0 was accepted:\n{text}");
    assert!(
        text.contains(
            "mesh.envoy_memory.prune_unused_after_days is 0, which is out of range; use 1 or more, or null to keep only each session's own pruning"
        ),
        "{text}"
    );
}

/// `.mesh files list <glob>` is refused as a pattern, not a session, pointing at the
/// listing of your own shares and at `--prefix`, and lists nothing.
#[test]
fn files_list_with_a_glob_is_refused_as_a_pattern_not_a_session() {
    let probe = Probe::new("glob", silent_relay(), "");
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);
    wait_for(&mut session, JOINED_PATTERN);
    wait_for(&mut session, SESSION_PROMPT);
    for pattern in ["docs/**", "*.rs"] {
        session
            .send(format!(".mesh files list {pattern}\r"))
            .expect("type the command");
        let text = wait_for_text(&mut session, r"Nothing was listed\.");
        let row = text
            .lines()
            .rev()
            .find(|line| line.contains("not a session"))
            .unwrap_or_else(|| panic!("no pattern refusal for {pattern}: {text}"));
        assert!(
            row.contains(&format!("'{pattern}'")) && row.contains("--prefix"),
            "{row}"
        );
        assert!(
            !text.contains("Asking "),
            "a pattern was sent to a session: {text}"
        );
        wait_for(&mut session, SESSION_PROMPT);
    }
    exit_clean(&mut session);
}
