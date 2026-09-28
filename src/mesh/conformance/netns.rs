//! Two of this crate's nodes reaching each other through a relay, each in its own Linux
//! network namespace: the two-host production topology on one machine.
//!
//! Topology (`scripts/mesh-netns/setup.sh`): `coyote-relay` holds a bridge at 10.77.0.1/24;
//! `coyote-a` (10.77.0.2) and `coyote-b` (10.77.0.3) each hang off it over a veth pair whose
//! bridge port is isolated. An isolated port forwards to the bridge itself and never to
//! another isolated port, so both nodes reach the relay's TCP port and a connection from one
//! node to the other fails (its ARP request is never forwarded); the kernel guarantees the
//! only path between them is the relay. The reachability test asserts that failure before
//! it asserts anything the relay carries, and each node child holds a throwaway listener on
//! the relay port so the connect would succeed, rather than be refused, were the ports not
//! isolated: the assertion wants a timeout or no route, never `ConnectionRefused`.
//!
//! ```text
//!     coyote-a [10.77.0.2] --veth--+
//!                                  |  coyote-br0, isolated ports
//!                                  +--  10.77.0.1:4242  rnsd  (coyote-relay)
//!     coyote-b [10.77.0.3] --veth--+
//! ```
//!
//! Process model: the test process stays in the root namespace and supervises three children.
//! The relay is the pinned Python `rnsd` with transport enabled, since a `MeshConfig` can only
//! dial TCP, never listen. Each node is this test binary re-invoked on `netns_child`, which
//! starts a `MeshRuntime` there and answers JSON-line commands on stdin. Children rather than
//! threads because a network namespace is a property of a task: the runtime binds sockets
//! from whichever worker thread runs it, and `setns` would move one thread only. Every child
//! is entered with `sudo -n ip netns exec <ns>` and dropped back to the caller's uid, gid and
//! supplementary groups with `setpriv --init-groups` before the program starts, so nothing
//! of ours runs as root and the parent can signal the program directly; the `sudo` handle it
//! holds belongs to root. `ip netns exec`, `setpriv` and `env` all exec into the next
//! program, so a leading `sh -c 'echo "PID $$"; exec "$@"'` tells the parent the pid the
//! program keeps.
//!
//! Linux only: `cfg(target_os = "linux")`, narrower than `interop`'s `cfg(unix)`, because
//! network namespaces are a Linux kernel feature. As with `interop`, this says nothing about
//! the mesh itself; product code under `src/mesh/` is never cfg-gated.
//!
//! Privileges: creating a namespace needs root (`CAP_NET_ADMIN`, and `CAP_SYS_ADMIN` for the
//! namespace mount), so the scripts run under `sudo`; the tests run unprivileged and reach
//! the namespaces through passwordless `sudo -n`, which is the primary mode and CI's. A test
//! process that is itself root (a root-only container) enters them directly, without `sudo`
//! or `setpriv`, and runs the relay and the nodes as root. Gated on `COYOTE_MESH_INTEROP`
//! like `interop`. A host that cannot create a namespace at all is the one case that skips,
//! with a line naming `scripts/mesh-netns/setup.sh`; once it can, a missing namespace or
//! reference clone is a panic naming the script to run, so CI cannot pass by skipping.
//!
//! "Reach each other" is asserted to mean: each node files the other's announce with the
//! identity and display name the other reported at start; a `/status` request from an
//! identity the callee knows but has not trusted is refused with `NoAccess` (an unknown
//! identity would hear silence, as `interop` notes); and once each trusts the other, a
//! `/status` request returns the other's card with its display name and published state.
//! Every byte of that crosses the relay. A second test starts two `lan` nodes in one
//! namespace and asserts the second refuses to bind with the message the product prints,
//! without touching the host's discovery port.

use super::interop::{interop_enabled, off_switch_skip_line, require_reference};
use crate::config::Session;
use crate::config::mesh_config::{MeshConfig, MeshInterface};
use crate::mesh::card::STATE_IDLE;
use crate::mesh::node::{MeshRuntime, MeshSlot, NodeOptions};
use crate::mesh::protocol::MESH_PROTOCOL_VERSION;
use crate::mesh::test_support::{
    TempDir, TrustList, disable_ingress_control, mesh_paths, snapshot_fixture, started_runtime,
};
use crate::mesh::trust::TrustOptions;

use rns_transport::hash::AddressHash;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{self, ChildStdin, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs};
use tokio::time::sleep;

const SETUP_SH: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-netns/setup.sh"
));
const SETUP_SH_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/mesh-netns/setup.sh");
const TEARDOWN_SH: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-netns/teardown.sh"
));
const HARNESS_README: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/mesh-netns/README.md"
));

/// What `setup.sh` builds; the pin test holds the script to these.
const RELAY_NS: &str = "coyote-relay";
const NODE_A_NS: &str = "coyote-a";
const NODE_B_NS: &str = "coyote-b";
const RELAY_IP: &str = "10.77.0.1";
const RELAY_PORT: u16 = 4242;
const NODE_A_IP: &str = "10.77.0.2";
const NODE_B_IP: &str = "10.77.0.3";
/// The throwaway namespace the privilege probe creates; `teardown.sh` sweeps stragglers.
const PROBE_NS_PREFIX: &str = "coyote-probe-";

/// Selects the child mode: `node` or `lan-collision`.
const CHILD_MODE: &str = "COYOTE_MESH_NETNS_CHILD";
const CHILD_RELAY: &str = "COYOTE_MESH_NETNS_RELAY";
const CHILD_NAME: &str = "COYOTE_MESH_NETNS_NAME";
/// An identity the node starts knowing without trusting any of its destinations.
const CHILD_KNOWN_IDENTITY: &str = "COYOTE_MESH_NETNS_KNOWN_IDENTITY";
const CHILD_TEST: &str = "mesh::conformance::netns::netns_child";
const DEBUG: &str = "COYOTE_MESH_INTEROP_DEBUG";

/// The relay importing Reticulum and a node dialing it until it listens.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// One dial of the relay from a node child; its retry loop runs to `START_TIMEOUT` and then
/// waits out one more of these before it reports.
const CHILD_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The other node's announce, relayed live or answered to a path request.
const PEER_TIMEOUT: Duration = Duration::from_secs(30);
/// A path request repeated while the announce has not arrived.
const PATH_REQUEST_INTERVAL: Duration = Duration::from_secs(3);
/// One `/status` round trip under `RequestOptions::default()`: the link, then the request.
const STATUS_TIMEOUT: Duration = Duration::from_secs(45);
/// A TCP connect the bridge is meant to drop. The kernel keeps soliciting the unanswered
/// ARP for about 3 s, longer than this, so the usual outcome is `TimedOut`;
/// `HostUnreachable` is the neighbour lookup giving up first.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const EXIT_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// How the test process reaches a namespace: as root directly, else through `sudo -n`.
/// Through `sudo`, children drop back to the caller's `uid`/`gid` with `setpriv` once
/// inside; a root test process runs them as root.
struct Privilege {
    sudo: bool,
    uid: String,
    gid: String,
}

impl Privilege {
    /// `None`, with the skip line printed, when this host cannot create a namespace.
    fn probe() -> Option<Self> {
        // Tests in this process probe concurrently, so the pid alone does not name the
        // namespace uniquely.
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let uid = id("-u");
        let privilege = Self {
            sudo: uid != "0",
            uid,
            gid: id("-g"),
        };
        let probe = format!(
            "{PROBE_NS_PREFIX}{}-{}",
            process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let attempt = privilege.ip().args(["netns", "add", &probe]).output();
        let command = if privilege.sudo {
            "sudo -n ip netns add"
        } else {
            "ip netns add"
        };
        match Self::probe_outcome(command, attempt) {
            Ok(()) => {
                let _ = privilege.ip().args(["netns", "del", &probe]).status();
                Some(privilege)
            }
            Err(skip_line) => {
                eprintln!("{skip_line}");
                None
            }
        }
    }

    /// `Err` with the skip line when `attempt`, the output of `command` creating the probe
    /// namespace, did not succeed.
    fn probe_outcome(command: &str, attempt: io::Result<Output>) -> Result<(), String> {
        let failure = match attempt {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => match String::from_utf8_lossy(&out.stderr).trim() {
                "" => out.status.to_string(),
                stderr => stderr.to_string(),
            },
            Err(err) => err.to_string(),
        };
        Err(format!(
            "skipping: could not create a network namespace ({command} failed: {failure}); this needs root (CAP_NET_ADMIN and CAP_SYS_ADMIN) -- run sudo scripts/mesh-netns/setup.sh on a host where that works"
        ))
    }

    /// `program` as root: through `sudo -n` unless this process already is.
    fn as_root(&self, program: &str) -> Command {
        if self.sudo {
            let mut command = Command::new("sudo");
            command.args(["-n", program]);
            command
        } else {
            Command::new(program)
        }
    }

    fn ip(&self) -> Command {
        self.as_root("ip")
    }

    fn namespaces(&self) -> Vec<String> {
        let out = self.ip().args(["netns", "list"]).output().expect("ip runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_string))
            .collect()
    }

    /// `program` inside `ns` as this user with `envs` set, its stdout opening with
    /// `PID <pid>` of the program itself.
    fn enter(&self, ns: &str, envs: &[(&str, String)], program: &[String]) -> Command {
        let mut command = self.ip();
        command.args(["netns", "exec", ns]);
        if self.uid != "0" {
            command.args([
                "setpriv",
                &format!("--reuid={}", self.uid),
                &format!("--regid={}", self.gid),
                "--init-groups",
            ]);
        }
        command.arg("env");
        command.arg(format!("HOME={}", env::var("HOME").expect("HOME is set")));
        for (key, value) in envs {
            command.arg(format!("{key}={value}"));
        }
        command.args(["sh", "-c", "echo \"PID $$\"; exec \"$@\"", "sh"]);
        command.args(program);
        command
    }
}

fn id(flag: &str) -> String {
    let out = Command::new("id").arg(flag).output().expect("id runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn signal(pid: u32, name: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status();
}

/// One child in a namespace: the `sudo` (or `ip`) handle and, once its `PID` line has
/// arrived, the pid of the program it ends up running, which is what the parent signals.
/// Dropping it terminates the program.
struct Supervised {
    label: String,
    handle: process::Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    pid: Option<u32>,
}

impl Supervised {
    async fn spawn(
        privilege: &Privilege,
        label: &str,
        ns: &str,
        envs: &[(&str, String)],
        program: &[String],
    ) -> Self {
        let mut envs = envs.to_vec();
        if env::var_os(DEBUG).is_some() {
            envs.push((DEBUG, "1".to_string()));
        }
        let mut handle = privilege
            .enter(ns, &envs, program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {label} in {ns}: {err}"));
        let stdout = handle.stdout.take().unwrap();
        let stdin = handle.stdin.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut child = Self {
            label: label.to_string(),
            handle,
            stdin,
            lines,
            pid: None,
        };
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            let line = child.next_line(deadline).await;
            if let Some(pid) = line.strip_prefix("PID ") {
                child.pid = Some(pid.trim().parse().unwrap_or_else(|err| {
                    panic!("{label} reported an unparseable pid {line:?}: {err}")
                }));
                return child;
            }
        }
    }

    async fn next_line(&mut self, deadline: Instant) -> String {
        loop {
            match self.lines.try_recv() {
                Ok(line) => return line,
                Err(TryRecvError::Disconnected) => self.exited(),
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting on {}",
                        self.label
                    );
                    sleep(POLL).await;
                }
            }
        }
    }

    fn exited(&mut self) -> ! {
        self.exited_within(EXIT_GRACE);
        panic!(
            "{} exited: {:?}; run sudo scripts/mesh-netns/setup.sh and scripts/mesh-interop/setup.sh (its stderr above has the reason; setpriv, env and sh must be on sudo's secure_path)",
            self.label,
            self.handle.try_wait()
        )
    }

    /// The next `READY`, `OK` or `ERR` line as (prefix, JSON); everything else on the
    /// child's stdout is libtest's chatter.
    async fn next_message(&mut self, deadline: Instant) -> (String, Value) {
        loop {
            let line = self.next_line(deadline).await;
            if let Some((prefix, value)) = parse_message(&line) {
                return (prefix.to_string(), value);
            }
        }
    }

    /// Runs one command and returns `OK`'s body as `Ok` and `ERR`'s as `Err`.
    async fn command(&mut self, command: Value, timeout: Duration) -> Result<Value, Value> {
        self.write_line(&command);
        let (prefix, body) = self.next_message(Instant::now() + timeout).await;
        match prefix.as_str() {
            "OK" => Ok(body),
            "ERR" => Err(body),
            other => panic!("{} answered {command} with {other} {body}", self.label),
        }
    }

    /// A dead child shows up here as EPIPE before its stdout closes.
    fn write_line(&mut self, line: &Value) {
        if writeln!(self.stdin, "{line}")
            .and_then(|()| self.stdin.flush())
            .is_err()
        {
            self.exited();
        }
    }

    /// Whether the program has exited within `timeout`.
    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(self.handle.try_wait(), Ok(Some(_))) {
                return true;
            }
            thread::sleep(POLL);
        }
        false
    }

    fn terminate(&mut self) {
        if matches!(self.handle.try_wait(), Ok(Some(_))) {
            return;
        }
        match self.pid {
            Some(pid) => {
                signal(pid, "TERM");
                if !self.exited_within(EXIT_GRACE) {
                    signal(pid, "KILL");
                    if !self.exited_within(EXIT_GRACE) {
                        eprintln!(
                            "{} (pid {pid}) survived TERM and KILL; sudo scripts/mesh-netns/teardown.sh sweeps it",
                            self.label
                        );
                        return;
                    }
                }
            }
            // Before the `PID` line: the handle is `sudo`, which keeps the caller's real
            // uid and relays TERM to its command.
            None => {
                signal(self.handle.id(), "TERM");
                if self.exited_within(EXIT_GRACE) {
                    return;
                }
                let _ = self.handle.kill();
                if !self.exited_within(EXIT_GRACE) {
                    eprintln!(
                        "{} (pid {}) survived TERM and KILL; sudo scripts/mesh-netns/teardown.sh sweeps it",
                        self.label,
                        self.handle.id()
                    );
                    return;
                }
            }
        }
        let _ = self.handle.wait();
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// `(prefix, body)` of a `READY`, `OK` or `ERR` line; `None` for any other line, including
/// one where the prefix does not start the line.
fn parse_message(line: &str) -> Option<(&'static str, Value)> {
    ["READY", "OK", "ERR"].into_iter().find_map(|prefix| {
        let body = line.strip_prefix(prefix)?.strip_prefix(' ')?;
        let value = serde_json::from_str(body)
            .unwrap_or_else(|err| panic!("a child wrote {line:?}: {err}"));
        Some((prefix, value))
    })
}

fn test_binary() -> Vec<String> {
    vec![
        env::current_exe().unwrap().display().to_string(),
        "--exact".to_string(),
        CHILD_TEST.to_string(),
        "--test-threads=2".to_string(),
        "--nocapture".to_string(),
    ]
}

/// A node child: this binary in `node` mode inside one namespace, joined to the relay.
struct Node {
    name: &'static str,
    child: Supervised,
    identity_hash: String,
    destination_hash: String,
}

impl Node {
    async fn start(
        privilege: &Privilege,
        ns: &str,
        name: &'static str,
        known_identity: Option<&str>,
    ) -> Self {
        let mut envs = vec![
            (CHILD_MODE, "node".to_string()),
            (CHILD_RELAY, format!("{RELAY_IP}:{RELAY_PORT}")),
            (CHILD_NAME, name.to_string()),
        ];
        if let Some(identity) = known_identity {
            envs.push((CHILD_KNOWN_IDENTITY, identity.to_string()));
        }
        let mut child = Supervised::spawn(privilege, name, ns, &envs, &test_binary()).await;
        let (prefix, ready) = child
            .next_message(Instant::now() + START_TIMEOUT + CHILD_CONNECT_TIMEOUT + POLL * 20)
            .await;
        assert_eq!(prefix, "READY", "{name} did not start: {ready}");
        let text = |key: &str| ready[key].as_str().unwrap().to_string();
        Self {
            name,
            identity_hash: text("identity_hash"),
            destination_hash: text("destination_hash"),
            child,
        }
    }

    /// Waits until this node has filed `other`'s announce and returns the record.
    async fn wait_peer(&mut self, other: &Node) -> Value {
        self.child
            .command(
                json!({ "cmd": "wait_peer", "destination": other.destination_hash }),
                PEER_TIMEOUT + POLL * 20,
            )
            .await
            .unwrap_or_else(|err| panic!("{} never filed {}: {err}", self.name, other.name))
    }

    async fn trust(&mut self, other: &Node) {
        self.child
            .command(
                json!({ "cmd": "trust", "destination": other.destination_hash }),
                STATUS_TIMEOUT,
            )
            .await
            .unwrap_or_else(|err| panic!("{} could not trust {}: {err}", self.name, other.name));
    }

    /// `other`'s card, or the `StatusError` as the child printed it.
    async fn status(&mut self, other: &Node) -> Result<Value, Value> {
        self.child
            .command(
                json!({ "cmd": "status", "destination": other.destination_hash }),
                STATUS_TIMEOUT,
            )
            .await
    }

    /// The outcome of a plain TCP connect from this node's namespace to `addr`.
    async fn connect(&mut self, addr: &str) -> Value {
        self.child
            .command(
                json!({ "cmd": "connect", "addr": addr }),
                CONNECT_TIMEOUT * 2,
            )
            .await
            .unwrap_or_else(|err| panic!("{} could not attempt {addr}: {err}", self.name))
    }

    async fn quit(&mut self) {
        self.child.write_line(&json!({ "cmd": "quit" }));
        assert!(
            self.child.exited_within(EXIT_GRACE * 2),
            "{} did not exit on quit",
            self.name
        );
    }
}

/// The relay and both nodes, torn down together. Fields drop in order: the nodes go before
/// the relay they are dialing.
struct Harness {
    a: Node,
    b: Node,
    relay: Supervised,
    _tmp: TempDir,
}

impl Harness {
    async fn start(privilege: &Privilege) -> Self {
        let (dir, python) = require_reference();
        let tmp = TempDir::new("netns-relay");
        let relay_dir = tmp.path.join("relay");
        fs::create_dir_all(&relay_dir).unwrap();
        fs::write(relay_dir.join("config"), relay_config()).unwrap();
        let envs = [
            ("PYTHONPATH", dir.join("reticulum").display().to_string()),
            ("PYTHONUNBUFFERED", "1".to_string()),
        ];
        let program = [
            python.display().to_string(),
            "-m".to_string(),
            "RNS.Utilities.rnsd".to_string(),
            "--config".to_string(),
            relay_dir.display().to_string(),
            "-v".to_string(),
        ];
        let relay = Supervised::spawn(privilege, "rnsd", RELAY_NS, &envs, &program).await;
        let a = Node::start(privilege, NODE_A_NS, "Alpha", None).await;
        // Bravo knows Alpha's identity without trusting it: the default-closed verdict,
        // which knocks and refuses, where an unknown identity would hear silence.
        let b = Node::start(privilege, NODE_B_NS, "Bravo", Some(&a.identity_hash)).await;
        Self {
            a,
            b,
            relay,
            _tmp: tmp,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if env::var_os(DEBUG).is_none() {
            return;
        }
        for line in self.relay.lines.try_iter() {
            eprintln!("[rnsd] {line}");
        }
    }
}

/// The relay's Reticulum config: a transport node listening on the bridge address.
/// `ingress_control = No` mirrors the `ingress_control: Some(false)` the nodes set, so a
/// fresh peer's announce is never held for 360 s on the relay's side either.
fn relay_config() -> String {
    format!(
        "[reticulum]\n  enable_transport = Yes\n  share_instance = No\n  respond_to_probes = No\n\n[interfaces]\n  [[Relay]]\n    type = TCPServerInterface\n    enabled = Yes\n    listen_ip = {RELAY_IP}\n    listen_port = {RELAY_PORT}\n    ingress_control = No\n"
    )
}

/// `None`, with the reason printed, when the suite is off or this host cannot create a
/// namespace; every other missing prerequisite panics naming the script to run.
fn gate() -> Option<Privilege> {
    if !interop_enabled() {
        eprintln!(
            "{}",
            off_switch_skip_line(
                "scripts/mesh-interop/setup.sh and sudo scripts/mesh-netns/setup.sh"
            )
        );
        return None;
    }
    let privilege = Privilege::probe()?;
    let present = privilege.namespaces();
    for ns in [RELAY_NS, NODE_A_NS, NODE_B_NS] {
        assert!(
            present.iter().any(|name| name == ns),
            "network namespace {ns} is missing; run sudo scripts/mesh-netns/setup.sh"
        );
    }
    Some(privilege)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CAP_NET_ADMIN (scripts/mesh-netns/setup.sh) and COYOTE_MESH_INTEROP=1"]
async fn two_nodes_in_separate_namespaces_reach_each_other_through_the_relay() {
    let Some(privilege) = gate() else {
        return;
    };
    let started = Instant::now();
    let mut harness = Harness::start(&privilege).await;
    let Harness { a, b, .. } = &mut harness;

    let direct = a.connect(&format!("{NODE_B_IP}:{RELAY_PORT}")).await;
    assert_eq!(direct["connected"], json!(false), "{direct}");
    assert!(
        matches!(
            direct["kind"].as_str(),
            Some("TimedOut" | "HostUnreachable")
        ),
        "the bridge should drop the connect, not refuse it: {direct}"
    );

    let filed = a.wait_peer(b).await;
    assert_eq!(filed["identity_hash"], json!(b.identity_hash), "{filed}");
    assert_eq!(filed["display_name"], json!(b.name), "{filed}");
    assert_eq!(
        filed["protocol_version"],
        json!(MESH_PROTOCOL_VERSION),
        "{filed}"
    );
    let filed = b.wait_peer(a).await;
    assert_eq!(filed["identity_hash"], json!(a.identity_hash), "{filed}");
    assert_eq!(filed["display_name"], json!(a.name), "{filed}");
    assert_eq!(
        filed["protocol_version"],
        json!(MESH_PROTOCOL_VERSION),
        "{filed}"
    );

    let refused = a
        .status(b)
        .await
        .expect_err("a known but untrusted caller hears a refusal, not a card");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("NoAccess")),
        "{refused}"
    );

    a.trust(b).await;
    b.trust(a).await;
    assert_reads_card(a, b).await;
    assert_reads_card(b, a).await;

    a.quit().await;
    b.quit().await;
    drop(harness);
    eprintln!("netns reachability: {:.1?} wall-clock", started.elapsed());
}

/// `from`, trusting `to`, reads the card `to` serves: the name and the published state.
async fn assert_reads_card(from: &mut Node, to: &Node) {
    let card = from
        .status(to)
        .await
        .unwrap_or_else(|err| panic!("{} could not read {}'s card: {err}", from.name, to.name));
    assert_eq!(card["display_name"], json!(to.name), "{card}");
    assert_eq!(card["state_code"], json!(STATE_IDLE), "{card}");
    assert_eq!(
        card["objective"],
        json!(snapshot_fixture().objective),
        "{card}"
    );
    assert!(
        card["served_at_secs"]
            .as_u64()
            .is_some_and(|secs| secs > 1_700_000_000),
        "{card}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CAP_NET_ADMIN (scripts/mesh-netns/setup.sh) and COYOTE_MESH_INTEROP=1"]
async fn two_lan_nodes_in_one_namespace_cannot_both_bind() {
    let Some(privilege) = gate() else {
        return;
    };
    let started = Instant::now();
    let envs = [(CHILD_MODE, "lan-collision".to_string())];
    let mut child = Supervised::spawn(
        &privilege,
        "lan-collision",
        NODE_A_NS,
        &envs,
        &test_binary(),
    )
    .await;
    let (prefix, reply) = child.next_message(Instant::now() + START_TIMEOUT).await;
    assert_eq!(prefix, "OK", "{reply}");
    let refusal = reply["refusal"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"));
    assert!(
        refusal.starts_with("Failed to bind the mesh lan interface"),
        "{refusal}"
    );
    assert!(
        child.exited_within(EXIT_GRACE * 2),
        "the child did not exit"
    );
    eprintln!("netns lan collision: {:.1?} wall-clock", started.elapsed());
}

/// `setup.sh` over namespaces that already exist: exit 0, no `creating` line, the relay
/// address on stdout, the same namespaces after as before. `teardown.sh` is never run from
/// a test: the tests in this binary run in parallel, and deleting the namespaces would pull
/// them from under the reachability test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CAP_NET_ADMIN (scripts/mesh-netns/setup.sh) and COYOTE_MESH_INTEROP=1"]
async fn setup_sh_reruns_idempotently_on_existing_namespaces() {
    let Some(privilege) = gate() else {
        return;
    };
    // The other tests' probes create and delete probe namespaces concurrently.
    let durable = |names: Vec<String>| -> BTreeSet<String> {
        names
            .into_iter()
            .filter(|name| !name.starts_with(PROBE_NS_PREFIX))
            .collect()
    };
    let before = durable(privilege.namespaces());
    let out = privilege
        .as_root(SETUP_SH_PATH)
        .output()
        .expect("setup.sh runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{}\n{stdout}\n{stderr}", out.status);
    assert!(
        !stderr.lines().any(|line| line.contains("creating")),
        "a rerun created something:\n{stderr}"
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            format!("RELAY_IP={RELAY_IP}"),
            format!("RELAY_PORT={RELAY_PORT}")
        ],
        "{stderr}"
    );
    assert_eq!(durable(privilege.namespaces()), before);
}

#[test]
fn a_failed_probe_skips_with_a_line_naming_the_privilege_and_the_script() {
    let failed = Command::new("false").output();
    let unspawnable = Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no sudo on this host",
    ));
    for (command, attempt, failure) in [
        ("sudo -n ip netns add", failed, "exit status: 1"),
        ("ip netns add", unspawnable, "no sudo on this host"),
    ] {
        let line = Privilege::probe_outcome(command, attempt).expect_err("the probe failed");
        assert!(line.starts_with("skipping:"), "{line}");
        assert!(line.contains("CAP_NET_ADMIN"), "{line}");
        assert!(line.contains("scripts/mesh-netns/setup.sh"), "{line}");
        assert!(
            line.contains(&format!("({command} failed: {failure})")),
            "{line}"
        );
    }
}

#[test]
fn a_protocol_line_is_recognised_only_at_column_zero() {
    assert_eq!(parse_message("READY {}"), Some(("READY", json!({}))));
    assert_eq!(
        parse_message("OK {\"a\":1}"),
        Some(("OK", json!({ "a": 1 })))
    );
    assert_eq!(parse_message("ERR {}"), Some(("ERR", json!({}))));
    assert_eq!(parse_message("running 1 test"), None);
    // Serial libtest prints `test <name> ... ` without a newline before the body runs;
    // `emit`'s leading newline is what keeps the message off that line.
    assert_eq!(parse_message("test x ... READY {}"), None);
}

#[test]
fn the_scripts_and_the_test_agree_on_the_namespaces_and_the_relay_address() {
    let port = RELAY_PORT.to_string();
    for (name, value) in [
        ("RELAY_NS", RELAY_NS),
        ("NODE_A_NS", NODE_A_NS),
        ("NODE_B_NS", NODE_B_NS),
        ("RELAY_IP", RELAY_IP),
        ("RELAY_PORT", port.as_str()),
        ("NODE_A_IP", NODE_A_IP),
        ("NODE_B_IP", NODE_B_IP),
    ] {
        let assignment = format!("{name}=\"{value}\"");
        assert!(
            SETUP_SH.lines().any(|line| line == assignment),
            "scripts/mesh-netns/setup.sh does not set {assignment}"
        );
    }
    for ns in [RELAY_NS, NODE_A_NS, NODE_B_NS] {
        assert!(
            TEARDOWN_SH
                .lines()
                .any(|line| line.ends_with(&format!("=\"{ns}\""))),
            "scripts/mesh-netns/teardown.sh does not name {ns}"
        );
        assert!(
            HARNESS_README.contains(ns),
            "scripts/mesh-netns/README.md does not name {ns}"
        );
    }
    assert!(
        TEARDOWN_SH.contains(PROBE_NS_PREFIX),
        "scripts/mesh-netns/teardown.sh does not sweep {PROBE_NS_PREFIX}* namespaces"
    );
    assert!(
        SETUP_SH.contains("log \"creating"),
        "the idempotency test greps setup.sh's stderr for `creating`"
    );
    for needle in [
        "CAP_NET_ADMIN",
        "sudo scripts/mesh-netns/setup.sh",
        "sudo scripts/mesh-netns/teardown.sh",
        "COYOTE_MESH_INTEROP=1 cargo test --all mesh::conformance -- --include-ignored",
        RELAY_IP,
        NODE_A_IP,
        NODE_B_IP,
    ] {
        assert!(
            HARNESS_README.contains(needle),
            "scripts/mesh-netns/README.md does not mention {needle:?}"
        );
    }
}

/// The child entry point: nothing unless `COYOTE_MESH_NETNS_CHILD` selects a mode, so the
/// default suite runs it as an empty test and the parent reaches it with `--exact`.
#[test]
fn netns_child() {
    let Some(mode) = env::var_os(CHILD_MODE) else {
        return;
    };
    crate::testing::install_log_collector();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    match mode.to_str() {
        Some("node") => runtime.block_on(serve_node()),
        Some("lan-collision") => runtime.block_on(lan_collision()),
        _ => panic!("unknown {CHILD_MODE} {mode:?}"),
    }
    if env::var_os(DEBUG).is_some() {
        for line in crate::testing::debug_snapshot() {
            eprintln!("[{mode:?}] DEBUG {line}");
        }
        for line in crate::testing::warn_snapshot() {
            eprintln!("[{mode:?}] WARN {line}");
        }
    }
}

fn emit(prefix: &str, body: Value) {
    // On one CPU libtest runs serially and has already printed `test <name> ... ` with no
    // newline; the parent only recognises a prefix at column 0.
    println!("\n{prefix} {body}");
}

/// Stdin, one line at a time, `None` once it closes.
fn stdin_lines() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

async fn next_command(lines: &Receiver<String>) -> Option<Value> {
    loop {
        match lines.try_recv() {
            Ok(line) => {
                return Some(
                    serde_json::from_str(&line)
                        .unwrap_or_else(|err| panic!("the parent wrote {line:?}: {err}")),
                );
            }
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => sleep(POLL).await,
        }
    }
}

/// `node` mode: a node dialing the relay, then commands until `quit` or stdin closes.
async fn serve_node() {
    let relay = env::var(CHILD_RELAY).unwrap();
    let (host, port) = relay.rsplit_once(':').unwrap();
    let name = env::var(CHILD_NAME).unwrap();
    let config = MeshConfig {
        interfaces: vec![MeshInterface::Private {
            host: host.to_string(),
            port: port.parse().unwrap(),
        }],
        display_name: Some(name.clone()),
        ..MeshConfig::default()
    };
    let tmp = TempDir::new(&format!("netns-{name}"));
    let paths = mesh_paths(&tmp);
    if let Ok(identity) = env::var(CHILD_KNOWN_IDENTITY) {
        TrustList::default()
            .identity(&identity, false)
            .write(&paths.config_dir);
    }
    let mut session = Session::default();
    // The relay was spawned moments before this node and is still importing Reticulum, so
    // a refused connection is retried until it listens.
    let deadline = Instant::now() + START_TIMEOUT;
    let mut attempts = 0u32;
    let mut first_error = None;
    let runtime = loop {
        attempts += 1;
        let options = NodeOptions {
            connect_timeout: CHILD_CONNECT_TIMEOUT,
            ..NodeOptions::default()
        };
        match MeshRuntime::start(&config, true, &mut session, mesh_paths(&tmp), options).await {
            Ok(runtime) => break runtime,
            Err(err) => {
                let error = format!("{err:#}");
                if first_error.is_none() {
                    first_error = Some(error.clone());
                }
                if Instant::now() < deadline {
                    sleep(POLL * 10).await;
                    continue;
                }
                emit(
                    "ERR",
                    json!({
                        "first_error": first_error,
                        "last_error": error,
                        "attempts": attempts,
                    }),
                );
                return;
            }
        }
    };
    disable_ingress_control(&runtime).await;
    let slot = Arc::new(MeshSlot::default());
    slot.install(runtime.clone()).unwrap();
    slot.publish(snapshot_fixture());
    // Nothing accepts on it; it exists so a connect from the other node would succeed,
    // rather than be refused, if the bridge ports were not isolated.
    let _probe_listener = TcpListener::bind(("0.0.0.0", port.parse::<u16>().unwrap()))
        .unwrap_or_else(|err| panic!("binding the probe listener on :{port}: {err}"));
    emit(
        "READY",
        json!({
            "identity_hash": runtime.fingerprint(),
            "destination_hash": runtime.current_destination_hash(),
            "display_name": name,
            "attempts": attempts,
            "probe_listener": true,
        }),
    );

    let lines = stdin_lines();
    while let Some(command) = next_command(&lines).await {
        let destination = command["destination"].as_str().unwrap_or_default();
        let reply = match command["cmd"].as_str() {
            Some("wait_peer") => wait_peer(&runtime, destination).await,
            Some("trust") => runtime
                .trust()
                .trust_destination(
                    slot.as_ref(),
                    destination,
                    TrustOptions::default(),
                    SystemTime::now(),
                )
                .map(|_| json!({}))
                .map_err(|err| format!("{err:#}")),
            Some("status") => status(&runtime, destination).await,
            Some("connect") => connect(command["addr"].as_str().unwrap_or_default()).await,
            Some("quit") => break,
            _ => Err(format!("unknown command {command}")),
        };
        match reply {
            Ok(body) => emit("OK", body),
            Err(error) => emit("ERR", json!({ "error": error })),
        }
    }
    assert!(slot.stop().await.unwrap());
}

async fn wait_peer(runtime: &MeshRuntime, destination: &str) -> Result<Value, String> {
    let peers = runtime.peers();
    let hash = AddressHash::new_from_hex_string(destination)
        .map_err(|err| format!("{destination} is not a destination hash: {err:?}"))?;
    let transport = runtime
        .transport_handle()
        .await
        .ok_or("the node has stopped")?;
    let deadline = Instant::now() + PEER_TIMEOUT;
    let mut next_request = Instant::now();
    loop {
        if let Some(peer) = peers.get(destination) {
            return Ok(json!({
                "identity_hash": peer.identity_hash,
                "display_name": peer.display_name,
                "protocol_version": peer.protocol_version,
                "hops": peer.hops,
            }));
        }
        if Instant::now() >= deadline {
            return Err(format!("{destination} was never filed"));
        }
        // An announce the relay heard before this node joined is not replayed on its own;
        // the relay answers a path request with the announce it cached.
        if Instant::now() >= next_request {
            transport.request_path(&hash, None, None).await;
            next_request = Instant::now() + PATH_REQUEST_INTERVAL;
        }
        sleep(POLL).await;
    }
}

async fn status(runtime: &MeshRuntime, destination: &str) -> Result<Value, String> {
    let desc = runtime
        .resolve_destination(destination)
        .await
        .ok_or_else(|| format!("{destination} is not a filed peer"))?;
    let card = runtime
        .request_status(&desc)
        .await
        .map_err(|err| format!("{err:?}"))?;
    Ok(json!({
        "display_name": card.display_name,
        "objective": card.objective,
        "state_code": card.state.code,
        "served_at_secs": card.served_at_secs,
    }))
}

async fn connect(addr: &str) -> Result<Value, String> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|err| format!("{addr} is not a socket address: {err}"))?;
    let attempt =
        tokio::task::spawn_blocking(move || TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT))
            .await
            .map_err(|err| format!("the connect task failed: {err}"))?;
    Ok(match attempt {
        Ok(_) => json!({ "connected": true, "error": null, "kind": null }),
        Err(err) => json!({
            "connected": false,
            "error": err.to_string(),
            "kind": format!("{:?}", err.kind()),
        }),
    })
}

/// `lan-collision` mode: the second `lan` node in one namespace must refuse to bind.
async fn lan_collision() {
    let config = MeshConfig {
        interfaces: vec![MeshInterface::Lan],
        ..MeshConfig::default()
    };
    let first_tmp = TempDir::new("netns-lan-first");
    let mut first_session = Session::default();
    let first = match MeshRuntime::start(
        &config,
        true,
        &mut first_session,
        mesh_paths(&first_tmp),
        NodeOptions::default(),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(err) => {
            emit(
                "ERR",
                json!({ "error": format!("the first lan node did not start: {err:#}") }),
            );
            return;
        }
    };
    let first_slot = Arc::new(MeshSlot::default());
    first_slot.install(first).unwrap();
    let second_tmp = TempDir::new("netns-lan-second");
    let mut second_session = Session::default();
    match MeshRuntime::start(
        &config,
        true,
        &mut second_session,
        mesh_paths(&second_tmp),
        NodeOptions::default(),
    )
    .await
    {
        Ok(second) => {
            let second_slot = Arc::new(MeshSlot::default());
            second_slot.install(second).unwrap();
            assert!(second_slot.stop().await.unwrap());
            emit(
                "ERR",
                json!({ "error": "the second lan node started alongside the first" }),
            );
        }
        Err(err) => emit("OK", json!({ "refusal": format!("{err:#}") })),
    }
    assert!(first_slot.stop().await.unwrap());
}

// Usage-probe additions (spec-first, always-on): the acceptance criteria as written, not
// the implementation as read.

/// Acceptance (c): the relay is "a transport-enabled `rnsd` relay with a
/// `TCPServerInterface`" that the two `private` nodes dial on the bridge address. It must
/// also never share the host's own Reticulum instance, or the namespace isolation the
/// suite asserts would be a fiction on a host running `rnsd`.
#[test]
fn usage_probe_the_relay_is_a_transport_node_with_a_tcp_server_on_the_bridge_address() {
    let config = relay_config();
    let setting = |key: &str| -> Option<String> {
        config.lines().find_map(|line| {
            let (k, v) = line.trim().split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    assert_eq!(
        setting("enable_transport").as_deref(),
        Some("Yes"),
        "{config}"
    );
    assert_eq!(setting("share_instance").as_deref(), Some("No"), "{config}");
    assert_eq!(
        setting("type").as_deref(),
        Some("TCPServerInterface"),
        "{config}"
    );
    assert_eq!(setting("enabled").as_deref(), Some("Yes"), "{config}");
    assert_eq!(setting("listen_ip").as_deref(), Some(RELAY_IP), "{config}");
    assert_eq!(
        setting("listen_port").as_deref(),
        Some(RELAY_PORT.to_string().as_str()),
        "{config}"
    );
    assert_eq!(
        config.matches("[[").count(),
        1,
        "exactly one interface, the relay's server: {config}"
    );
    // setup.sh's stdout is the address for shell scripts; it must be the address the relay
    // listens on.
    let script_says = |key: &str| {
        SETUP_SH
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .map(|value| value.trim_matches('"').to_string())
    };
    assert_eq!(script_says("RELAY_IP").as_deref(), Some(RELAY_IP));
    assert_eq!(
        script_says("RELAY_PORT").as_deref(),
        Some(RELAY_PORT.to_string().as_str())
    );
}

/// Ruling: "Test nodes set `ingress_control: Some(false)`; product defaults untouched", and
/// the helper is read-modify-write, so any other per-interface setting a caller had made
/// survives it. Proven on a live runtime: every interface ends with `Some(false)` and a
/// setting made beforehand is still there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_probe_disable_ingress_control_flips_only_ingress_control_on_every_interface() {
    let started = started_runtime("netns-probe-ingress").await;
    let runtime = started.runtime.clone();
    let transport = runtime
        .transport_handle()
        .await
        .expect("the node just started");
    let manager = transport.iface_manager();
    let hashes: Vec<_> = manager
        .lock()
        .await
        .interface_hashes()
        .into_iter()
        .collect();
    assert!(
        !hashes.is_empty(),
        "a private node has at least one interface"
    );
    {
        let mut manager = manager.lock().await;
        for iface in &hashes {
            let before = manager.shared_config(iface).cloned().unwrap_or_default();
            assert_ne!(
                before.ingress_control,
                Some(false),
                "the product default must not already be the test-only setting"
            );
            let mut marked = before;
            marked.announce_rate_target = Some(7);
            manager.set_shared_config(*iface, marked);
        }
    }

    disable_ingress_control(&runtime).await;

    let manager = manager.lock().await;
    for iface in &hashes {
        let after = manager
            .shared_config(iface)
            .unwrap_or_else(|| panic!("interface {iface} lost its shared config"));
        assert_eq!(after.ingress_control, Some(false), "{iface}");
        assert_eq!(
            after.announce_rate_target,
            Some(7),
            "{iface}: a setting made before the helper ran was clobbered"
        );
    }
    drop(manager);
    let slot = Arc::new(MeshSlot::default());
    slot.install(runtime).unwrap();
    assert!(slot.stop().await.unwrap());
    started.relay_handle.abort();
}

/// README: "The tests are `#[ignore]`d, so a plain `cargo test` never touches a
/// namespace." Every test that reaches `gate()` (the only way into a namespace) carries
/// the ruling's verbatim ignore reason, so a new namespace-touching test cannot slip into
/// the default run.
#[test]
fn usage_probe_every_test_that_reaches_a_namespace_is_ignored_with_the_rulings_reason() {
    const REASON: &str = "#[ignore = \"needs CAP_NET_ADMIN (scripts/mesh-netns/setup.sh) and COYOTE_MESH_INTEROP=1\"]";
    let source = include_str!("netns.rs");
    let lines: Vec<&str> = source.lines().collect();
    let entries: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == "let Some(privilege) = gate() else {")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        entries.len(),
        3,
        "three namespace-touching tests are expected"
    );
    for entry in entries {
        let window = &lines[entry.saturating_sub(6)..entry];
        assert!(
            window.iter().any(|line| line.trim() == REASON),
            "the test entering a namespace at line {} lacks the ruling's ignore reason:\n{}",
            entry + 1,
            window.join("\n")
        );
    }
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.trim_start().starts_with("#[ignore"))
            .count(),
        3,
        "no other ignore form in this module"
    );
}

/// README Privileges: "the tests call `sudo -n ip netns exec`, and every child drops back to
/// the caller's uid and gid with `setpriv` before the relay or the node starts, so no relay
/// or node of ours runs as root. A test process that is itself root [...] needs no `sudo`:
/// it enters the namespaces directly and runs the relay and the nodes as root." Pinned on
/// the command each mode builds, so the root branch (the previous probe's finding) cannot
/// silently regrow a `setpriv` into a uid that has no `SUDO_UID` to drop to, and the primary
/// branch cannot lose its drop.
#[test]
fn usage_probe_children_enter_as_the_caller_under_sudo_and_as_root_without_sudo_or_setpriv() {
    let envs = [("COYOTE_MESH_NETNS_NAME", "Alpha".to_string())];
    let program = ["/opt/prog".to_string(), "--flag".to_string()];
    let args_of = |command: &Command| -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    };
    let position = |args: &[String], needle: &str| -> usize {
        args.iter()
            .position(|arg| arg == needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from {args:?}"))
    };

    // Primary mode and CI's: an unprivileged caller with passwordless sudo.
    let caller = Privilege {
        sudo: true,
        uid: "1000".to_string(),
        gid: "1000".to_string(),
    };
    let command = caller.enter(NODE_A_NS, &envs, &program);
    assert_eq!(command.get_program(), "sudo");
    let args = args_of(&command);
    assert_eq!(
        &args[..5],
        ["-n", "ip", "netns", "exec", NODE_A_NS],
        "{args:?}"
    );
    let setpriv = position(&args, "setpriv");
    let env_at = position(&args, "env");
    let prog_at = position(&args, "/opt/prog");
    assert!(
        setpriv < env_at && env_at < prog_at,
        "the drop happens inside the namespace and before the program: {args:?}"
    );
    assert_eq!(
        &args[setpriv..setpriv + 4],
        ["setpriv", "--reuid=1000", "--regid=1000", "--init-groups"],
        "uid, gid and supplementary groups all drop: {args:?}"
    );
    assert!(
        args.iter().any(|arg| arg == "COYOTE_MESH_NETNS_NAME=Alpha"),
        "{args:?}"
    );
    assert!(
        args.iter().any(|arg| arg.starts_with("HOME=")),
        "the child keeps the caller's HOME under sudo's scrubbed environment: {args:?}"
    );
    // The `sh -c` prelude announces the pid and then execs, so the pid it prints is the
    // program's own and the parent can signal it directly (README: "the parent can signal
    // the program directly").
    let prelude = &args[position(&args, "-c") + 1];
    assert!(
        prelude.contains("PID $$") && prelude.contains("exec \"$@\""),
        "{prelude:?}"
    );
    assert_eq!(&args[prog_at..], ["/opt/prog", "--flag"], "{args:?}");
    assert_eq!(
        args.iter().filter(|arg| *arg == "sudo").count(),
        0,
        "sudo is the program, never nested in the arguments: {args:?}"
    );

    // A root test process: no sudo, no drop, the namespace entered directly.
    let root = Privilege {
        sudo: false,
        uid: "0".to_string(),
        gid: "0".to_string(),
    };
    let command = root.enter(NODE_B_NS, &envs, &program);
    assert_eq!(command.get_program(), "ip");
    let args = args_of(&command);
    assert_eq!(&args[..3], ["netns", "exec", NODE_B_NS], "{args:?}");
    assert!(
        !args
            .iter()
            .any(|arg| arg == "sudo" || arg == "setpriv" || arg.starts_with("--reuid")),
        "root enters directly and stays root: {args:?}"
    );
    assert_eq!(
        &args[position(&args, "/opt/prog")..],
        ["/opt/prog", "--flag"]
    );

    // The scripts and `ip` follow the same rule.
    let command = caller.as_root(SETUP_SH_PATH);
    assert_eq!(command.get_program(), "sudo");
    assert_eq!(args_of(&command), ["-n", SETUP_SH_PATH]);
    let command = root.as_root(SETUP_SH_PATH);
    assert_eq!(command.get_program(), SETUP_SH_PATH);
    assert!(args_of(&command).is_empty());
}

/// README Privileges: "The scripts refuse to run otherwise and say to use `sudo`." Both
/// scripts, invoked by the unprivileged test process, exit 1 before touching anything, name
/// the privilege, and print the `sudo` invocation to use. Nothing appears on stdout, which
/// `setup.sh` reserves for the relay address on success. A root test process cannot prove a
/// refusal (and running the scripts would create the namespaces), so it steps aside.
#[test]
fn usage_probe_the_scripts_refuse_an_unprivileged_caller_and_say_to_use_sudo() {
    if id("-u") == "0" {
        eprintln!("skipping: the test process is root, so the scripts would not refuse it");
        return;
    }
    let teardown_sh_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-netns/teardown.sh"
    );
    for (script, verb) in [(SETUP_SH_PATH, "creating"), (teardown_sh_path, "deleting")] {
        let out = Command::new(script)
            .output()
            .unwrap_or_else(|err| panic!("{script} runs: {err}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{script}: {}\n{stdout}\n{stderr}",
            out.status
        );
        assert!(
            stdout.is_empty(),
            "{script} wrote to stdout on refusal: {stdout:?}"
        );
        let refusal = stderr.trim();
        assert_eq!(refusal.lines().count(), 1, "one refusal line: {stderr:?}");
        assert!(refusal.starts_with("[mesh-netns] ERROR: "), "{refusal}");
        assert!(
            refusal.contains(&format!("{verb} network namespaces needs root")),
            "{refusal}"
        );
        assert!(
            refusal.contains("CAP_NET_ADMIN") && refusal.contains("CAP_SYS_ADMIN"),
            "{refusal}"
        );
        assert!(
            refusal.ends_with(&format!("run: sudo {script}")),
            "the refusal ends with the command to run instead: {refusal}"
        );
    }
    // The refusal is decided before the tool checks, so a host without iproute2 still hears
    // about the privilege first, not about `ip`.
    for script in [SETUP_SH, TEARDOWN_SH] {
        let root_check = script.find("id -u").expect("the script checks its uid");
        let tool_check = script
            .find("command -v ip")
            .expect("the script checks for ip");
        assert!(
            root_check < tool_check,
            "the uid check comes before the tool checks"
        );
    }
}
