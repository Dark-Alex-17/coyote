//! Interop against the pinned Python Reticulum/LXMF reference, spawned as a subprocess.
//!
//! `Reference` runs `scripts/mesh-interop/reference_peer.py` and talks to it over JSON lines;
//! the reference is the TCP server and transport node, this crate's runtime dials it as a
//! `TcpClient`, the production relay topology.

use crate::config::Session;
use crate::config::mesh_config::{DEFAULT_LOCAL_RNSD_INSTANCE, MeshConfig, MeshInterface};
use crate::hooks::HookEvent;
use crate::mesh::announce::ANNOUNCE_MAGIC;
use crate::mesh::envoy::EnvoySink;
use crate::mesh::events::RecordingHookSink;
use crate::mesh::knock::{KnockIntro, KnockOutcome, KnockVia};
use crate::mesh::lan_count::{LanReading, ReadEffort, RnsdLanReader, RnsdLocator};
use crate::mesh::message::{OutboundPeer, PEER_MESSAGE_TYPE, PeerKind, PeerVia};
use crate::mesh::node::{
    FullEnvoy, InterfaceState, LanVerdict, MeshRuntime, MeshSlot, NodeOptions, RecordingEnvoy,
    lan_silent_verdict, session_destination_name,
};
use crate::mesh::notify::{NotificationSink, RenderedNotification};
use crate::mesh::propagation_nodes::PROPAGATION_NODE_TABLE_FILE;
use crate::mesh::request::FETCH_PATH;
use crate::mesh::test_support::{
    Compatibility, OriginName, TempDir, TrustList, disable_ingress_control, mesh_paths,
    private_config, snapshot_fixture, wait_until,
};
use crate::mesh::trust::TrustOptions;
use crate::mesh::{decode_hex, hex_lower, mesh_cache_dir, mesh_config_dir, short};
use crate::repl::idle::testing::driver_on_a_fresh_state;
use crate::supervisor::mailbox::EnvelopePayload;

use rns_transport::rand_core::{OsRng, RngCore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
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
const SCRIPT_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-interop/reference_peer.py"
));
const SETUP_SH_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/mesh-interop/setup.sh");

/// A link open or a path wait on the reference's side.
const LINK_TIMEOUT: Duration = Duration::from_secs(15);
/// One request to us, answered or timed out, on the reference's side.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A request to us that nothing answers, so the reference's own wait is the whole cost.
const UNANSWERED_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A command that has to wait for a request to us to time out before it can fall back.
const STORE_AND_FORWARD_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(50);

const NO_ACCESS: u64 = 0xf1;
const INVALID_DATA: u64 = 0xf4;
const THROTTLED: u64 = 0xf6;
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
        self.request_within(node, path, envelope, REQUEST_TIMEOUT)
            .await
    }

    /// `request` with the reference's own wait for the reply set to `timeout`: a reply
    /// that travels as a multi-segment Resource through Python RNS over loopback takes
    /// longer than a status round trip.
    async fn request_within(
        &mut self,
        node: &Node,
        path: &str,
        envelope: Value,
        timeout: Duration,
    ) -> Value {
        let reply = self.request_receipt(node, path, envelope, timeout).await;
        assert_eq!(reply["status"], json!("ready"), "{path}: {reply}");
        reply["response"].clone()
    }

    /// The reference's receipt for a request to `node`, however it ended: `status` is
    /// `ready` with the unpacked `response`, or `failed` with none once `timeout` passed.
    async fn request_receipt(
        &mut self,
        node: &Node,
        path: &str,
        envelope: Value,
        timeout: Duration,
    ) -> Value {
        self.send(
            "request",
            json!({
                "destination_hash": node.runtime.current_destination_hash(),
                "instance_id": node.instance_id,
                "path": path,
                "envelope": envelope,
                "timeout_secs": timeout.as_secs(),
            }),
            timeout + LINK_TIMEOUT,
        )
        .await
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
}

impl Drop for Reference {
    fn drop(&mut self) {
        self.shut_down();
        print_debug_log(self.debug_mark);
    }
}

/// This crate's captured debug log from `mark` on, and every warning, to stderr; nothing
/// without a mark.
fn print_debug_log(mark: Option<usize>) {
    let Some(mark) = mark else { return };
    for line in crate::testing::debug_snapshot().iter().skip(mark) {
        eprintln!("[coyote] DEBUG {line}");
    }
    for line in crate::testing::warn_snapshot() {
        eprintln!("[coyote] WARN {line}");
    }
}

/// The pinned Python `rnsd`, sharing its instance where no other daemon listens: this
/// crate's nodes attach to it as `local` clients, the way they attach to a host's rnsd.
/// `spawn_relay` runs it as a transport relay the nodes dial over TCP instead.
/// The config dir outlives a `respawn`, so the second rnsd is the first one restarted:
/// same endpoint, same `storage/`.
struct Rnsd {
    child: Child,
    config_dir: TempDir,
    python: PathBuf,
    pythonpath: String,
    entry: MeshInterface,
    /// As `Reference::debug_mark`: this crate's debug log from the spawn on is printed
    /// at teardown when `COYOTE_MESH_INTEROP_DEBUG` is set.
    debug_mark: Option<usize>,
}

impl Rnsd {
    /// `None` when the suite is not switched on; see `require_reference` for what happens
    /// once it is.
    async fn spawn(tag: &str) -> Option<Self> {
        let (instance, entry) = Self::instance();
        Self::spawn_with(
            tag,
            &format!(
                "[reticulum]\nenable_transport = No\nshare_instance = Yes\n{instance}[logging]\nloglevel = 6\n[interfaces]\n"
            ),
            entry,
        )
        .await
    }

    /// The pinned `rnsd` as a transport node that shares no instance and serves one
    /// `TCPServerInterface` on a loopback port of this run's own: the relay a node dials
    /// with a `private` entry, as it dials a team relay. `None` as for `spawn`.
    async fn spawn_relay(tag: &str) -> Option<Self> {
        let port = free_port();
        Self::spawn_with(
            tag,
            &format!(
                "[reticulum]\nenable_transport = Yes\nshare_instance = No\n[logging]\nloglevel = 6\n[interfaces]\n  [[Relay]]\n    type = TCPServerInterface\n    enabled = Yes\n    listen_ip = 127.0.0.1\n    listen_port = {port}\n"
            ),
            MeshInterface::Private {
                host: "127.0.0.1".to_string(),
                port,
            },
        )
        .await
    }

    /// rnsd on `config`, ready once `entry` answers.
    async fn spawn_with(tag: &str, config: &str, entry: MeshInterface) -> Option<Self> {
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
        let config_dir = TempDir::new(tag);
        fs::write(config_dir.path.join("config"), config).unwrap();
        let child = Self::launch(&python, &pythonpath, &config_dir);
        let mut rnsd = Self {
            child,
            config_dir,
            python,
            pythonpath,
            entry,
            debug_mark,
        };
        rnsd.wait_ready().await;
        Some(rnsd)
    }

    /// The `[reticulum]` lines that put the shared instance on an endpoint of this run's
    /// own, and the `local` entry a node attaches with. rnsd shares on an abstract socket
    /// named by `instance_name` on Linux and Android and on loopback TCP everywhere else.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn instance() -> (String, MeshInterface) {
        let name = format!("coyote-interop-{}-{}", std::process::id(), unix_nanos());
        (
            format!("instance_name = {name}\n"),
            MeshInterface::Local {
                port: None,
                instance: Some(name),
            },
        )
    }

    /// The TCP arm: rnsd also binds its RPC listener on `instance_control_port`, and the
    /// default is the host rnsd's, so both ports are this run's own. A shared port in
    /// use would turn this rnsd into a client of whoever holds it rather than fail.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn instance() -> (String, MeshInterface) {
        let (shared, control) = two_free_ports();
        (
            format!("shared_instance_port = {shared}\ninstance_control_port = {control}\n"),
            MeshInterface::Local {
                port: Some(shared),
                instance: None,
            },
        )
    }

    async fn answers(&self) -> bool {
        if let MeshInterface::Private { host, port } = &self.entry {
            return tokio::net::TcpStream::connect((host.as_str(), *port))
                .await
                .is_ok();
        }
        self.shared_instance_answers().await
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    async fn shared_instance_answers(&self) -> bool {
        use rns_transport::iface::local::{LocalUnixEndpoint, preflight_unix_connect};
        let MeshInterface::Local {
            instance: Some(name),
            ..
        } = &self.entry
        else {
            unreachable!("the Linux arm names its instance")
        };
        preflight_unix_connect(&LocalUnixEndpoint::from_config_value(&format!(
            "@rns/{name}"
        )))
        .await
        .is_ok()
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    async fn shared_instance_answers(&self) -> bool {
        let MeshInterface::Local {
            port: Some(port), ..
        } = &self.entry
        else {
            unreachable!("the TCP arm holds its port")
        };
        tokio::net::TcpStream::connect(("127.0.0.1", *port))
            .await
            .is_ok()
    }

    /// rnsd prints through a bare `print()`: a piped stdout nobody reads would block it,
    /// so it is dropped unless `COYOTE_MESH_INTEROP_DEBUG` asks for it.
    fn launch(python: &Path, pythonpath: &str, config_dir: &TempDir) -> Child {
        let stdout = if env::var_os("COYOTE_MESH_INTEROP_DEBUG").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        Command::new(python)
            .args(["-m", "RNS.Utilities.rnsd", "--config"])
            .arg(&config_dir.path)
            .arg("-vv")
            .env("PYTHONPATH", pythonpath)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to spawn {} -m RNS.Utilities.rnsd: {err}; run scripts/mesh-interop/setup.sh",
                    python.display()
                )
            })
    }

    async fn wait_ready(&mut self) {
        let deadline = Instant::now() + LINK_TIMEOUT;
        while !self.answers().await {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "rnsd exited {status} before it listened; run scripts/mesh-interop/setup.sh"
                );
            }
            assert!(
                Instant::now() < deadline,
                "rnsd did not listen at {} within {LINK_TIMEOUT:?}; run scripts/mesh-interop/setup.sh",
                self.entry
            );
            sleep(POLL).await;
        }
    }

    /// The entry a node joins this rnsd with: `local` for a shared instance, `private`
    /// for a relay.
    fn entry(&self) -> MeshInterface {
        self.entry.clone()
    }

    /// SIGTERM, then the exit status; a child still running five seconds later is killed.
    fn terminate(&mut self) -> ExitStatus {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status;
            }
            thread::sleep(POLL);
        }
        let _ = self.child.kill();
        self.child.wait().unwrap()
    }

    /// rnsd again on the same config dir: the restarted daemon the attached clients redial.
    async fn respawn(&mut self) {
        self.child = Self::launch(&self.python, &self.pythonpath, &self.config_dir);
        self.wait_ready().await;
    }
}

impl Drop for Rnsd {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        print_debug_log(self.debug_mark);
    }
}

/// A loopback port nothing holds at the time of the call.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Two distinct loopback ports nothing holds, both bound before either is released so
/// the second bind cannot be handed the first port back.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn two_free_ports() -> (u16, u16) {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    (
        first.local_addr().unwrap().port(),
        second.local_addr().unwrap().port(),
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A `discovery_port` and `data_port` for an rnsd AutoInterface of this run's own: the
/// host's own rnsd or `lan` interface holds the defaults. AutoInterface also binds the
/// port after `discovery_port` for unicast discovery, so that one is held free too, and
/// all three are bound before any is released.
fn auto_interface_ports() -> (u16, u16) {
    loop {
        let discovery = std::net::UdpSocket::bind("[::]:0").unwrap();
        let port = discovery.local_addr().unwrap().port();
        let Some(_unicast) = port
            .checked_add(1)
            .and_then(|next| std::net::UdpSocket::bind(("::", next)).ok())
        else {
            continue;
        };
        let data = std::net::UdpSocket::bind("[::]:0").unwrap();
        return (port, data.local_addr().unwrap().port());
    }
}

/// This crate's node joined to the reference's relay port, with a slot installed so
/// `/status` and `/message` are served.
struct Node {
    runtime: Arc<MeshRuntime>,
    slot: Arc<MeshSlot>,
    instance_id: String,
    port: u16,
    session: Session,
    /// Shared by every session of one user started on it; the dir is removed once the
    /// last of them is gone.
    tmp: Arc<TempDir>,
}

impl Node {
    /// `session` already carries the instance id, so a caller may have told the reference
    /// which aspect to watch before the start announce goes out.
    async fn start(tag: &str, port: u16, session: Session, trust: &TrustList) -> Self {
        Self::start_on(Arc::new(MeshSlot::default()), tag, port, session, trust).await
    }

    /// `start` on a slot the caller holds, such as an `AppState`'s.
    async fn start_on(
        slot: Arc<MeshSlot>,
        tag: &str,
        port: u16,
        session: Session,
        trust: &TrustList,
    ) -> Self {
        let tmp = TempDir::new(tag);
        trust.write(&mesh_paths(&tmp).config_dir);
        Self::join(slot, port, session, Arc::new(tmp)).await
    }

    /// A node whose only interface is `entry`, a `local` client of an `Rnsd`. `port` has
    /// no meaning for it and reads 0.
    async fn start_local(
        tag: &str,
        entry: MeshInterface,
        session: Session,
        trust: &TrustList,
    ) -> Self {
        let tmp = TempDir::new(tag);
        trust.write(&mesh_paths(&tmp).config_dir);
        let config = MeshConfig {
            interfaces: vec![entry],
            ..MeshConfig::default()
        };
        Self::join_with(
            Arc::new(MeshSlot::default()),
            &config,
            0,
            session,
            Arc::new(tmp),
        )
        .await
    }

    async fn join(slot: Arc<MeshSlot>, port: u16, session: Session, tmp: Arc<TempDir>) -> Self {
        Self::join_with(slot, &private_config(port), port, session, tmp).await
    }

    /// `join_limiting_ingress` with announce ingress control off, so announces sent close
    /// together are filed as soon as they arrive.
    async fn join_with(
        slot: Arc<MeshSlot>,
        config: &MeshConfig,
        port: u16,
        session: Session,
        tmp: Arc<TempDir>,
    ) -> Self {
        let node = Self::join_limiting_ingress(slot, config, port, session, tmp).await;
        disable_ingress_control(&node.runtime).await;
        node
    }

    /// A node on `tmp` with the transport's announce ingress control as it runs outside
    /// tests. Every node started on one `tmp` loads the same identity, so each is a session
    /// of one user; a distinct `session` gives each its own instance.
    async fn join_limiting_ingress(
        slot: Arc<MeshSlot>,
        config: &MeshConfig,
        port: u16,
        mut session: Session,
        tmp: Arc<TempDir>,
    ) -> Self {
        let runtime = MeshRuntime::start(
            config,
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
        slot.install(runtime.clone()).unwrap();
        Self {
            instance_id: runtime.current_instance_id(),
            runtime,
            slot,
            port,
            session,
            tmp,
        }
    }

    fn name_hash(&self) -> String {
        name_hash_of(&self.instance_id)
    }

    async fn trust_reference(&self, reference: &Reference) {
        trust_peer(self, &reference.ready.destination_hash).await;
    }

    async fn stop(self) {
        assert!(self.slot.stop().await.unwrap());
    }

    /// Stops the node and keeps what a rejoin needs: the slot, the session with its
    /// instance id, and the identity and cache on disk.
    async fn leave(self) -> Parked {
        assert!(self.slot.stop().await.unwrap());
        Parked {
            slot: self.slot,
            port: self.port,
            session: self.session,
            tmp: self.tmp,
        }
    }
}

/// A node between `leave` and `rejoin`.
struct Parked {
    slot: Arc<MeshSlot>,
    port: u16,
    session: Session,
    tmp: Arc<TempDir>,
}

impl Parked {
    /// The same node again: same destination, same slot, a fresh transport.
    async fn rejoin(self) -> Node {
        Node::join(self.slot, self.port, self.session, self.tmp).await
    }
}

fn session_with_instance_id() -> (Session, String) {
    let mut session = Session::default();
    let instance_id = session.ensure_mesh_instance_id().to_string();
    (session, instance_id)
}

/// This crate's name hash for `scope.session.<instance_id>`.
fn name_hash_of(instance_id: &str) -> String {
    let name = session_destination_name(instance_id);
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

    let magic = str::from_utf8(&ANNOUNCE_MAGIC).unwrap();
    for present in [
        format!("`{magic}`"),
        "scope.session.<instance_id>".to_string(),
    ] {
        assert!(
            HARNESS_README.contains(&present),
            "scripts/mesh-interop/README.md does not spell {present:?}"
        );
    }
    // Assembled at runtime so the mesh source guard does not match this test's text.
    let old_app = ["coy", "ote"].concat();
    for absent in [
        ["COY", "M"].concat(),
        format!("{old_app}.mesh"),
        format!("{old_app}.peer/"),
        format!("{old_app}.knock/"),
    ] {
        assert!(
            !HARNESS_README.contains(&absent),
            "scripts/mesh-interop/README.md still spells {absent:?}"
        );
        assert!(
            !SCRIPT_SOURCE.contains(&absent),
            "scripts/mesh-interop/reference_peer.py still spells {absent:?}"
        );
    }
    for line in [
        format!("MESH_APP = {:?}", "scope"),
        format!("ANNOUNCE_MAGIC = b{magic:?}"),
        format!("return ({:?}, instance_id)", "session"),
    ] {
        assert!(
            SCRIPT_SOURCE.lines().any(|l| l.trim() == line),
            "scripts/mesh-interop/reference_peer.py does not spell {line:?}"
        );
    }
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

/// The setup script, probed on the script itself rather than its text: `setup.sh` honours
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
        "the reference heard our announce: {heard}"
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

/// Ceiling on the automatic fetch after a rejoin: the join-time sync finds no node yet and
/// asks again after `PROPAGATION_SYNC_SHORT_RETRY`, by when the node is filed.
const AUTOMATIC_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Printed(parking_lot::Mutex<Vec<String>>);

impl NotificationSink for Printed {
    fn notify(&self, rendered: RenderedNotification) {
        self.0.lock().extend(rendered.lines().iter().cloned());
    }
}

/// Trusts `destination` on `node` once its announce has been filed.
async fn trust_peer(node: &Node, destination: &str) {
    let peers = node.runtime.peers();
    wait_until("the node to file the peer", || {
        peers.get(destination).is_some()
    })
    .await;
    node.runtime
        .trust()
        .trust_destination(
            node.slot.as_ref(),
            destination,
            TrustOptions::default(),
            SystemTime::now(),
        )
        .unwrap();
}

async fn wait_for_propagation_node(node: &Node, pn_hash: &str) {
    let nodes = node.runtime.propagation_nodes();
    wait_until("the propagation node to be filed", || {
        nodes
            .snapshot()
            .iter()
            .any(|record| record.node.destination.address_hash.to_hex_string() == pn_hash)
    })
    .await;
}

async fn wait_for_pn_count(reference: &mut Reference, expected: u64) {
    let deadline = Instant::now() + LINK_TIMEOUT;
    loop {
        let count = reference.send("pn_count", json!({}), REQUEST_TIMEOUT).await;
        if count["count"] == json!(expected) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the propagation node never held {expected} messages: {count}"
        );
        sleep(POLL).await;
    }
}

/// The receiver runs under an `AppState` with the REPL's idle-time driver on its slot, so
/// what fetches the held message is the sync the driver runs when the node joins, against
/// the propagation node its table kept across the stop, and the retries that sync arms,
/// not a call from the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn a_message_spooled_while_the_receiver_is_off_is_delivered_after_it_rejoins_without_a_command()
 {
    spooled_message_is_delivered_after_the_rejoin(NodeTableAtRejoin::Kept).await;
}

/// The same with the saved node table gone while the receiver is off: the join-time sync
/// finds no propagation node, and what fetches the held message is the driver's retry once
/// the node announces itself again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn a_spooled_message_is_delivered_after_a_rejoin_with_no_node_on_file_once_the_node_announces()
 {
    spooled_message_is_delivered_after_the_rejoin(NodeTableAtRejoin::Forgotten).await;
}

/// What the receiver's propagation-node table holds when it rejoins.
#[derive(Clone, Copy, PartialEq)]
enum NodeTableAtRejoin {
    /// The table saved at stop, loaded again at start.
    Kept,
    /// The saved table removed while the receiver was off, so it starts empty.
    Forgotten,
}

async fn spooled_message_is_delivered_after_the_rejoin(table: NodeTableAtRejoin) {
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    let (session, _) = session_with_instance_id();
    let sender = Node::start(
        "interop-sync-sender",
        reference.ready.relay_port,
        session,
        &TrustList::default(),
    )
    .await;

    let (app, driver) = driver_on_a_fresh_state();
    let printed = Arc::new(Printed::default());
    app.mesh
        .set_notifier(Arc::clone(&printed) as Arc<dyn NotificationSink>);
    let (session, _) = session_with_instance_id();
    // With the node kept, the rejoin's own sync screens the held message before any call
    // could trust the sender on the live node, and an untrusted signer's message is
    // discarded, so the receiver's trust file names the sender from the start; the
    // sender's destination survives its restart below.
    let trusts_sender = TrustList::default().destination(
        &sender.runtime.current_destination_hash(),
        sender.runtime.fingerprint(),
    );
    let receiver = Node::start_on(
        Arc::clone(&app.mesh),
        "interop-sync-receiver",
        reference.ready.relay_port,
        session,
        &trusts_sender,
    )
    .await;
    let receiver_destination = receiver.runtime.current_destination_hash();
    // Announces reach only the nodes joined when they are sent: the sender, up first, hears
    // the receiver's.
    trust_peer(&sender, &receiver_destination).await;

    let pn = reference
        .send("pn_start", json!({ "cost": 0 }), REQUEST_TIMEOUT)
        .await;
    let pn_hash = pn["destination_hash"].as_str().unwrap().to_string();
    wait_for_propagation_node(&sender, &pn_hash).await;
    wait_for_propagation_node(&receiver, &pn_hash).await;

    let parked = receiver.leave().await;
    let left_at = SystemTime::now();
    assert!(app.mesh.get().is_none());

    let message = OutboundPeer {
        kind: PeerKind::Message,
        id: "held-1".to_string(),
        in_reply_to: None,
        title: None,
        content: "words for later".to_string(),
        fields: None,
        parts: Vec::new(),
        thread: None,
        disposition: None,
        retry_after: None,
    };
    let sent = tokio::time::timeout(
        STORE_AND_FORWARD_TIMEOUT,
        sender.runtime.send_peer(&receiver_destination, &message),
    )
    .await
    .expect("the send falls back before the suite's ceiling")
    .unwrap();
    assert_eq!(
        sent.via,
        PeerVia::StoreAndForward,
        "the message went store-and-forward"
    );
    wait_for_pn_count(&mut reference, 1).await;

    if table == NodeTableAtRejoin::Forgotten {
        fs::remove_file(
            mesh_cache_dir(&mesh_paths(&parked.tmp).cache_dir).join(PROPAGATION_NODE_TABLE_FILE),
        )
        .expect("the stop saved the node table");
    }
    let receiver = parked.rejoin().await;
    assert!(app.mesh.get().is_some());
    let filed = receiver.runtime.propagation_nodes().snapshot();
    let on_file: Vec<_> = filed
        .iter()
        .map(|record| {
            (
                record.node.destination.address_hash.to_hex_string(),
                record.last_seen,
            )
        })
        .collect();
    match table {
        // The node table is saved at stop and loaded at start. Heard no later than the
        // stop means the node came from disk: a delayed self-announce from the reference
        // landing after the rejoin would carry a later `last_seen`, and nothing below
        // announces the node again.
        NodeTableAtRejoin::Kept => assert!(
            on_file.len() == 1 && on_file[0].0 == pn_hash && on_file[0].1 <= left_at,
            "the rejoined node starts with only the propagation node it saved at stop: {on_file:?}"
        ),
        NodeTableAtRejoin::Forgotten => assert!(
            on_file.is_empty(),
            "with no table on file, a node filed this early means the reference's delayed self-announce landed inside the off-window: {on_file:?}"
        ),
    }
    // The rejoined transport knows no identities yet; the sender's start announce, or the
    // sync's own ask of the mesh, brings back the key the held message was signed with.
    let sender = sender.leave().await.rejoin().await;
    if table == NodeTableAtRejoin::Forgotten {
        reference
            .send("pn_announce", json!({}), REQUEST_TIMEOUT)
            .await;
        wait_for_propagation_node(&receiver, &pn_hash).await;
    }

    let inbox = app.mesh.peer_inbox();
    let deadline = Instant::now() + AUTOMATIC_SYNC_TIMEOUT;
    while inbox.len() == 0 {
        assert!(
            Instant::now() < deadline,
            "the driver never fetched the held message; printed: {:?}",
            printed.0.lock()
        );
        sleep(POLL).await;
    }
    let (envelopes, dropped) = inbox.drain();
    assert_eq!(dropped, 0);
    assert_eq!(envelopes.len(), 1, "{envelopes:?}");
    let EnvelopePayload::Peer(received) = &envelopes[0].payload else {
        panic!("not a peer envelope: {:?}", envelopes[0].payload);
    };
    assert_eq!(received.message_id, "held-1");
    assert_eq!(received.content, "words for later");
    assert_eq!(received.via, PeerVia::StoreAndForward);
    assert_eq!(
        received.source_destination,
        sender.runtime.current_destination_hash()
    );
    let synced_line = format!(
        "[mesh:propagation] synced 1 message from {}: 1 delivered",
        short(&pn_hash)
    );
    wait_until("the sync line to be printed", || {
        printed.0.lock().contains(&synced_line)
    })
    .await;
    wait_for_pn_count(&mut reference, 0).await;

    driver.stop().await;
    assert!(app.mesh.stop().await.unwrap());
    sender.stop().await;
    drop(reference);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn reference_requests_hear_the_specified_replies() {
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    // The identity is known but its instance is not trusted: the default-closed verdict,
    // which knocks and refuses. An unknown identity would hear silence instead.
    let known_identity = TrustList::default().identity(&reference.ready.identity_hash, false);
    let (mut session, _) = session_with_instance_id();
    session.set_name("interop-node".to_string());
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
    assert_eq!(
        refused,
        json!(NO_ACCESS),
        "an untrusted identity is refused with no_access"
    );
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
    assert_eq!(card["v"], json!(1), "the card is version 1: {card}");
    let code = card["state"]["code"]
        .as_u64()
        .unwrap_or_else(|| panic!("{card}"));
    assert!(code <= 2, "{card}");
    assert!(card["served_at_secs"].as_u64().is_some(), "{card}");
    assert_eq!(
        card["session"],
        json!("interop-node"),
        "Coyote shares the session name by default: {card}"
    );
    assert_eq!(
        keys(&card)[..2],
        ["v", "session"],
        "the session name follows `v` and the absent display name: {card}"
    );
    assert!(
        card.get("person_name").is_none(),
        "no person_name is configured, so none is sent: {card}"
    );

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

    // A path no handler is registered for is dropped by the library at the node, so the
    // reference's request runs out its own wait with nothing to show.
    let unknown = reference
        .request_receipt(
            &node,
            "/nope",
            reference.envelope(Value::Null),
            UNANSWERED_REQUEST_TIMEOUT,
        )
        .await;
    assert_eq!(unknown["status"], json!("failed"), "{unknown}");
    assert_eq!(unknown["response"], Value::Null, "{unknown}");

    // The dropped path left the node serving.
    let card = reference
        .request(&node, "/status", reference.envelope(Value::Null))
        .await;
    assert_eq!(
        card["v"],
        json!(1),
        "a status request after the dropped path is answered: {card}"
    );

    // The reference's farewell, a bulletin whose `fields.presence` is `leaving`, is
    // acknowledged by id and marks its row left; nothing is filed or counted, and its
    // next announce, or any other request it makes over a link, lifts the mark.
    let identity = reference.ready.identity_hash.clone();
    let messages = || {
        node.slot
            .limits()
            .window_of(&identity, Instant::now())
            .map_or(0, |window| window.messages)
    };
    let counted = messages();
    let farewell = json!({
        "v": 1,
        "kind": "bulletin",
        "id": "py-farewell",
        "content": "leaving",
        "fields": { "presence": "leaving" },
        "ts": unix_now(),
    });
    let received = reference
        .request(&node, "/message", reference.envelope(farewell))
        .await;
    assert_eq!(received, json!({ "received": true, "id": "py-farewell" }));
    let peers = node.runtime.peers();
    let reference_destination = reference.ready.destination_hash.clone();
    let left_at = || {
        peers
            .get(&reference_destination)
            .and_then(|row| row.left_at)
    };
    assert!(left_at().is_some(), "the farewell marks the reference left");
    assert_eq!(node.slot.peer_inbox().len(), 0, "a farewell is not filed");
    assert_eq!(messages(), counted, "a farewell is not counted");
    reference.announce(Some("Reference")).await;
    wait_until("the announce to lift the left mark", || left_at().is_none()).await;

    node.stop().await;
    drop(reference);
}

/// The node-wide gate first (a full envoy queue), then a per-identity one (the reference
/// already has a run in flight): both answer the reference's message with the bare
/// `Throttled` code before any acknowledgement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn a_reference_message_the_envoy_could_not_run_hears_throttled_before_any_ack() {
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    let (session, _) = session_with_instance_id();
    let node = Node::start(
        "interop-throttled",
        reference.ready.relay_port,
        session,
        &TrustList::default(),
    )
    .await;
    reference.announce(Some("Reference")).await;
    node.trust_reference(&reference).await;
    node.slot
        .set_envoy(Arc::new(FullEnvoy) as Arc<dyn EnvoySink>);

    let body = json!({
        "v": 1,
        "kind": "message",
        "id": "py-throttled",
        "content": "words the envoy has no room for",
        "ts": unix_now(),
    });
    let refused = reference
        .request(&node, "/message", reference.envelope(body))
        .await;
    assert_eq!(
        refused,
        json!(THROTTLED),
        "a full envoy queue answers throttled"
    );
    assert_eq!(
        node.slot.peer_inbox().len(),
        0,
        "a throttled message never reaches the inbox"
    );
    let pending = node
        .slot
        .inbound_store()
        .unwrap()
        .list(SystemTime::now())
        .unwrap();
    assert!(
        pending.is_empty(),
        "a throttled message is not stored: {pending:?}"
    );
    let identity = reference.ready.identity_hash.clone();
    let messages = || {
        node.slot
            .limits()
            .window_of(&identity, Instant::now())
            .map_or(0, |window| window.messages)
    };
    assert_eq!(messages(), 0, "a refused message is not counted");

    let bulletin = json!({
        "v": 1,
        "kind": "bulletin",
        "id": "py-bulletin",
        "content": "a bulletin is not the envoy's",
        "ts": unix_now(),
    });
    let received = reference
        .request(&node, "/message", reference.envelope(bulletin))
        .await;
    assert_eq!(
        received,
        json!({ "received": true, "id": "py-bulletin" }),
        "a bulletin is received while the envoy queue is full"
    );
    let inbox = node.slot.peer_inbox();
    wait_until("the bulletin to reach the inbox", || inbox.len() == 1).await;
    let (envelopes, dropped) = inbox.drain();
    assert_eq!(dropped, 0);
    let EnvelopePayload::Peer(message) = &envelopes[0].payload else {
        panic!("not a peer envelope: {:?}", envelopes[0].payload);
    };
    assert_eq!(message.kind, PeerKind::Bulletin);
    assert_eq!(message.message_id, "py-bulletin");
    assert_eq!(messages(), 1, "the bulletin is counted");

    let envoy = RecordingEnvoy::new(true, false);
    node.slot.set_envoy(envoy.clone() as Arc<dyn EnvoySink>);
    let held = node
        .slot
        .limits()
        .try_reserve(&identity, Instant::now())
        .unwrap();
    let while_in_flight = json!({
        "v": 1,
        "kind": "message",
        "id": "py-in-flight",
        "content": "words while a run of ours is in flight",
        "ts": unix_now(),
    });
    let refused = reference
        .request(&node, "/message", reference.envelope(while_in_flight))
        .await;
    assert_eq!(
        refused,
        json!(THROTTLED),
        "a run in flight answers throttled"
    );
    assert!(
        envoy.job_ids().is_empty(),
        "the envoy never saw the throttled message"
    );
    assert_eq!(
        inbox.len(),
        0,
        "the throttled message never reached the inbox"
    );
    assert_eq!(
        messages(),
        1,
        "the message refused for the run in flight is not counted"
    );

    drop(held);
    let after = json!({
        "v": 1,
        "kind": "message",
        "id": "py-after",
        "content": "words once the run has ended",
        "ts": unix_now(),
    });
    let received = reference
        .request(&node, "/message", reference.envelope(after))
        .await;
    assert_eq!(
        received,
        json!({ "received": true, "id": "py-after" }),
        "a message once the run has ended is received"
    );
    wait_until("the envoy to take the message", || {
        envoy.job_ids() == ["py-after"]
    })
    .await;
    assert_eq!(inbox.len(), 0, "the envoy took the message, not the inbox");
    assert_eq!(messages(), 2, "the received message is counted");

    node.stop().await;
    drop(reference);
}

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
        title: Some("hello".to_string()),
        content: "words for the reference".to_string(),
        fields: None,
        parts: Vec::new(),
        thread: None,
        disposition: None,
        retry_after: None,
    };
    let sent = node
        .runtime
        .send_peer(&reference.ready.destination_hash, &message)
        .await
        .unwrap();
    assert_eq!(sent.via, PeerVia::Direct, "the message went direct");

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
    assert_eq!(body["title"], json!("hello"));
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
    let card = node
        .runtime
        .request_status(crate::mesh::message::Route::unasked(desc))
        .await
        .unwrap();
    assert_eq!(card.state.code, 1);
    assert!(card.served_at_secs > 1_700_000_000);
    assert_eq!(
        (card.session.as_deref(), card.person_name.as_deref()),
        (Some("reference-session"), Some("Reference Person")),
        "the reference's names are read cleaned"
    );
    let people = node.runtime.trust().people(&[]);
    assert_eq!(
        people
            .person(&reference.ready.identity_hash)
            .map(|alias| alias.text.as_str()),
        Some("Reference Person"),
        "a trusted card's person_name is kept as the identity's advertised name"
    );
    assert_eq!(
        people
            .session(
                &reference.ready.identity_hash,
                &reference.ready.destination_hash
            )
            .map(|alias| alias.text.as_str()),
        Some("reference-session"),
        "a trusted card's session is kept as the destination's advertised name"
    );
    let heard = reference.next_event("request", REQUEST_TIMEOUT).await;
    assert_eq!(heard["path"], json!("/status"), "{heard}");
    assert_eq!(heard["envelope"]["name_hash"], json!(node.name_hash()));
    assert_eq!(heard["envelope"]["body"], Value::Null, "{heard}");

    // Leaving says goodbye to the trusted reference over the link: a bulletin whose
    // `fields.presence` is `leaving` and whose words are fixed.
    let node_identity = node.runtime.fingerprint().to_string();
    let stopping = Instant::now();
    node.stop().await;
    let heard = reference.next_event("request", REQUEST_TIMEOUT).await;
    assert_eq!(heard["path"], json!("/message"), "{heard}");
    assert_eq!(heard["remote_identity"], json!(node_identity));
    let body = &heard["envelope"]["body"];
    let mut body_keys = keys(body);
    body_keys.sort_unstable();
    assert_eq!(
        body_keys,
        ["content", "fields", "id", "kind", "ts", "v"],
        "{heard}"
    );
    assert!(
        body["ts"].as_f64().is_some_and(|ts| ts > 1_700_000_000.0),
        "{body}"
    );
    assert_eq!(body["v"], json!(1), "{heard}");
    assert_eq!(body["kind"], json!("bulletin"), "{heard}");
    assert_eq!(body["content"], json!("leaving"), "{heard}");
    assert_eq!(body["fields"], json!({ "presence": "leaving" }), "{heard}");
    assert!(
        stopping.elapsed() < REQUEST_TIMEOUT,
        "the farewell never holds the leave up: {:?}",
        stopping.elapsed()
    );
    drop(reference);
}

/// A file of `MAX_EFFICIENT_SIZE` plus 512 KiB spans two Resource segments. The reference
/// fetches it from this node over `/fetch` and hands back what Python RNS reassembled, so
/// the bytes are compared across implementations and not between two copies of this crate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn a_reference_fetch_of_a_file_above_one_segment_arrives_whole() {
    const MULTI_SEGMENT_FETCH_TIMEOUT: Duration = Duration::from_secs(90);
    let Some(mut reference) = Reference::spawn().await else {
        return;
    };
    let (session, _) = session_with_instance_id();
    let node = Node::start(
        "interop-fetch-big",
        reference.ready.relay_port,
        session,
        &TrustList::default(),
    )
    .await;
    reference.announce(Some("Reference")).await;
    node.trust_reference(&reference).await;
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

    let shares = mesh_config_dir(&mesh_paths(&node.tmp).config_dir).join("shares.yaml");
    std::fs::create_dir_all(shares.parent().unwrap()).unwrap();
    std::fs::write(shares, "version: 1\nallow:\n- pattern: 'docs/**'\n").unwrap();
    let workspace = TempDir::new("interop-fetch-big-root");
    std::fs::create_dir_all(workspace.path.join("docs")).unwrap();
    let mut bytes = vec![0u8; crate::mesh::test_support::MULTI_SEGMENT_BYTES];
    RngCore::fill_bytes(&mut OsRng, &mut bytes);
    std::fs::write(workspace.path.join("docs").join("big.bin"), &bytes).unwrap();
    let mut snapshot = snapshot_fixture();
    snapshot.cwd = workspace.path.clone();
    node.slot.publish(snapshot);

    let reply = reference
        .request_within(
            &node,
            FETCH_PATH,
            reference.envelope(json!({ "v": 1, "path": "docs/big.bin", "if_sha256": null })),
            MULTI_SEGMENT_FETCH_TIMEOUT,
        )
        .await;
    assert_eq!(reply["status"], json!("ok"), "{}", short_reply(&reply));
    assert_eq!(reply["size"], json!(bytes.len()), "{}", short_reply(&reply));
    let received = decode_hex(reply["bytes"].as_str().expect("bytes travel as hex"))
        .expect("the reference renders bytes as hex");
    assert_eq!(received.len(), bytes.len());
    assert!(received == bytes, "the fetched bytes differ from the file");
    assert_eq!(
        reply["sha256"],
        json!(hex_lower(&Sha256::digest(&bytes))),
        "{}",
        short_reply(&reply)
    );

    node.stop().await;
    drop(reference);
}

/// `reply` with its `bytes` field cut to its length, so an assertion message does not
/// print megabytes of hex.
fn short_reply(reply: &Value) -> String {
    let mut shown = reply.clone();
    if let Some(hex) = shown["bytes"].as_str() {
        shown["bytes"] = json!(format!("<{} hex chars>", hex.len()));
    }
    shown.to_string()
}

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
        "the propagation node announces the raised stamp cost: {pn}"
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
        parts: Vec::new(),
        thread: None,
        disposition: None,
        retry_after: None,
    };
    let sent = tokio::time::timeout(
        STORE_AND_FORWARD_TIMEOUT,
        node.runtime
            .send_peer(&reference.ready.destination_hash, &message),
    )
    .await
    .expect("the send falls back before the suite's ceiling")
    .unwrap();
    assert_eq!(
        sent.via,
        PeerVia::StoreAndForward,
        "the message went store-and-forward"
    );

    wait_for_pn_count(&mut reference, 1).await;
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
        json!(PEER_MESSAGE_TYPE),
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

/// What a `local` row's `Disconnected` carries once rnsd drops the socket. `node.rs`
/// renders it from the client's `PeerClosed` reason; a drift there fails this suite
/// rather than passing it quietly.
const RNSD_DROP_REASON: &str = "rnsd closed the socket";
/// How long after SIGTERM both `local` rows must read lost: the client sees the socket
/// close as soon as rnsd detaches its interfaces.
const RNSD_LOST_DEADLINE: Duration = Duration::from_secs(3);
/// How long after the respawn both `local` rows must read connected: two reconnect waits
/// of the client's two-second cadence, with slack for rnsd to listen again.
const RNSD_RECONNECTED_DEADLINE: Duration = Duration::from_secs(7);

/// Waits until the interface at `index` reads `state` or `deadline` passes.
async fn wait_for_state(
    runtime: &MeshRuntime,
    index: usize,
    what: &str,
    deadline: Instant,
    state: impl Fn(&InterfaceState) -> bool,
) {
    while !state(&runtime.interface_states()[index].state) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; states {:?}",
            runtime
                .interface_states()
                .into_iter()
                .map(|status| (status.label, status.state))
                .collect::<Vec<_>>()
        );
        sleep(POLL).await;
    }
}

/// Two nodes attached to one rnsd as `local` clients reach each other through it, both
/// rows follow it down when it is killed and back when it is restarted on the same
/// endpoint, and a knock lands again after the restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn two_local_clients_of_one_rnsd_knock_through_it_and_follow_it_down_and_back() {
    let Some(mut rnsd) = Rnsd::spawn("interop-rnsd").await else {
        return;
    };
    let a = Node::start_local(
        "interop-rnsd-a",
        rnsd.entry(),
        Session::default(),
        &TrustList::default(),
    )
    .await;
    // B knows A's identity without trusting its instance, so A's knock is answered
    // `NoAccess` (landed) rather than met with the silence a stranger hears.
    let b = Node::start_local(
        "interop-rnsd-b",
        rnsd.entry(),
        Session::default(),
        &TrustList::default().identity(a.runtime.fingerprint(), false),
    )
    .await;
    for node in [&a, &b] {
        assert_eq!(node.runtime.interface_kinds(), ["local"]);
        assert_eq!(
            node.runtime.interface_states()[0].state,
            InterfaceState::Connected
        );
    }

    let b_hash = b.runtime.current_destination_hash();
    let peers = a.runtime.peers();
    wait_until("A to file B's announce relayed by rnsd", || {
        peers.get(&b_hash).is_some()
    })
    .await;
    let b_desc = a.runtime.resolve_destination(&b_hash).await.unwrap();
    let outcome = a
        .runtime
        .knock(
            crate::mesh::message::Route::unasked(b_desc),
            &KnockIntro::new("hi from A").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        KnockOutcome {
            via: KnockVia::Direct,
            fallback: None,
        }
    );
    let gate = b.runtime.knock_gate();
    wait_until("B to file A's knock", || {
        gate.cache().list(SystemTime::now()).unwrap().len() == 1
    })
    .await;
    let filed = gate.cache().list(SystemTime::now()).unwrap();
    assert_eq!(filed[0].identity_hash, a.runtime.fingerprint());
    assert_eq!(
        filed[0].destination_hash,
        a.runtime.current_destination_hash()
    );
    assert_eq!(filed[0].intro.as_deref(), Some("hi from A"));

    let killed = Instant::now();
    let status = rnsd.terminate();
    assert!(status.success(), "rnsd exited {status} on SIGTERM");
    let lost = InterfaceState::Disconnected {
        reason: RNSD_DROP_REASON.to_string(),
    };
    for (node, what) in [
        (&a, "A's local row to read lost"),
        (&b, "B's local row to read lost"),
    ] {
        wait_for_state(
            &node.runtime,
            0,
            what,
            killed + RNSD_LOST_DEADLINE,
            |state| *state == lost,
        )
        .await;
        let state = node.runtime.interface_states()[0].state.to_string();
        assert!(state.starts_with("lost, retrying:"), "{state}");
        assert_eq!(state, "lost, retrying: rnsd closed the socket");
    }
    eprintln!(
        "both local rows read lost {:?} after SIGTERM",
        killed.elapsed()
    );

    // Sampled only now: rnsd goes on accepting while its exit handler runs, so a client's
    // first redial can land on the dying daemon and its re-announce reach A through it.
    // With the old rnsd reaped and both rows lost, anything A hears next came through
    // the new one.
    let b_heard_before = peers.get(&b_hash).unwrap().last_seen;
    let respawned = Instant::now();
    rnsd.respawn().await;
    for (node, what) in [
        (&a, "A's local row to read connected again"),
        (&b, "B's local row to read connected again"),
    ] {
        wait_for_state(
            &node.runtime,
            0,
            what,
            respawned + RNSD_RECONNECTED_DEADLINE,
            |state| *state == InterfaceState::Connected,
        )
        .await;
    }
    eprintln!(
        "both local rows read connected {:?} after the respawn",
        respawned.elapsed()
    );

    // Each client re-announces over its restored socket; A's path to B is good again
    // once B's re-announce has come through the new rnsd.
    wait_until(
        "A to hear B's re-announce through the restarted rnsd",
        || {
            peers
                .get(&b_hash)
                .is_some_and(|peer| peer.last_seen > b_heard_before)
        },
    )
    .await;
    let again = a
        .runtime
        .knock(
            crate::mesh::message::Route::unasked(b_desc),
            &KnockIntro::new("hi again from A").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        again,
        KnockOutcome {
            via: KnockVia::Direct,
            fallback: None,
        }
    );
    wait_until("B to file A's second knock", || {
        gate.cache()
            .list(SystemTime::now())
            .unwrap()
            .iter()
            .any(|knock| knock.intro.as_deref() == Some("hi again from A"))
    })
    .await;

    a.stop().await;
    b.stop().await;
    let status = rnsd.terminate();
    assert!(status.success(), "rnsd exited {status} on SIGTERM");
}

/// How soon after the second of two sessions of one user begins to start a third client
/// of the same rnsd must have filed both of their announces; the bound covers the
/// session's own start, which sends the announce. A shared-instance client used to hold
/// the second of two local announces sent less than about 0.6 s apart for 360 s.
const SECOND_SESSION_FILED_WITHIN: Duration = Duration::from_secs(1);

/// Whether the nodes of `OneUserTwoSessions` keep the transport's announce ingress
/// control on.
#[derive(Clone, Copy)]
enum AnnounceIngress {
    /// As a node runs outside tests.
    Limited,
    /// As `Node::join_with` leaves it.
    Unlimited,
}

/// Two sessions of one user, A1 and A2, and a third user B, all joined with one config.
/// The sessions share a dir, so they load one identity and one transport identity,
/// while each has an instance and a destination of its own.
struct OneUserTwoSessions {
    b: Node,
    a1: Node,
    a2: Node,
    a1_fires: Arc<RecordingHookSink>,
    a2_fires: Arc<RecordingHookSink>,
    /// When A2 began to start: with A1 when they start together, else once B had filed
    /// A1.
    a2_starting: Instant,
}

/// When the second of A's sessions starts.
#[derive(Clone, Copy)]
enum SecondSession {
    /// With the first, so their start announces go out close together.
    Together,
    /// Once B has filed the first's announce, so the network already holds a path to
    /// the first session when the second, on the same identity, joins.
    OnceTheFirstIsHeard,
}

impl OneUserTwoSessions {
    /// B starts first, then A1 and A2 as `second` says. Both know B's identity without
    /// trusting it, so B's knock is answered `NoAccess` (landed) rather than met with the
    /// silence a stranger hears.
    async fn start(
        tag: &str,
        config: &MeshConfig,
        ingress: AnnounceIngress,
        second: SecondSession,
    ) -> Self {
        let join = |tmp: Arc<TempDir>| {
            let slot = Arc::new(MeshSlot::default());
            let session = Session::default();
            async move {
                match ingress {
                    AnnounceIngress::Limited => {
                        Node::join_limiting_ingress(slot, config, 0, session, tmp).await
                    }
                    AnnounceIngress::Unlimited => {
                        Node::join_with(slot, config, 0, session, tmp).await
                    }
                }
            }
        };
        let b_dir = TempDir::new(&format!("{tag}-b"));
        TrustList::default().write(&mesh_paths(&b_dir).config_dir);
        let b = join(Arc::new(b_dir)).await;
        let a_dir = TempDir::new(&format!("{tag}-a"));
        TrustList::default()
            .identity(b.runtime.fingerprint(), false)
            .write(&mesh_paths(&a_dir).config_dir);
        let a_dir = Arc::new(a_dir);
        let (a1, a2, a2_starting) = match second {
            SecondSession::Together => {
                let a2_starting = Instant::now();
                let (a1, a2) = tokio::join!(join(a_dir.clone()), join(a_dir));
                (a1, a2, a2_starting)
            }
            SecondSession::OnceTheFirstIsHeard => {
                let a1 = join(a_dir.clone()).await;
                let a1_hash = a1.runtime.current_destination_hash();
                let peers = b.runtime.peers();
                wait_until("B to file A1's announce before A2 starts", || {
                    peers.get(&a1_hash).is_some()
                })
                .await;
                let a2_starting = Instant::now();
                let a2 = join(a_dir).await;
                (a1, a2, a2_starting)
            }
        };

        assert_eq!(a1.runtime.fingerprint(), a2.runtime.fingerprint());
        assert_ne!(
            a1.runtime.current_destination_hash(),
            a2.runtime.current_destination_hash()
        );
        Self {
            a1_fires: RecordingHookSink::attach(a1.runtime.hooks()),
            a2_fires: RecordingHookSink::attach(a2.runtime.hooks()),
            b,
            a1,
            a2,
            a2_starting,
        }
    }

    /// B files the announces of both of A's sessions within `within` of A2 beginning to
    /// start.
    async fn b_files_both_sessions(&self, within: Duration) {
        let peers = self.b.runtime.peers();
        for (node, name) in [(&self.a1, "A1"), (&self.a2, "A2")] {
            let hash = node.runtime.current_destination_hash();
            while peers.get(&hash).is_none() {
                assert!(
                    self.a2_starting.elapsed() < within,
                    "B did not file {name}'s announce within {within:?} of A2 beginning to start"
                );
                sleep(POLL).await;
            }
        }
        eprintln!(
            "B filed both sessions {:?} after A2 began to start",
            self.a2_starting.elapsed()
        );
    }

    /// With both of A's sessions up, B knocks A1 and then A2, and each knock is filed by
    /// the session it names and by no other.
    async fn knocks_land_on_the_session_they_name(&self) {
        for (target, fires, other_fires, name) in [
            (&self.a1, &self.a1_fires, &self.a2_fires, "A1"),
            (&self.a2, &self.a2_fires, &self.a1_fires, "A2"),
        ] {
            let filed = knocks_received(fires);
            let filed_elsewhere = knocks_received(other_fires);
            let desc = self
                .b
                .runtime
                .resolve_destination(&target.runtime.current_destination_hash())
                .await
                .unwrap();
            let outcome = self
                .b
                .runtime
                .knock(
                    crate::mesh::message::Route::unasked(desc),
                    &KnockIntro::new(&format!("hi {name} from B")).unwrap(),
                )
                .await;
            assert!(
                matches!(
                    outcome,
                    Ok(KnockOutcome {
                        via: KnockVia::Direct,
                        fallback: None,
                    })
                ),
                "B's knock on {name} did not land directly: {outcome:?}"
            );
            wait_until(&format!("{name} to file B's knock"), || {
                knocks_received(fires) == filed + 1
            })
            .await;
            assert_eq!(
                knocks_received(other_fires),
                filed_elsewhere,
                "B's knock on {name} was filed by the other session"
            );
        }
    }

    async fn stop(self) {
        self.a1.stop().await;
        self.a2.stop().await;
        self.b.stop().await;
    }
}

fn knocks_received(fires: &RecordingHookSink) -> usize {
    fires
        .snapshot()
        .iter()
        .filter(|(event, _)| *event == HookEvent::MeshKnockReceived)
        .count()
}

/// Two sessions of one user attached to one rnsd as `local` clients, with announce
/// ingress control on as it runs outside tests and both started together: a third client
/// files both sessions' announces at once although they went out close together, and a
/// link to either session reaches that session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn two_sessions_of_one_user_on_one_rnsd() {
    let Some(mut rnsd) = Rnsd::spawn("interop-rnsd-one-user").await else {
        return;
    };
    let config = MeshConfig {
        interfaces: vec![rnsd.entry()],
        ..MeshConfig::default()
    };
    let sessions = OneUserTwoSessions::start(
        "interop-rnsd-one-user",
        &config,
        AnnounceIngress::Limited,
        SecondSession::Together,
    )
    .await;

    sessions
        .b_files_both_sessions(SECOND_SESSION_FILED_WITHIN)
        .await;
    sessions.knocks_land_on_the_session_they_name().await;

    sessions.stop().await;
    let status = rnsd.terminate();
    assert!(status.success(), "rnsd exited {status} on SIGTERM");
}

/// Two sessions of one user attached to one rnsd as `local` clients, with announce
/// ingress control on, the second only once rnsd holds a path to the first: a third
/// client files the second's announce at once, a link to the first still reaches the
/// first rather than the session that attached last, and a link to the second reaches
/// the second. A tunnel synthesized on each attach collided here, as both sessions carry
/// the user's transport identity, and rnsd moved the first's path onto the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn the_first_of_two_sessions_of_one_user_on_one_rnsd_is_reached_after_the_second_attached() {
    let Some(mut rnsd) = Rnsd::spawn("interop-rnsd-one-user-later").await else {
        return;
    };
    let config = MeshConfig {
        interfaces: vec![rnsd.entry()],
        ..MeshConfig::default()
    };
    let sessions = OneUserTwoSessions::start(
        "interop-rnsd-one-user-later",
        &config,
        AnnounceIngress::Limited,
        SecondSession::OnceTheFirstIsHeard,
    )
    .await;

    sessions
        .b_files_both_sessions(SECOND_SESSION_FILED_WITHIN)
        .await;
    sessions.knocks_land_on_the_session_they_name().await;

    sessions.stop().await;
    let status = rnsd.terminate();
    assert!(status.success(), "rnsd exited {status} on SIGTERM");
}

/// Two sessions of one user dialing one rnsd transport relay over TCP, as they dial a
/// team relay, the second only once the relay holds a path to the first: a link to
/// either session reaches that session, not the one that connected last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn two_sessions_of_one_user_dialing_a_transport_relay_directly_over_tcp() {
    let Some(mut rnsd) = Rnsd::spawn_relay("interop-rnsd-relay-one-user").await else {
        return;
    };
    let config = MeshConfig {
        interfaces: vec![rnsd.entry()],
        ..MeshConfig::default()
    };
    let sessions = OneUserTwoSessions::start(
        "interop-rnsd-relay-one-user",
        &config,
        AnnounceIngress::Unlimited,
        SecondSession::OnceTheFirstIsHeard,
    )
    .await;

    sessions.b_files_both_sessions(LINK_TIMEOUT).await;
    sessions.knocks_land_on_the_session_they_name().await;

    sessions.stop().await;
    let status = rnsd.terminate();
    assert!(status.success(), "rnsd exited {status} on SIGTERM");
}

/// A node attached to an rnsd that runs an AutoInterface reads that rnsd's LAN host
/// count over its RPC listener: 0, on a discovery group no other host joins. Read with a
/// wrong `rpc_key` the same rnsd is unavailable for the run and told at debug only, and
/// the silent-LAN check falls back to hosts heard rather than telling a silent LAN.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs COYOTE_MESH_INTEROP=1 and scripts/mesh-interop/setup.sh"]
async fn a_node_reads_the_lan_host_count_of_its_rnsd() {
    let (instance, entry) = Rnsd::instance();
    let mut group = [0u8; 8];
    OsRng.fill_bytes(&mut group);
    let (discovery_port, data_port) = auto_interface_ports();
    let config = format!(
        "[reticulum]\nenable_transport = No\nshare_instance = Yes\n{instance}[logging]\nloglevel = 6\n[interfaces]\n  [[Default Interface]]\n    type = AutoInterface\n    enabled = Yes\n    group_id = coyote-interop-{}\n    discovery_port = {discovery_port}\n    data_port = {data_port}\n",
        hex_lower(&group)
    );
    let Some(mut rnsd) = Rnsd::spawn_with("interop-rnsd-lan-count", &config, entry).await else {
        return;
    };
    let rnsd_dir = rnsd.config_dir.path.clone();
    let identity = rnsd_dir.join("storage").join("transport_identity");
    wait_until("rnsd to write its transport identity", || {
        identity.is_file()
    })
    .await;

    let tmp = TempDir::new("interop-rnsd-lan-count-node");
    let mut session = Session::default();
    let runtime = MeshRuntime::start(
        &MeshConfig {
            interfaces: vec![rnsd.entry()],
            ..MeshConfig::default()
        },
        true,
        &mut session,
        mesh_paths(&tmp),
        NodeOptions {
            rnsd: RnsdLocator {
                dir_override: Some(rnsd_dir.clone()),
            },
            ..NodeOptions::default()
        },
    )
    .await
    .unwrap();
    let slot = Arc::new(MeshSlot::default());
    slot.install(runtime.clone()).unwrap();
    // rnsd binds its RPC listener a few seconds after the shared instance answers.
    let deadline = Instant::now() + LINK_TIMEOUT;
    loop {
        let readings = runtime.lan_readings(ReadEffort::Patient).await;
        if readings == [LanReading::Count(0)] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rnsd's AutoInterface did not read 0 hosts within {LINK_TIMEOUT:?}: {readings:?}"
        );
        sleep(Duration::from_millis(500)).await;
    }

    let wrong_key_dir = TempDir::new("interop-rnsd-lan-count-wrong-key");
    let wrong_key = format!("[reticulum]\nrpc_key = {}\n", "ab".repeat(32));
    fs::write(
        wrong_key_dir.path.join("config"),
        config.replacen("[reticulum]\n", &wrong_key, 1),
    )
    .unwrap();
    let MeshInterface::Local { instance, .. } = rnsd.entry() else {
        unreachable!("a shared instance is joined with a local entry")
    };
    let reader = RnsdLanReader::new(
        RnsdLocator {
            dir_override: Some(wrong_key_dir.path.clone()),
        },
        instance.as_deref().unwrap_or(DEFAULT_LOCAL_RNSD_INSTANCE),
    );
    let reading = reader.read().await;
    assert_eq!(reading, Some(LanReading::Unknown));
    assert!(reader.unavailable().await);
    let readings: Vec<LanReading> = reading.into_iter().collect();
    assert_eq!(
        lan_silent_verdict(Duration::from_secs(600), &readings, true, false),
        LanVerdict::Settle
    );
    let wrong_key_path = wrong_key_dir.path.display().to_string();
    let told = |lines: Vec<String>| {
        lines
            .iter()
            .filter(|line| line.contains("interface stats") && line.contains(&wrong_key_path))
            .count()
    };
    assert_eq!(told(crate::testing::debug_snapshot()), 1);
    assert_eq!(told(crate::testing::warn_snapshot()), 0);

    assert!(slot.stop().await.unwrap());
    let status = rnsd.terminate();
    assert!(status.success(), "rnsd exited {status} on SIGTERM");
}
