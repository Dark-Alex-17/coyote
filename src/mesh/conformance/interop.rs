//! Interop against the pinned Python Reticulum/LXMF reference, spawned as a subprocess.
//!
//! `Reference` runs `scripts/mesh-interop/reference_peer.py` and talks to it over JSON lines;
//! the reference is the TCP server and transport node, this crate's runtime dials it as a
//! `TcpClient`, the production relay topology. Every test lists the `docs/mesh/PROTOCOL.md`
//! ids it exercises in the `*_IDS` constants, which `listed` feeds to the coverage report.

use super::{Kind, Listed};
use crate::config::Session;
use crate::mesh::hex_lower;
use crate::mesh::message::{OutboundPeer, PeerKind, PeerVia};
use crate::mesh::node::{MeshRuntime, MeshSlot, NodeOptions};
use crate::mesh::test_support::{
    Compatibility, OriginName, TempDir, TrustList, disable_ingress_control, mesh_paths,
    private_config, wait_until,
};
use crate::mesh::trust::TrustOptions;
use crate::supervisor::mailbox::EnvelopePayload;

use rns_transport::destination::DestinationName;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, str};
use tokio::time::sleep;

const RETICULUM_PIN: &str = "ea98db4f53dcf0defc0e71a16e60d28b1229c4e6";
const LXMF_PIN: &str = "727830cefda83d9c6e3982b48675425f3f988f9c";
const SETUP_SH: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-interop/setup.sh"
));
const HARNESS_README: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-interop/README.md"
));
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-interop/reference_peer.py"
);
const SETUP_SH_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/mesh-interop/setup.sh");

/// A link open or a path wait on the reference's side.
const LINK_TIMEOUT: Duration = Duration::from_secs(15);
/// One request to us, answered or timed out, on the reference's side.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A command that has to wait for a request to us to time out before it can fall back.
const STORE_AND_FORWARD_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(50);

const NO_ACCESS: u64 = 0xf1;
const INVALID_DATA: u64 = 0xf4;
/// LXMF's custom-type and custom-data field keys (`lxmf_core::constants`), as the reference
/// serialises them: integer map keys become decimal strings in JSON.
const FIELD_CUSTOM_TYPE: &str = "251";
const FIELD_CUSTOM_DATA: &str = "252";

/// What `reference_peer.py` prints once it listens.
struct Ready {
    relay_port: u16,
    identity_hash: String,
    destination_hash: String,
    name_hash: String,
    instance_id: String,
}

struct Reference {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    events: VecDeque<Value>,
    next_id: u64,
    ready: Ready,
    /// With `COYOTE_MESH_INTEROP_DEBUG` set, where this crate's captured `mesh` debug log
    /// stood when the reference was spawned; everything after it is printed to stderr
    /// when the reference is torn down, so a silent drop on either side can be placed.
    debug_mark: Option<usize>,
}

/// Where the clones and the venv live: `COYOTE_MESH_INTEROP_DIR`, else `setup.sh`'s default.
fn interop_dir() -> PathBuf {
    env::var_os("COYOTE_MESH_INTEROP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").expect("HOME is set"))
                .join(".cache/coyote/mesh-interop")
        })
}

/// The interpreter to spawn: `COYOTE_MESH_INTEROP_PYTHON`, else the venv `setup.sh` built
/// under `dir`, else whatever `python3` is on the path. A bare name is resolved against this
/// process's `PATH` here: the netns relay is launched under `sudo -n`, whose `secure_path`
/// would resolve it differently.
fn interop_python(dir: &Path) -> PathBuf {
    let python = env::var_os("COYOTE_MESH_INTEROP_PYTHON")
        .map(PathBuf::from)
        .or_else(|| {
            let venv = dir.join("venv").join("bin").join("python");
            venv.is_file().then_some(venv)
        })
        .unwrap_or_else(|| PathBuf::from("python3"));
    if python.components().count() != 1 {
        return python;
    }
    env::var_os("PATH")
        .and_then(|path| {
            env::split_paths(&path)
                .map(|dir| dir.join(&python))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or(python)
}

/// `COYOTE_MESH_INTEROP` read by value: unset, empty, `0` and `false`/`no`/`off` (in any
/// ASCII case) leave the suites off; anything else switches them on.
fn interop_switch_is_on(value: Option<&OsStr>) -> bool {
    let Some(value) = value else { return false };
    let value = value.to_string_lossy();
    let value = value.trim();
    !(value.is_empty()
        || value == "0"
        || ["false", "no", "off"]
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off)))
}

pub(super) fn interop_enabled() -> bool {
    interop_switch_is_on(env::var_os("COYOTE_MESH_INTEROP").as_deref())
}

/// The `skipping:` line for a suite that is off: with the switch set to an off value, the
/// value as observed, so a typo in it is not mistaken for an unset variable.
pub(super) fn off_switch_skip_line(instructions: &str) -> String {
    match env::var_os("COYOTE_MESH_INTEROP") {
        Some(value) => format!(
            "skipping: COYOTE_MESH_INTEROP={value:?} is off; set it to 1 and run {instructions}"
        ),
        None => format!("skipping: set COYOTE_MESH_INTEROP=1 and run {instructions}"),
    }
}

/// The interop directory and the interpreter, once both clones are present and at their
/// pins. Every miss is a panic naming `scripts/mesh-interop/setup.sh`, so CI cannot pass
/// by skipping.
pub(super) fn require_reference() -> (PathBuf, PathBuf) {
    let dir = interop_dir();
    for (name, pin) in [("reticulum", RETICULUM_PIN), ("lxmf", LXMF_PIN)] {
        let clone = dir.join(name);
        assert!(
            clone.is_dir(),
            "{} is missing; run scripts/mesh-interop/setup.sh",
            clone.display()
        );
        // A root test process verifying a user-owned clone: without this git refuses the
        // repository as dubiously owned and prints nothing.
        let clone_path = clone.display().to_string();
        let out = Command::new("git")
            .args(["-c", &format!("safe.directory={clone_path}")])
            .args(["-C", &clone_path, "rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        let head = str::from_utf8(&out.stdout).unwrap().trim().to_string();
        assert!(
            out.status.success() && !head.is_empty(),
            "git rev-parse HEAD in {clone_path} failed ({}): {}; run scripts/mesh-interop/setup.sh",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        assert_eq!(
            head,
            pin,
            "{} is at {head} but the suite pins {pin}; run scripts/mesh-interop/setup.sh",
            clone.display()
        );
    }
    let python = interop_python(&dir);
    (dir, python)
}

impl Reference {
    /// `None` when the suite is not switched on; see `require_reference` for what happens
    /// once it is.
    async fn spawn() -> Option<Self> {
        if !interop_enabled() {
            eprintln!("{}", off_switch_skip_line("scripts/mesh-interop/setup.sh"));
            return None;
        }
        crate::testing::install_log_collector();
        let debug_mark = env::var_os("COYOTE_MESH_INTEROP_DEBUG")
            .map(|_| crate::testing::debug_snapshot().len());
        let (dir, python) = require_reference();
        let pythonpath = format!(
            "{}:{}",
            dir.join("reticulum").display(),
            dir.join("lxmf").display()
        );
        let mut child = Command::new(&python)
            .arg(SCRIPT)
            .env("PYTHONPATH", pythonpath)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to spawn {} {SCRIPT}: {err}; run scripts/mesh-interop/setup.sh",
                    python.display()
                )
            });
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut reference = Self {
            child,
            stdin,
            lines,
            events: VecDeque::new(),
            next_id: 0,
            ready: Ready {
                relay_port: 0,
                identity_hash: String::new(),
                destination_hash: String::new(),
                name_hash: String::new(),
                instance_id: String::new(),
            },
            debug_mark,
        };
        let line = reference.next_line(Instant::now() + LINK_TIMEOUT).await;
        let ready: Value = line
            .strip_prefix("READY ")
            .map(|json| serde_json::from_str(json).unwrap())
            .unwrap_or_else(|| panic!("the reference did not start: {line}"));
        let text = |key: &str| ready[key].as_str().unwrap().to_string();
        reference.ready = Ready {
            relay_port: u16::try_from(ready["relay_port"].as_u64().unwrap()).unwrap(),
            identity_hash: text("identity_hash"),
            destination_hash: text("destination_hash"),
            name_hash: text("name_hash"),
            instance_id: text("instance_id"),
        };
        Some(reference)
    }

    async fn next_line(&mut self, deadline: Instant) -> String {
        loop {
            match self.lines.try_recv() {
                Ok(line) => return line,
                Err(TryRecvError::Disconnected) => {
                    panic!(
                        "the reference exited: {:?}; run scripts/mesh-interop/setup.sh",
                        self.child.try_wait()
                    )
                }
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting on the reference"
                    );
                    sleep(POLL).await;
                }
            }
        }
    }

    /// Runs one command and returns its reply; a reply that is not `ok` is a failure of
    /// the test. Events that arrive meanwhile are kept for `next_event`.
    async fn send(&mut self, cmd: &str, mut args: Value, timeout: Duration) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        args["id"] = json!(id);
        args["cmd"] = json!(cmd);
        writeln!(self.stdin, "{args}").unwrap();
        self.stdin.flush().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            let line = self.next_line(deadline).await;
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("the reference wrote {line:?}: {err}"));
            if value["id"].as_u64() == Some(id) {
                assert_eq!(value["ok"], json!(true), "{cmd} failed: {value}");
                return value;
            }
            if value.get("event").is_some() {
                self.events.push_back(value);
            }
        }
    }

    /// The next `kind` event, taken from the ones already heard first.
    async fn next_event(&mut self, kind: &str, timeout: Duration) -> Value {
        if let Some(at) = self.events.iter().position(|event| event["event"] == kind) {
            return self.events.remove(at).unwrap();
        }
        let deadline = Instant::now() + timeout;
        loop {
            let line = self.next_line(deadline).await;
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("the reference wrote {line:?}: {err}"));
            if value["event"] == kind {
                return value;
            }
            if value.get("event").is_some() {
                self.events.push_back(value);
            }
        }
    }

    async fn announce(&mut self, display_name: Option<&str>) {
        self.send(
            "announce",
            json!({ "display_name": display_name }),
            REQUEST_TIMEOUT,
        )
        .await;
    }

    /// Asks `node` for `path` over a fresh identified link and returns the reply as the
    /// reference unpacked it: an integer for a refusal code, a map otherwise.
    async fn request(&mut self, node: &Node, path: &str, envelope: Value) -> Value {
        let reply = self
            .send(
                "request",
                json!({
                    "destination_hash": node.runtime.current_destination_hash(),
                    "instance_id": node.instance_id,
                    "path": path,
                    "envelope": envelope,
                    "timeout_secs": REQUEST_TIMEOUT.as_secs(),
                }),
                REQUEST_TIMEOUT + LINK_TIMEOUT,
            )
            .await;
        assert_eq!(reply["status"], json!("ready"), "{path}: {reply}");
        reply["response"].clone()
    }

    /// The Envelope a well-behaved requester sends: this reference's own name hash.
    fn envelope(&self, body: Value) -> Value {
        json!({ "v": 1, "name_hash": self.ready.name_hash, "body": body })
    }

    fn shut_down(&mut self) {
        let _ = writeln!(self.stdin, "{}", json!({ "id": 0, "cmd": "quit" }));
        let _ = self.stdin.flush();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(POLL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn print_debug_log(&self) {
        let Some(mark) = self.debug_mark else { return };
        for line in crate::testing::debug_snapshot().iter().skip(mark) {
            eprintln!("[coyote] DEBUG {line}");
        }
        for line in crate::testing::warn_snapshot() {
            eprintln!("[coyote] WARN {line}");
        }
    }
}

impl Drop for Reference {
    fn drop(&mut self) {
        self.shut_down();
        self.print_debug_log();
    }
}

/// This crate's node joined to the reference's relay port, with a slot installed so
/// `/status` and `/message` are served.
struct Node {
    runtime: Arc<MeshRuntime>,
    slot: Arc<MeshSlot>,
    instance_id: String,
    _tmp: TempDir,
}

impl Node {
    /// `session` already carries the instance id, so a caller may have told the reference
    /// which aspect to watch before the start announce goes out.
    async fn start(tag: &str, port: u16, mut session: Session, trust: &TrustList) -> Self {
        let tmp = TempDir::new(tag);
        let paths = mesh_paths(&tmp);
        trust.write(&paths.config_dir);
        let runtime = MeshRuntime::start(
            &private_config(port),
            true,
            &mut session,
            paths,
            NodeOptions::default(),
        )
        .await
        .unwrap();
        disable_ingress_control(&runtime).await;
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        Self {
            instance_id: runtime.current_instance_id(),
            runtime,
            slot,
            _tmp: tmp,
        }
    }

    fn name_hash(&self) -> String {
        name_hash_of(&self.instance_id)
    }

    /// Waits for the reference's announce to be filed, then trusts its destination.
    async fn trust_reference(&self, reference: &Reference) {
        let peers = self.runtime.peers();
        let destination = reference.ready.destination_hash.clone();
        wait_until("the node to file the reference", || {
            peers.get(&destination).is_some()
        })
        .await;
        self.runtime
            .trust()
            .trust_destination(
                self.slot.as_ref(),
                &destination,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
    }

    async fn stop(self) {
        assert!(self.slot.stop().await.unwrap());
    }
}

fn session_with_instance_id() -> (Session, String) {
    let mut session = Session::default();
    let instance_id = session.ensure_mesh_instance_id().to_string();
    (session, instance_id)
}

/// This crate's name hash for `coyote.mesh.<instance_id>`, section 4's `trunc_10(H(name))`.
fn name_hash_of(instance_id: &str) -> String {
    let name = DestinationName::new("coyote", &format!("mesh.{instance_id}"));
    hex_lower(&OriginName::of(&name).0)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn is_hex_of_len(text: &str, len: usize) -> bool {
    text.len() == len && text.chars().all(|c| c.is_ascii_hexdigit())
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("not a map: {value}"))
        .keys()
        .map(String::as_str)
        .collect()
}

/// The reference decodes our announce and we file its announce.
const ANNOUNCE_IDS: &[&str] = &[
    "MESH-DEST-003",
    "MESH-DEST-004",
    "MESH-DEST-005",
    "MESH-ANN-001",
    "MESH-ANN-002",
    "MESH-ANN-004",
    "MESH-ANN-010",
    "MESH-VER-003",
];

/// The reference's well-formed requests are served: the card and the received reply.
const REPLY_VALID_IDS: &[&str] = &[
    "MESH-STATUS-001",
    "MESH-STATUS-004",
    "MESH-STATUS-012",
    "MESH-MSG-001",
    "MESH-MSG-015",
    "MESH-MSG-016",
];

/// The reference's untrusted, nameless, wrong-version, wrong-kind and unknown-path requests
/// hear `NoAccess`, `InvalidData`, the version refusal or the `unknown_path` map.
const REPLY_INVALID_IDS: &[&str] = &[
    "MESH-ENV-014",
    "MESH-ENV-015",
    "MESH-ENV-029",
    "MESH-ENV-030",
    "MESH-ENV-032",
    "MESH-ENV-034",
    "MESH-ENV-038",
    "MESH-VER-005",
    "MESH-VER-006",
    "MESH-VER-007",
    "MESH-VER-008",
    "MESH-VER-009",
    "MESH-VER-010",
    "MESH-VER-011",
    "MESH-STATUS-003",
    "MESH-MSG-002",
    "MESH-MSG-018",
];

/// Our `/message` and `/status` requests are decoded by the reference as sent.
const REQUEST_IDS: &[&str] = &[
    "MESH-ENV-012",
    "MESH-ENV-018",
    "MESH-ENV-022",
    "MESH-MSG-011",
    "MESH-MSG-014",
    "MESH-MSG-022",
    "MESH-STATUS-002",
    "MESH-CANON-008",
];

/// A propagation node announcing a stamp cost above the default and under the cap is filed,
/// selected and mined for at that cost.
const PROPAGATION_COST_IDS: &[&str] = &[
    "MESH-ANN-024",
    "MESH-ANN-027",
    "MESH-PROP-010",
    "MESH-PROP-012",
];

/// A silent peer's message falls back to the propagation node and is stored there.
const PROPAGATION_IDS: &[&str] = &["MESH-PROP-015", "MESH-MSG-024"];

pub(super) fn listed() -> Vec<Listed> {
    [
        (ANNOUNCE_IDS, Kind::Valid),
        (REPLY_VALID_IDS, Kind::Valid),
        (REPLY_INVALID_IDS, Kind::Invalid),
        (REQUEST_IDS, Kind::Valid),
        (PROPAGATION_COST_IDS, Kind::Boundary),
        (PROPAGATION_IDS, Kind::Valid),
    ]
    .into_iter()
    .flat_map(|(ids, kind)| {
        ids.iter().map(move |id| Listed {
            id,
            kind,
            family: "Interop",
        })
    })
    .collect()
}

#[test]
fn the_pins_agree_with_setup_sh_and_the_harness_readme() {
    for (name, pin) in [("RETICULUM_PIN", RETICULUM_PIN), ("LXMF_PIN", LXMF_PIN)] {
        let assignment = format!("{name}=\"{pin}\"");
        assert!(
            SETUP_SH.lines().any(|line| line == assignment),
            "scripts/mesh-interop/setup.sh does not pin {assignment}"
        );
        assert!(
            HARNESS_README.contains(pin),
            "scripts/mesh-interop/README.md does not name {pin}"
        );
    }
    let installs: Vec<&str> = SETUP_SH
        .lines()
        .filter(|line| line.contains("pip install"))
        .filter(|line| {
            line.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == "rns" || word == "lxmf")
        })
        .collect();
    assert_eq!(
        installs,
        Vec::<&str>::new(),
        "setup.sh installs the reference from a package index instead of the pinned clones"
    );
}

#[test]
fn the_interop_switch_reads_its_value() {
    assert!(!interop_switch_is_on(None));
    for off in ["", " ", "0", "false", "FALSE", "no", "off", "Off"] {
        assert!(!interop_switch_is_on(Some(OsStr::new(off))), "{off:?}");
    }
    for on in ["1", "yes", "true", "on", "anything"] {
        assert!(interop_switch_is_on(Some(OsStr::new(on))), "{on:?}");
    }
    for off in ["0", "false", "no", "off"] {
        assert!(
            HARNESS_README.contains(&format!("`{off}`")),
            "scripts/mesh-interop/README.md does not list `{off}` as an off value"
        );
    }
}

/// `git <args>` in `cwd`, which must succeed.
fn git(cwd: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `setup.sh` against `dir`: (exit status, stdout, stderr).
fn run_setup_sh(dir: &std::path::Path) -> (bool, String, String) {
    let out = Command::new("bash")
        .arg(SETUP_SH_PATH)
        .env("COYOTE_MESH_INTEROP_DIR", dir)
        .output()
        .expect("bash runs setup.sh");
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Acceptance (d)/(e), probed on the script itself rather than its text: `setup.sh` honours
/// `COYOTE_MESH_INTEROP_DIR`, re-pins a clone that drifted off its pin, verifies both `HEAD`s,
/// prints the environment exports and writes the same to `env.sh`; a second run changes
/// nothing and fetches nothing; a clone that cannot reach its pin is a failure that leaves
/// no `env.sh` behind. Seeded from the local clones, so it needs no network, and it reuses
/// the local venv so it installs nothing.
#[test]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
fn setup_sh_repins_a_drifted_clone_reruns_idempotently_and_refuses_an_unreachable_pin() {
    if !interop_enabled() {
        eprintln!("{}", off_switch_skip_line("scripts/mesh-interop/setup.sh"));
        return;
    }
    let source = interop_dir();
    for name in ["reticulum", "lxmf"] {
        assert!(
            source.join(name).is_dir(),
            "{} is missing; run scripts/mesh-interop/setup.sh",
            source.join(name).display()
        );
    }
    let tmp = TempDir::new("setup-sh");

    // A directory whose reticulum clone drifted one commit off the pin and whose lxmf clone
    // is already pinned.
    let drifted = tmp.path.join("drifted");
    std::fs::create_dir_all(&drifted).unwrap();
    for name in ["reticulum", "lxmf"] {
        git(
            &tmp.path,
            &[
                "clone",
                "--quiet",
                &source.join(name).display().to_string(),
                &drifted.join(name).display().to_string(),
            ],
        );
    }
    git(
        &drifted.join("reticulum"),
        &["checkout", "--quiet", "--detach", "HEAD~1"],
    );
    assert_ne!(
        git(&drifted.join("reticulum"), &["rev-parse", "HEAD"]),
        RETICULUM_PIN
    );
    let venv = source.join("venv");
    if venv.join("bin").join("python").is_file() {
        std::os::unix::fs::symlink(&venv, drifted.join("venv")).unwrap();
    }

    let (ok, stdout, stderr) = run_setup_sh(&drifted);
    assert!(ok, "first run failed:\n{stderr}");
    for (name, pin) in [("reticulum", RETICULUM_PIN), ("lxmf", LXMF_PIN)] {
        assert_eq!(
            git(&drifted.join(name), &["rev-parse", "HEAD"]),
            pin,
            "{name} was not re-pinned"
        );
    }
    let exports: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        exports.len(),
        3,
        "stdout is exactly the three exports:\n{stdout}"
    );
    assert!(
        exports[0].starts_with("export COYOTE_MESH_INTEROP_DIR="),
        "{stdout}"
    );
    assert!(exports[0].ends_with("/drifted"), "{stdout}");
    assert!(
        exports[1].starts_with("export COYOTE_MESH_INTEROP_PYTHON="),
        "{stdout}"
    );
    assert!(exports[2].starts_with("export PYTHONPATH="), "{stdout}");
    assert!(exports[2].contains("/drifted/reticulum:"), "{stdout}");
    assert!(exports[2].ends_with("/drifted/lxmf"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(drifted.join("env.sh")).unwrap(),
        stdout,
        "env.sh is what was printed"
    );
    assert!(
        stderr.contains(&format!("at {RETICULUM_PIN}"))
            && stderr.contains(&format!("at {LXMF_PIN}")),
        "both pins are reported:\n{stderr}"
    );
    assert!(
        !stderr.contains("pip install") || !stderr.contains(" rns"),
        "the reference is never pip-installed:\n{stderr}"
    );

    // Idempotent: the second run neither clones, fetches nor rebuilds, and prints the same.
    let (ok, stdout_again, stderr_again) = run_setup_sh(&drifted);
    assert!(ok, "second run failed:\n{stderr_again}");
    assert_eq!(stdout_again, stdout, "the exports changed between runs");
    for verb in ["cloning", "fetching", "creating venv"] {
        assert!(
            !stderr_again.contains(verb),
            "the second run should not be {verb}:\n{stderr_again}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(drifted.join("env.sh")).unwrap(),
        stdout
    );

    // A reticulum clone with an unrelated history and a dead origin can never reach the pin.
    let unreachable = tmp.path.join("unreachable");
    std::fs::create_dir_all(&unreachable).unwrap();
    let bogus = unreachable.join("reticulum");
    std::fs::create_dir_all(&bogus).unwrap();
    git(&bogus, &["init", "--quiet"]);
    git(
        &bogus,
        &[
            "-c",
            "user.email=probe@example.invalid",
            "-c",
            "user.name=probe",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "unrelated",
        ],
    );
    let dead_origin = tmp.path.join("no-such-remote");
    git(
        &bogus,
        &[
            "remote",
            "add",
            "origin",
            &dead_origin.display().to_string(),
        ],
    );
    git(
        &tmp.path,
        &[
            "clone",
            "--quiet",
            &source.join("lxmf").display().to_string(),
            &unreachable.join("lxmf").display().to_string(),
        ],
    );
    let (ok, stdout, _stderr) = run_setup_sh(&unreachable);
    assert!(!ok, "setup.sh must fail when a clone cannot reach its pin");
    assert_eq!(stdout, "", "a failed run prints no exports");
    assert!(
        !unreachable.join("env.sh").exists(),
        "a failed run leaves no env.sh behind"
    );
}

/// Ids: `ANNOUNCE_IDS`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn the_reference_announce_is_filed_and_it_derives_our_destination_from_our_announce() {
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    assert_eq!(
        reference.ready.name_hash,
        name_hash_of(&reference.ready.instance_id)
    );
    let (session, instance_id) = session_with_instance_id();
    reference
        .send(
            "watch",
            json!({ "instance_id": instance_id }),
            REQUEST_TIMEOUT,
        )
        .await;
    let node = Node::start(
        "interop-announce",
        reference.ready.relay_port,
        session,
        &TrustList::default(),
    )
    .await;

    let heard = reference.next_event("announce", LINK_TIMEOUT).await;
    let ours = node.runtime.current_destination_hash();
    assert_eq!(
        heard["destination_hash"],
        json!(ours),
        "{ANNOUNCE_IDS:?}: {heard}"
    );
    assert_eq!(heard["derived_destination_hash"], json!(ours), "{heard}");
    assert_eq!(heard["identity_hash"], json!(node.runtime.fingerprint()));
    assert_eq!(heard["decoded"]["magic_ok"], json!(true), "{heard}");
    assert_eq!(heard["decoded"]["version"], json!(1), "{heard}");
    assert_eq!(heard["decoded"]["display_name"], Value::Null, "{heard}");

    reference.announce(Some("Reference")).await;
    let peers = node.runtime.peers();
    let destination = reference.ready.destination_hash.clone();
    wait_until("the node to file the reference", || {
        peers.get(&destination).is_some()
    })
    .await;
    let filed = peers.get(&destination).unwrap();
    assert_eq!(filed.identity_hash, reference.ready.identity_hash);
    assert_eq!(filed.name_hash, reference.ready.name_hash);
    assert_eq!(filed.display_name.as_deref(), Some("Reference"));
    assert_eq!(filed.protocol_version, 1);
    assert_eq!(filed.compatibility, Compatibility::Compatible);

    node.stop().await;
    drop(reference);
}

/// Ids: `REPLY_VALID_IDS` and `REPLY_INVALID_IDS`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn reference_requests_hear_the_specified_replies() {
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    // The identity is known but its instance is not trusted: the default-closed verdict,
    // which knocks and refuses. An unknown identity would hear silence instead.
    let known_identity = TrustList::default().identity(&reference.ready.identity_hash, false);
    let (session, _) = session_with_instance_id();
    let node = Node::start(
        "interop-replies",
        reference.ready.relay_port,
        session,
        &known_identity,
    )
    .await;
    reference
        .send(
            "wait_path",
            json!({
                "destination_hash": node.runtime.current_destination_hash(),
                "timeout_secs": LINK_TIMEOUT.as_secs(),
            }),
            LINK_TIMEOUT + POLL,
        )
        .await;

    let refused = reference
        .request(&node, "/status", reference.envelope(Value::Null))
        .await;
    assert_eq!(refused, json!(NO_ACCESS), "{REPLY_INVALID_IDS:?}");
    let gate = node.runtime.knock_gate();
    let (identity, destination) = (
        reference.ready.identity_hash.clone(),
        reference.ready.destination_hash.clone(),
    );
    wait_until("the knock to be filed", || {
        gate.cache()
            .list(SystemTime::now())
            .unwrap()
            .iter()
            .any(|knock| knock.identity_hash == identity && knock.destination_hash == destination)
    })
    .await;

    reference.announce(Some("Reference")).await;
    node.trust_reference(&reference).await;

    let card = reference
        .request(&node, "/status", reference.envelope(Value::Null))
        .await;
    assert_eq!(card["v"], json!(1), "{REPLY_VALID_IDS:?}: {card}");
    let code = card["state"]["code"]
        .as_u64()
        .unwrap_or_else(|| panic!("{card}"));
    assert!(code <= 2, "{card}");
    assert!(card["served_at_secs"].as_u64().is_some(), "{card}");

    let body = json!({
        "v": 1,
        "kind": "message",
        "id": "py-1",
        "content": "hello from the reference",
        "ts": unix_now(),
    });
    let received = reference
        .request(&node, "/message", reference.envelope(body))
        .await;
    assert_eq!(received, json!({ "received": true, "id": "py-1" }));
    let inbox = node.slot.peer_inbox();
    wait_until("the message to reach the inbox", || inbox.len() == 1).await;
    let (envelopes, dropped) = inbox.drain();
    assert_eq!(dropped, 0);
    let EnvelopePayload::Peer(message) = &envelopes[0].payload else {
        panic!("not a peer envelope: {:?}", envelopes[0].payload);
    };
    assert_eq!(message.kind, PeerKind::Message);
    assert_eq!(message.message_id, "py-1");
    assert_eq!(message.content, "hello from the reference");
    assert_eq!(message.source_identity, reference.ready.identity_hash);
    assert_eq!(message.source_destination, reference.ready.destination_hash);
    assert_eq!(message.via, PeerVia::Direct);

    let bogus = json!({
        "v": 1,
        "kind": "bogus",
        "id": "py-2",
        "content": "not a kind",
        "ts": unix_now(),
    });
    let invalid = reference
        .request(&node, "/message", reference.envelope(bogus))
        .await;
    assert_eq!(invalid, json!(INVALID_DATA));

    let future = reference
        .request(
            &node,
            "/status",
            json!({ "v": 2, "name_hash": reference.ready.name_hash, "body": null }),
        )
        .await;
    assert_eq!(
        future,
        json!({ "refusal": "unsupported_version", "found": 2, "min": 1, "max": 1 })
    );

    let nameless = reference
        .request(&node, "/status", json!({ "v": 1, "body": null }))
        .await;
    assert_eq!(nameless, json!(NO_ACCESS));

    let unknown = reference
        .request(&node, "/nope", reference.envelope(Value::Null))
        .await;
    assert_eq!(unknown["error"], json!("unknown_path"), "{unknown}");
    let path_hash = unknown["path_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("{unknown}"));
    assert!(is_hex_of_len(path_hash, 32), "{unknown}");

    node.stop().await;
    drop(reference);
}

/// Ids: `REQUEST_IDS`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn our_requests_are_decoded_by_the_reference() {
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    let (session, _) = session_with_instance_id();
    let node = Node::start(
        "interop-decoded",
        reference.ready.relay_port,
        session,
        &TrustList::default(),
    )
    .await;
    reference.announce(Some("Reference")).await;
    node.trust_reference(&reference).await;

    let message = OutboundPeer {
        kind: PeerKind::Message,
        id: "m-1".to_string(),
        in_reply_to: None,
        title: Some("plan".to_string()),
        content: "words for the reference".to_string(),
        fields: None,
    };
    let sent = node
        .runtime
        .send_peer(&reference.ready.destination_hash, &message)
        .await
        .unwrap();
    assert_eq!(sent.via, PeerVia::Direct, "{REQUEST_IDS:?}");

    let heard = reference.next_event("request", REQUEST_TIMEOUT).await;
    assert_eq!(heard["path"], json!("/message"), "{heard}");
    assert_eq!(heard["remote_identity"], json!(node.runtime.fingerprint()));
    let envelope = &heard["envelope"];
    assert_eq!(keys(envelope), ["v", "name_hash", "body"], "{heard}");
    assert_eq!(envelope["v"], json!(1));
    assert_eq!(envelope["name_hash"], json!(node.name_hash()));
    let body = &envelope["body"];
    assert_eq!(
        keys(body),
        ["v", "kind", "id", "title", "content", "ts"],
        "{heard}"
    );
    assert_eq!(body["v"], json!(1));
    assert_eq!(body["kind"], json!("message"));
    assert_eq!(body["id"], json!("m-1"));
    assert_eq!(body["title"], json!("plan"));
    assert_eq!(body["content"], json!("words for the reference"));
    assert!(
        body["ts"].as_f64().is_some_and(|ts| ts > 1_700_000_000.0),
        "{body}"
    );

    let desc = node
        .runtime
        .resolve_destination(&reference.ready.destination_hash)
        .await
        .expect("the reference's announce gives the node a description");
    let card = node.runtime.request_status(&desc).await.unwrap();
    assert_eq!(card.state.code, 1);
    assert!(card.served_at_secs > 1_700_000_000);
    let heard = reference.next_event("request", REQUEST_TIMEOUT).await;
    assert_eq!(heard["path"], json!("/status"), "{heard}");
    assert_eq!(heard["envelope"]["name_hash"], json!(node.name_hash()));
    assert_eq!(heard["envelope"]["body"], Value::Null, "{heard}");

    node.stop().await;
    drop(reference);
}

/// Ids: `PROPAGATION_COST_IDS` and `PROPAGATION_IDS`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn a_propagation_node_demanding_a_raised_stamp_cost_still_takes_our_message() {
    const COST: u32 = 18;
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    let (session, _) = session_with_instance_id();
    let node = Node::start(
        "interop-propagation",
        reference.ready.relay_port,
        session,
        &TrustList::default(),
    )
    .await;

    reference.announce(Some("Reference")).await;
    node.trust_reference(&reference).await;

    let pn = reference
        .send("pn_start", json!({ "cost": COST }), REQUEST_TIMEOUT)
        .await;
    assert_eq!(
        pn["stamp_cost"],
        json!(COST),
        "{PROPAGATION_COST_IDS:?}: {pn}"
    );
    let pn_hash = pn["destination_hash"].as_str().unwrap().to_string();
    let nodes = node.runtime.propagation_nodes();
    wait_until("the propagation node to be filed", || {
        nodes.snapshot().iter().any(|record| {
            record.node.destination.address_hash.to_hex_string() == pn_hash
                && record.node.stamp_cost == COST
                && record.node.propagation_enabled
        })
    })
    .await;

    reference
        .send("silence", json!({ "path": "/message" }), REQUEST_TIMEOUT)
        .await;

    let message = OutboundPeer {
        kind: PeerKind::Message,
        id: "m-2".to_string(),
        in_reply_to: None,
        title: Some("held".to_string()),
        content: "words for later".to_string(),
        fields: None,
    };
    let sent = tokio::time::timeout(
        STORE_AND_FORWARD_TIMEOUT,
        node.runtime
            .send_peer(&reference.ready.destination_hash, &message),
    )
    .await
    .expect("the send falls back before the suite's ceiling")
    .unwrap();
    assert_eq!(sent.via, PeerVia::StoreAndForward, "{PROPAGATION_IDS:?}");

    let deadline = Instant::now() + LINK_TIMEOUT;
    loop {
        let count = reference.send("pn_count", json!({}), REQUEST_TIMEOUT).await;
        if count["count"] == json!(1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the propagation node never stored the message: {count}"
        );
        sleep(POLL).await;
    }
    let stored = reference
        .send("pn_messages", json!({}), REQUEST_TIMEOUT)
        .await;
    let messages = stored["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "{stored}");
    let held = &messages[0];
    assert_eq!(held["content"], json!("words for later"), "{held}");
    assert_eq!(held["title"], json!("held"), "{held}");
    assert!(
        held["stamp_value"]
            .as_u64()
            .is_some_and(|value| value >= u64::from(COST)),
        "{held}"
    );
    assert_eq!(
        held["fields"][FIELD_CUSTOM_TYPE],
        json!("coyote.peer/1"),
        "{held}"
    );
    let custom = &held["fields"][FIELD_CUSTOM_DATA];
    assert_eq!(custom["kind"], json!("message"), "{held}");
    assert_eq!(custom["id"], json!("m-2"), "{held}");
    assert_eq!(custom["name_hash"], json!(node.name_hash()), "{held}");
    assert!(
        is_hex_of_len(held["destination_hash"].as_str().unwrap(), 32),
        "{held}"
    );

    node.stop().await;
    drop(reference);
}
