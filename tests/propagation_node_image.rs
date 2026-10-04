//! Usage coverage for the propagation-node image in `deployment/propagation-node/`.
//!
//! The image is an operator-facing artefact, so this file checks it the way an
//! operator meets it: the README's promises, then the container's behaviour.
//! Nothing here reaches into `src/`; the expectations are written from the
//! deployment README and the shipped config files, not from the entrypoint.
//!
//! Two layers:
//!
//! * The always-on tests pin the shipped text: the pip pins match what the
//!   interop harness is verified against, the shipped `lxmd.config` enables the
//!   node and requires auth, the README documents the private-network contract
//!   without citing upstream's build-only Dockerfile, the root README and the
//!   protocol spec cross-link, and every shipped file is plain ASCII with LF
//!   endings and no plan-tracker references. The `file:line` citations the README
//!   and entrypoint make into upstream are resolved against the interop harness
//!   checkout that `scripts/mesh-interop/setup.sh` makes under
//!   `${COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}` (the
//!   `mesh-interop` CI job runs them there); without it they print `skipping:`.
//! * The live tests build the image and run it. They need a Docker daemon and
//!   are `#[ignore]`d; run them with
//!
//!   ```sh
//!   COYOTE_PN_IMAGE_TESTS=1 cargo test --test propagation_node_image -- --include-ignored
//!   ```
//!
//!   Behind a TLS-intercepting proxy set `COYOTE_PN_PIP_CA=/path/to/ca-bundle.crt`;
//!   it is forwarded as the Dockerfile's optional `pip_ca` build secret, and
//!   `HTTP_PROXY`/`HTTPS_PROXY` are forwarded as build args when set. Every
//!   container and volume the tests create is named `coyote-pn-test-<pid>-...`
//!   and labelled `coyote-pn-test=<pid>`, and is removed when the test ends. The
//!   image tag `coyote-pn-test-<pid>:latest` is shared by every test in the
//!   process and is left behind; sweep it and anything a killed run leaked with
//!
//!   ```sh
//!   docker rm -f $(docker ps -aq -f label=coyote-pn-test); docker volume prune -f --filter label=coyote-pn-test; docker rmi $(docker images -q 'coyote-pn-test-*')
//!   ```

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

/// The versions `scripts/mesh-interop/README.md` says the harness is verified
/// against; the image must install exactly these.
const RNS_VERSION: &str = "1.5.2";
const LXMF_VERSION: &str = "0.9.6";

/// The warning lxmd 0.9.6 logs when `auth_required = yes` and the allowed list
/// is empty (the typo is upstream's). The README quotes it verbatim.
const EMPTY_ALLOWED_WARNING: &str = "Clint authentication was enabled, but no identity hashes could be loaded from /data/lxmd/allowed. Nobody will be able to sync messages from this propagation node.";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn deployment_dir() -> PathBuf {
    repo_root().join("deployment").join("propagation-node")
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The files the image is built from, in the order the README introduces them.
const SHIPPED_FILES: [&str; 5] = [
    "Dockerfile",
    "README.md",
    "entrypoint.sh",
    "lxmd.config",
    "reticulum.config",
];

// ---------------------------------------------------------------------------
// Always-on: the shipped text.
// ---------------------------------------------------------------------------

#[test]
fn dockerfile_pins_the_versions_the_interop_harness_is_verified_against() {
    let dockerfile = read(deployment_dir().join("Dockerfile"));
    assert!(
        dockerfile.contains(&format!("ARG RNS_VERSION={RNS_VERSION}")),
        "Dockerfile must default RNS_VERSION to {RNS_VERSION}"
    );
    assert!(
        dockerfile.contains(&format!("ARG LXMF_VERSION={LXMF_VERSION}")),
        "Dockerfile must default LXMF_VERSION to {LXMF_VERSION}"
    );
    assert!(
        dockerfile.contains("\"rns==${RNS_VERSION}\"")
            && dockerfile.contains("\"lxmf==${LXMF_VERSION}\""),
        "pip must install the two packages pinned with `==` to the ARGs"
    );

    let interop_readme = read(
        repo_root()
            .join("scripts")
            .join("mesh-interop")
            .join("README.md"),
    );
    assert!(
        interop_readme.contains(&format!("`{RNS_VERSION}`"))
            && interop_readme.contains(&format!("`{LXMF_VERSION}`")),
        "scripts/mesh-interop/README.md no longer names RNS {RNS_VERSION} / LXMF {LXMF_VERSION}; the image pins must move with it"
    );
}

#[test]
fn dockerfile_uses_a_slim_base_and_runs_as_uid_1000() {
    let dockerfile = read(deployment_dir().join("Dockerfile"));
    let from = dockerfile
        .lines()
        .find(|l| l.starts_with("FROM "))
        .expect("Dockerfile has a FROM line");
    assert!(
        from.contains("-slim"),
        "base image must be a slim variant, got {from:?}"
    );
    assert!(
        dockerfile.contains("--uid 1000"),
        "the daemon user must be uid 1000 (matches Coyote's own image)"
    );
    assert!(
        dockerfile.contains("\nUSER lxmd"),
        "the image must drop to the lxmd user before ENTRYPOINT"
    );
    assert!(
        dockerfile.contains("VOLUME [\"/data\"]"),
        "state must live on the /data volume"
    );
    assert!(
        dockerfile.contains("EXPOSE 4242/tcp"),
        "the Reticulum TCP port must be declared"
    );
}

#[test]
fn the_pip_ca_build_secret_is_optional_and_does_not_weaken_tls() {
    let dockerfile = read(deployment_dir().join("Dockerfile"));
    assert!(
        dockerfile.contains("--mount=type=secret,id=pip_ca,required=false"),
        "pip_ca must be an optional build secret so the documented plain `docker build` works"
    );
    assert!(
        dockerfile.contains("PIP_CERT=/run/secrets/pip_ca"),
        "the secret must be handed to pip as PIP_CERT (a trust anchor, not a verification bypass)"
    );
    let lower = dockerfile.to_ascii_lowercase();
    for forbidden in [
        "--trusted-host",
        "pip_trusted_host",
        "--insecure",
        "verify=false",
        "pythonhttpsverify",
    ] {
        assert!(
            !lower.contains(forbidden),
            "Dockerfile must not weaken TLS via {forbidden:?}"
        );
    }
    let entrypoint = read(deployment_dir().join("entrypoint.sh"));
    assert!(
        !entrypoint.contains("PIP_CERT") && !entrypoint.contains("pip "),
        "the runtime entrypoint has no business with pip"
    );
}

/// Keys of one section of an INI-ish config, comments and blanks stripped.
fn section_keys(config: &str, section: &str) -> Vec<(String, String)> {
    let mut in_section = false;
    let mut keys = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if in_section && let Some((k, v)) = line.split_once('=') {
            keys.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    keys
}

#[test]
fn shipped_lxmd_config_enables_the_node_and_requires_auth() {
    let config = read(deployment_dir().join("lxmd.config"));
    let propagation = section_keys(&config, "propagation");
    let get = |k: &str| {
        propagation
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("[propagation] lacks `{k}`; have {propagation:?}"))
    };
    assert_eq!(
        get("enable_node"),
        "yes",
        "the container must run as a propagation node"
    );
    assert_eq!(
        get("auth_required"),
        "yes",
        "fetching must be gated on the allowed list"
    );
    assert_eq!(
        get("autopeer"),
        "no",
        "peer sync is not gated by auth_required; a private node must not autopeer"
    );
    assert_eq!(
        get("announce_at_start"),
        "yes",
        "a (re)started node must be heard at once"
    );
    let interval: u32 = get("announce_interval")
        .parse()
        .expect("announce_interval is minutes");
    assert!(
        interval > 0 && interval <= 60,
        "announce_interval should be short, got {interval}"
    );
}

#[test]
fn shipped_reticulum_config_listens_on_4242_with_transport_enabled() {
    let config = read(deployment_dir().join("reticulum.config"));
    let reticulum = section_keys(&config, "reticulum");
    assert!(
        reticulum
            .iter()
            .any(|(k, v)| k == "enable_transport" && v.eq_ignore_ascii_case("yes")),
        "enable_transport must be on so the container relays between Coyote nodes; got {reticulum:?}"
    );
    assert!(
        config.contains("type = TCPServerInterface"),
        "the peer interface must be a TCP server"
    );
    assert!(
        config.contains("listen_port = 4242"),
        "the peer interface must listen on 4242 (what `type: private` dials)"
    );
    assert!(
        config.contains("listen_ip = 0.0.0.0"),
        "the peer interface must bind all addresses inside the container"
    );
}

#[test]
fn readme_documents_the_private_network_contract() {
    let readme = read(deployment_dir().join("README.md"));
    for needle in [
        "enable_node = yes",
        "auth_required = yes",
        "/data/lxmd/allowed",
        "one Reticulum identity hash per line, 32 hex",
        "fails closed",
        EMPTY_ALLOWED_WARNING,
        "ERROR_NO_ACCESS",
        "`auth_required` gates fetching only",
        "autopeer = no",
        "static_peers",
        "from_static_only",
        "ingress_control = No",
        "/data/reticulum/storage/",
        "-p <lan-or-vpn-ip>:4242:4242",
        "-v coyote-pn-data:/data",
    ] {
        assert!(
            readme.contains(needle),
            "deployment README must state {needle:?}"
        );
    }
    assert!(
        !readme.contains("-p 4242:4242"),
        "run recipes must publish 4242 on a specific address, not every address"
    );
    // The example allowed file: at least two full 32-hex lines inside a code block.
    let example_lines = readme
        .lines()
        .filter(|l| l.len() == 32 && l.bytes().all(|b| b.is_ascii_hexdigit()))
        .count();
    assert!(
        example_lines >= 2,
        "README must show an example `allowed` file with 32-hex lines, found {example_lines}"
    );
    // Same-config-dir => same-identity portability note.
    assert!(
        readme.contains("identity lives on the volume")
            && readme.contains("fresh volume mints a fresh identity")
            && readme.contains("copy to move, never to duplicate"),
        "README must explain that the identity travels with the volume"
    );
}

#[test]
fn readme_allowed_file_rules_match_upstreams_loader() {
    // lxmd 0.9.6 reads `allowed` with `bytes.splitlines()`, which treats CRLF as a
    // line break, so a CRLF-terminated 32-hex line is accepted; only lines whose
    // remaining bytes are not exactly 32 (whitespace, comments, short/long) are
    // dropped. The README must not tell operators otherwise.
    let readme = read(deployment_dir().join("README.md"));
    assert!(
        readme.contains("LF or CRLF line endings both work"),
        "README must say a CRLF-terminated `allowed` line is accepted; lxmd 0.9.6 strips the CRLF in bytes.splitlines. Live evidence: crlf_terminated_allowed_lines_are_accepted_by_lxmd"
    );
}

#[test]
fn operator_docs_do_not_cite_upstreams_dockerfile() {
    let docs = [
        ("deployment README", deployment_dir().join("README.md")),
        ("root README", repo_root().join("README.md")),
        (
            "PROTOCOL.md",
            repo_root().join("docs").join("mesh").join("PROTOCOL.md"),
        ),
    ];
    for (name, path) in docs {
        let text = read(&path);
        for citation in ["LXMF/Dockerfile", "Reticulum/Dockerfile"] {
            assert!(
                !text.contains(citation),
                "{name} must not point at upstream's build-only Dockerfile via {citation:?}"
            );
        }
        // Links do not re-wrap, so a URL-ish token naming a Dockerfile is a citation.
        for token in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            if !token.contains("Dockerfile") {
                continue;
            }
            assert!(
                !token.contains("markqvist")
                    && !token.contains("github.com/")
                    && !token.contains("http"),
                "{name} must not link upstream's build-only Dockerfile: {token:?}"
            );
        }
    }
    let readme = read(deployment_dir().join("README.md"));
    assert!(
        readme.contains("github.com/markqvist/LXMF#"),
        "deployment README should link the upstream LXMF README for context"
    );
}

#[test]
fn root_readme_and_protocol_spec_point_at_the_operator_docs() {
    let root = read(repo_root().join("README.md"));
    assert!(
        root.contains("deployment/propagation-node/README.md"),
        "root README must mention that the propagation-node image exists"
    );
    let protocol = read(repo_root().join("docs").join("mesh").join("PROTOCOL.md"));
    assert!(
        protocol.contains("https://github.com/Dark-Alex-17/coyote/wiki/Mesh")
            && protocol.contains("wire format only"),
        "PROTOCOL.md must cross-link the operator wiki and keep itself to the wire format"
    );
}

#[test]
fn shipped_files_are_ascii_lf_and_carry_no_plan_references() {
    let mut on_disk: Vec<String> = fs::read_dir(deployment_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort_unstable();
    assert_eq!(
        on_disk, SHIPPED_FILES,
        "deployment/propagation-node/ holds something SHIPPED_FILES does not list (or vice versa); every file there ships in the image context"
    );
    for name in SHIPPED_FILES {
        let path = deployment_dir().join(name);
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(bytes.is_ascii(), "{name} must be plain ASCII");
        assert!(!bytes.contains(&b'\r'), "{name} must use LF line endings");
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("TASK-") && !text.contains("PLAN-"),
            "{name} must not reference plan/task tracking; that belongs in commit messages"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(deployment_dir().join("entrypoint.sh")).unwrap();
        assert!(
            mode.permissions().mode() & 0o111 != 0,
            "entrypoint.sh should be executable in the tree"
        );
    }
}

#[test]
fn entrypoint_tightens_the_umask_and_seeds_on_every_daemon_path() {
    let entrypoint = read(deployment_dir().join("entrypoint.sh"));
    assert!(
        entrypoint.contains("umask 077"),
        "RNS writes identities with a plain open(path, \"wb\"); without `umask 077` they are world-readable"
    );
    let (_, after_arm) = entrypoint
        .split_once("lxmd)")
        .expect("entrypoint.sh has an `lxmd)` arm");
    let arm = after_arm
        .split_once(";;")
        .map(|(arm, _)| arm)
        .unwrap_or(after_arm);
    assert!(
        !arm.contains("exec"),
        "the `lxmd)` arm must fall through to the seed block, not exec early (a fresh volume would get lxmd's own defaults):{arm}"
    );
    let seed = entrypoint
        .find("/opt/coyote-pn/lxmd.config")
        .expect("entrypoint.sh seeds lxmd.config");
    let final_exec = entrypoint
        .rfind("exec lxmd --config /data/lxmd --rnsconfig /data/reticulum")
        .expect("entrypoint.sh ends by exec-ing lxmd against the volume");
    assert!(
        seed < final_exec,
        "the seed block must run before the daemon starts"
    );
}

// ---------------------------------------------------------------------------
// Live: build and run the image. Needs Docker; opt in with COYOTE_PN_IMAGE_TESTS=1.
// ---------------------------------------------------------------------------

const LIVE_SWITCH: &str = "COYOTE_PN_IMAGE_TESTS";

/// Same semantics as `COYOTE_MESH_INTEROP`: unset, blank, `0` and `false`/`no`/`off`
/// (any ASCII case) are off; anything else is on.
fn live_switch_is_on(value: Option<&OsStr>) -> bool {
    let Some(value) = value else { return false };
    let value = value.to_string_lossy();
    let value = value.trim();
    !(value.is_empty()
        || value == "0"
        || ["false", "no", "off"]
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off)))
}

/// Print the conventional `skipping:` line and report whether to bail.
fn skip_unless_live() -> bool {
    let value = env::var_os(LIVE_SWITCH);
    if live_switch_is_on(value.as_deref()) {
        return false;
    }
    let instructions = "`cargo test --test propagation_node_image -- --include-ignored` with a Docker daemon available";
    match value {
        Some(value) => eprintln!(
            "skipping: {LIVE_SWITCH}={value:?} is off; set it to 1 and run {instructions}"
        ),
        None => eprintln!("skipping: set {LIVE_SWITCH}=1 and run {instructions}"),
    }
    true
}

fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn docker {args:?}: {e}"))
}

fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {args:?} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_prefix() -> String {
    format!("coyote-pn-test-{}", std::process::id())
}

fn label() -> String {
    format!("coyote-pn-test={}", std::process::id())
}

static IMAGE: OnceLock<String> = OnceLock::new();

/// Build the image once per process (the daemon's layer cache makes repeats cheap)
/// and return its tag.
fn build_image() -> String {
    IMAGE
        .get_or_init(|| {
            let tag = format!("{}:latest", run_prefix());
            let mut args: Vec<String> = vec![
                "build".into(),
                "-q".into(),
                "--label".into(),
                label(),
                "-t".into(),
                tag.clone(),
            ];
            for var in ["HTTP_PROXY", "HTTPS_PROXY"] {
                if let Ok(v) = env::var(var) {
                    args.push("--build-arg".into());
                    args.push(format!("{var}={v}"));
                }
            }
            if let Ok(ca) = env::var("COYOTE_PN_PIP_CA") {
                args.push("--secret".into());
                args.push(format!("id=pip_ca,src={ca}"));
            }
            args.push(deployment_dir().to_string_lossy().into_owned());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            docker_ok(&refs);
            tag
        })
        .clone()
}

/// A container plus the volume it runs against, removed on drop (panics unwind, so
/// a failed assertion still tears down).
struct Node {
    name: String,
    volume: String,
    image: String,
}

impl Node {
    fn fresh(image: &str, suffix: &str) -> Self {
        let name = format!("{}-{suffix}", run_prefix());
        let volume = format!("{name}-vol");
        docker_ok(&["volume", "create", "--label", &label(), &volume]);
        Node {
            name,
            volume,
            image: image.to_string(),
        }
    }

    /// Start the daemon detached with no command. `extra` is spliced before the image (ports etc.).
    fn start(&self, extra: &[&str]) {
        self.start_with(extra, &[]);
    }

    /// Start detached; `command` follows the image and goes to the entrypoint.
    fn start_with(&self, extra: &[&str], command: &[&str]) {
        let lbl = label();
        let mount = format!("{}:/data", self.volume);
        let mut args = vec![
            "run", "-d", "--name", &self.name, "--label", &lbl, "-v", &mount,
        ];
        args.extend_from_slice(extra);
        args.push(&self.image);
        args.extend_from_slice(command);
        docker_ok(&args);
    }

    fn logs(&self) -> String {
        let out = docker(&["logs", &self.name]);
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// Poll the logs until `needle` appears (or fail after 60 s with the log so far).
    fn wait_for(&self, needle: &str) -> String {
        self.wait_for_nth(needle, 1)
    }

    /// Poll the logs until `needle` has appeared `n` times (or fail after 60 s with the
    /// log so far).
    fn wait_for_nth(&self, needle: &str, n: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let logs = self.logs();
            if logs.matches(needle).count() >= n {
                return logs;
            }
            assert!(
                Instant::now() < deadline,
                "{} never logged {needle:?} {n} time(s); log so far:\n{logs}",
                self.name
            );
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_started(&self) -> String {
        self.wait_for("Started lxmd version")
    }

    fn restart(&self) {
        docker_ok(&["restart", &self.name]);
    }

    /// Run a one-off shell command against this node's volume (as uid 1000, via the
    /// image's own `sh` passthrough).
    fn on_volume(&self, script: &str) -> String {
        let lbl = label();
        let mount = format!("{}:/data", self.volume);
        docker_ok(&[
            "run",
            "--rm",
            "--label",
            &lbl,
            "-v",
            &mount,
            &self.image,
            "sh",
            "-c",
            script,
        ])
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
        let _ = docker(&["volume", "rm", "-f", &self.volume]);
    }
}

/// The two destination hashes the daemon reports at start, in a fixed order.
fn destination_hashes(logs: &str) -> Vec<String> {
    let mut hashes: Vec<String> = logs
        .lines()
        .filter(|l| {
            l.contains("LXMF Router ready to receive on <")
                || l.contains("LXMF Propagation Node started on <")
        })
        .filter_map(|l| {
            let start = l.rfind('<')?;
            let end = l.rfind('>')?;
            let kind = if l.contains("Router ready") {
                "router"
            } else {
                "node"
            };
            Some(format!("{kind}:{}", &l[start + 1..end]))
        })
        .collect();
    hashes.sort();
    hashes.dedup();
    hashes
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn image_runs_lxmd_and_rnsd_at_the_pinned_versions() {
    if skip_unless_live() {
        return;
    }
    let image = build_image();
    let lbl = label();
    let version = docker_ok(&["run", "--rm", "--label", &lbl, &image, "--version"]);
    assert_eq!(
        version.trim(),
        format!("lxmd {LXMF_VERSION}"),
        "`docker run --rm <image> --version`"
    );
    let via_lxmd = docker_ok(&["run", "--rm", "--label", &lbl, &image, "lxmd", "--version"]);
    assert_eq!(
        via_lxmd.trim(),
        format!("lxmd {LXMF_VERSION}"),
        "`docker run --rm <image> lxmd --version`"
    );
    let rnsd = docker_ok(&["run", "--rm", "--label", &lbl, &image, "rnsd", "--version"]);
    assert_eq!(
        rnsd.trim(),
        format!("rnsd {RNS_VERSION}"),
        "`docker run --rm <image> rnsd --version`"
    );
    let example = docker_ok(&["run", "--rm", "--label", &lbl, &image, "--exampleconfig"]);
    assert!(
        example.contains("[propagation]"),
        "--exampleconfig must pass through to lxmd"
    );
    let id = docker_ok(&["run", "--rm", "--label", &lbl, &image, "sh", "-c", "id -u"]);
    assert_eq!(id.trim(), "1000", "the image must run as uid 1000");
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn cold_start_on_an_empty_volume_seeds_state_owned_by_uid_1000_and_fails_closed() {
    if skip_unless_live() {
        return;
    }
    let image = build_image();
    let node = Node::fresh(&image, "cold");
    node.start(&[]);
    let logs = node.wait_started();
    assert!(
        logs.contains("LXMF Router ready to receive on <"),
        "router line missing:\n{logs}"
    );
    assert!(
        logs.contains(EMPTY_ALLOWED_WARNING),
        "empty allowed list must log the fail-closed warning:\n{logs}"
    );
    assert!(
        logs.contains(&format!("Started lxmd version {LXMF_VERSION}")),
        "version line missing:\n{logs}"
    );

    let listing = node.on_volume(
        "set -e; for p in /data/lxmd/config /data/lxmd/allowed /data/lxmd/identity /data/lxmd/storage /data/reticulum/config; do \
           test -e \"$p\" || { echo MISSING $p; exit 1; }; done; \
         echo NOT_UID_1000=$(find /data ! -uid 1000 | wc -l); \
         echo ALLOWED_BYTES=$(wc -c < /data/lxmd/allowed); \
         cmp -s /opt/coyote-pn/lxmd.config /data/lxmd/config && echo LXMD_SEEDED; \
         cmp -s /opt/coyote-pn/reticulum.config /data/reticulum/config && echo RNS_SEEDED",
    );
    assert!(
        listing.contains("NOT_UID_1000=0"),
        "every path on the volume must be owned by uid 1000:\n{listing}"
    );
    assert!(
        listing.contains("ALLOWED_BYTES=0"),
        "allowed must be created empty:\n{listing}"
    );
    assert!(
        listing.contains("LXMD_SEEDED") && listing.contains("RNS_SEEDED"),
        "configs must be seeded byte-for-byte:\n{listing}"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn restart_keeps_the_identity_and_never_overwrites_operator_edits() {
    if skip_unless_live() {
        return;
    }
    let image = build_image();
    let node = Node::fresh(&image, "restart");
    node.start(&[]);
    let first = destination_hashes(&node.wait_started());
    assert_eq!(
        first.len(),
        2,
        "expected router + node hashes, got {first:?}"
    );

    node.on_volume(
        "printf '00000000000000000000000000000000\\n' > /data/lxmd/allowed; \
         printf '\\n# operator marker\\n' >> /data/lxmd/config; \
         printf '\\n# operator marker\\n' >> /data/reticulum/config",
    );
    node.restart();
    // Wait for the second start line, then look only at the post-restart log.
    let logs = node.wait_for_nth("Started lxmd version", 2);
    let after = logs
        .rsplit_once("Substantiating Reticulum")
        .map(|(_, tail)| tail.to_string())
        .unwrap_or(logs.clone());
    assert!(
        after.contains("Loaded Primary Identity <"),
        "second start must load, not mint, the identity:\n{after}"
    );
    assert_eq!(
        destination_hashes(&after),
        first,
        "destination hashes must survive a restart"
    );
    assert!(
        !after.contains("Clint authentication was enabled"),
        "one valid hash in allowed must silence the empty-list warning:\n{after}"
    );

    let markers = node.on_volume(
        "echo LXMD_MARKERS=$(grep -c 'operator marker' /data/lxmd/config); \
         echo RNS_MARKERS=$(grep -c 'operator marker' /data/reticulum/config); \
         echo ALLOWED=$(cat /data/lxmd/allowed)",
    );
    assert!(
        markers.contains("LXMD_MARKERS=1"),
        "lxmd config edit must survive restart:\n{markers}"
    );
    assert!(
        markers.contains("RNS_MARKERS=1"),
        "reticulum config edit must survive restart:\n{markers}"
    );
    assert!(
        markers.contains("ALLOWED=00000000000000000000000000000000"),
        "allowed edit must survive restart:\n{markers}"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn a_copied_volume_yields_the_same_node_identity() {
    if skip_unless_live() {
        return;
    }
    let image = build_image();
    let source = Node::fresh(&image, "src");
    source.start(&[]);
    let original = destination_hashes(&source.wait_started());
    docker_ok(&["stop", &source.name]);

    let copy = Node::fresh(&image, "copy");
    let lbl = label();
    let src_mount = format!("{}:/src:ro", source.volume);
    let dst_mount = format!("{}:/data", copy.volume);
    docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-v",
        &src_mount,
        "-v",
        &dst_mount,
        &image,
        "sh",
        "-c",
        "cp -a /src/. /data/",
    ]);
    copy.start(&[]);
    let copied = copy.wait_started();
    assert!(
        copied.contains("Loaded Primary Identity <"),
        "a copied config dir must load the existing identity:\n{copied}"
    );
    assert_eq!(
        destination_hashes(&copied),
        original,
        "same config dir => same destination hashes"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn port_4242_accepts_tcp_connections_when_published() {
    if skip_unless_live() {
        return;
    }
    let image = build_image();
    let node = Node::fresh(&image, "port");
    // An ephemeral host port keeps this test off any operator's 4242.
    node.start(&["-p", "127.0.0.1::4242"]);
    node.wait_started();
    let mapping = docker_ok(&["port", &node.name, "4242/tcp"]);
    let addr = mapping
        .lines()
        .find(|l| l.starts_with("127.0.0.1:"))
        .unwrap_or_else(|| panic!("no v4 mapping in {mapping:?}"));
    let mut stream = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    // Docker's userland proxy accepts the connection itself, so connecting proves
    // nothing; only a backend that holds the socket open (or speaks first) does.
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        Ok(n) if n > 0 => {}
        Ok(_) => panic!(
            "{addr} closed at once: the proxy found nothing listening on 4242 in the container"
        ),
        Err(e) => panic!("{addr} read failed instead of holding the connection: {e}"),
    }
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn the_lxmd_arm_seeds_a_fresh_volume_before_starting_the_daemon() {
    if skip_unless_live() {
        return;
    }
    // `docker run -v vol:/data IMG lxmd` must not let lxmd mint its own defaults
    // (node off, auth off, autopeer on) into a fresh volume, where the seed guards
    // would then keep them forever.
    let image = build_image();
    let node = Node::fresh(&image, "arm");
    node.start_with(&[], &["lxmd"]);
    node.wait_started();
    let listing = node.on_volume(
        "set -e; \
         diff /data/lxmd/config /opt/coyote-pn/lxmd.config && echo LXMD_SEEDED; \
         diff /data/reticulum/config /opt/coyote-pn/reticulum.config && echo RNS_SEEDED; \
         test -f /data/lxmd/allowed && echo ALLOWED_PRESENT; \
         stat -c 'MODE=%a %n' /data/lxmd/identity /data/reticulum/storage/transport_identity",
    );
    assert!(
        listing.contains("LXMD_SEEDED") && listing.contains("RNS_SEEDED"),
        "the `lxmd` arm must reach the seed block before exec-ing the daemon:\n{listing}"
    );
    assert!(
        listing.contains("ALLOWED_PRESENT"),
        "the `lxmd` arm must create the allowed list too:\n{listing}"
    );
    assert!(
        listing.contains("MODE=600 /data/lxmd/identity")
            && listing.contains("MODE=600 /data/reticulum/storage/transport_identity"),
        "both identities must be created mode 600 (umask 077):\n{listing}"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn crlf_terminated_allowed_lines_are_accepted_by_lxmd() {
    if skip_unless_live() {
        return;
    }
    // Ground truth for `readme_allowed_file_rules_match_upstreams_loader`: with the
    // only entry CRLF-terminated the daemon must NOT log the empty-list warning,
    // while a trailing space (33 bytes) must.
    let image = build_image();
    let node = Node::fresh(&image, "crlf");
    node.on_volume("mkdir -p /data/lxmd && printf '00000000000000000000000000000000\\r\\n' > /data/lxmd/allowed");
    node.start(&[]);
    let logs = node.wait_started();
    assert!(
        !logs.contains("Clint authentication was enabled"),
        "a CRLF-terminated 32-hex line is accepted by lxmd {LXMF_VERSION}; the warning must not appear:\n{logs}"
    );

    let spaced = Node::fresh(&image, "space");
    spaced.on_volume(
        "mkdir -p /data/lxmd && printf '00000000000000000000000000000000 \\n' > /data/lxmd/allowed",
    );
    spaced.start(&[]);
    let logs = spaced.wait_started();
    assert!(
        logs.contains("Clint authentication was enabled"),
        "a line with inline whitespace is not 32 bytes and must be dropped, leaving the list empty:\n{logs}"
    );
}

// ---------------------------------------------------------------------------
// Usage probe (spec-first, written from the README's promises before the
// entrypoint was read): the `lxmd` passthrough, the argument fallthrough, the
// shutdown path and the identity-portability fine print.
// ---------------------------------------------------------------------------

/// The interop harness checkout of `clone` (`lxmf` or `reticulum`, as
/// `scripts/mesh-interop/setup.sh` names them). `override_var` wins when set;
/// `COYOTE_MESH_INTEROP_DIR` is authoritative when set, as it is in CI, so a
/// missing clone there fails the version guard instead of skipping; the
/// `$HOME/.cache/coyote/mesh-interop` default is used only when it holds the
/// `package` tree, since a checkout that never ran `setup.sh` has nothing to check.
fn upstream_clone(override_var: &str, clone: &str, package: &str) -> Option<PathBuf> {
    if let Some(dir) = env::var_os(override_var) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = env::var_os("COYOTE_MESH_INTEROP_DIR") {
        return Some(PathBuf::from(dir).join(clone));
    }
    let default = dirs::home_dir()?
        .join(".cache")
        .join("coyote")
        .join("mesh-interop")
        .join(clone);
    default.join(package).is_dir().then_some(default)
}

/// Pinned LXMF clone the README's `file:line` citations are checked against:
/// `COYOTE_PN_UPSTREAM_LXMF=/path/to/LXMF`, else the interop harness `lxmf` checkout
/// (see [`upstream_clone`]). `None` means the citation test prints `skipping:`.
fn upstream_lxmf_dir() -> Option<PathBuf> {
    upstream_clone("COYOTE_PN_UPSTREAM_LXMF", "lxmf", "LXMF")
}

/// The RNS twin of [`upstream_lxmf_dir`]: `COYOTE_PN_UPSTREAM_RNS`, else the interop
/// harness `reticulum` checkout.
fn upstream_rns_dir() -> Option<PathBuf> {
    upstream_clone("COYOTE_PN_UPSTREAM_RNS", "reticulum", "RNS")
}

/// Lines `from..=to` (1-based, inclusive) of `path`, joined with `\n`.
fn cited_lines(path: &Path, from: usize, to: usize) -> String {
    let text = read(path);
    let total = text.lines().count();
    assert!(
        to <= total,
        "{}: cited range {from}-{to} runs past the file ({total} lines)",
        path.display()
    );
    text.lines()
        .skip(from - 1)
        .take(to - from + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn usage_probe_readme_upstream_citations_resolve_in_the_pinned_lxmf_clone() {
    let Some(lxmf) = upstream_lxmf_dir() else {
        eprintln!(
            "skipping: no pinned LXMF clone; run scripts/mesh-interop/setup.sh (checks it out under ${{COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}}/lxmf) or set COYOTE_PN_UPSTREAM_LXMF; cannot check the README's file:line citations"
        );
        return;
    };
    let version = read(lxmf.join("LXMF").join("_version.py"));
    assert!(
        version.contains(&format!("\"{LXMF_VERSION}\"")),
        "the pinned clone is not LXMF {LXMF_VERSION}: {version:?}"
    );
    let lxmd_py = lxmf.join("LXMF").join("Utilities").join("lxmd.py");
    let router_py = lxmf.join("LXMF").join("LXMRouter.py");
    let readme = read(deployment_dir().join("README.md"));
    let entrypoint = read(deployment_dir().join("entrypoint.sh"));

    // Every citation the shipped files make, with what the cited range must contain.
    // (citing file, `file:from-to` label as written, path, from, to, expected fragments)
    type Citation<'a> = (&'a str, &'a str, &'a Path, usize, usize, &'a [&'a str]);
    let citations: [Citation<'_>; 10] = [
        (
            "README",
            "lxmd.py:103-116",
            &lxmd_py,
            103,
            116,
            &["\"enable_node\"", "\"auth_required\""],
        ),
        (
            "README",
            "lxmd.py:248-268",
            &lxmd_py,
            248,
            268,
            &[
                "allowed_identities",
                ".splitlines()",
                "TRUNCATED_HASHLENGTH//8*2",
            ],
        ),
        (
            "README",
            "lxmd.py:204-210",
            &lxmd_py,
            204,
            210,
            &["\"static_peers\"", "bytes.fromhex(static_peer)"],
        ),
        (
            "README",
            "lxmd.py:217-220",
            &lxmd_py,
            217,
            220,
            &["\"from_static_only\"", "as_bool(\"from_static_only\")"],
        ),
        (
            "README",
            "LXMRouter.py:309",
            &router_py,
            309,
            309,
            &["node_state    = self.propagation_node and not self.from_static_only"],
        ),
        (
            "README",
            "LXMRouter.py:1417-1429",
            &router_py,
            1417,
            1429,
            &["def identity_allowed", "ERROR_NO_ACCESS"],
        ),
        (
            "README",
            "lxmd.py:639-643",
            &lxmd_py,
            639,
            643,
            &[
                "RNS.Transport.has_path(control_destination.hash)",
                "RNS.Transport.request_path(control_destination.hash)",
            ],
        ),
        (
            "README",
            "lxmd.py:631-635",
            &lxmd_py,
            631,
            635,
            &[
                "def check_timeout",
                "Getting lxmd statistics timed out",
                "exit(200)",
            ],
        ),
        (
            "entrypoint.sh",
            "lxmd.py:307-313",
            &lxmd_py,
            307,
            313,
            &["/etc/lxmd", "/.config/lxmd", "/.lxmd"],
        ),
        (
            "entrypoint.sh",
            "lxmd.py:366-371",
            &lxmd_py,
            366,
            371,
            &[
                "No Primary Identity file found",
                "identity.to_file(identitypath)",
            ],
        ),
    ];
    for (where_, label, path, from, to, needles) in citations {
        let source = if where_ == "README" {
            &readme
        } else {
            &entrypoint
        };
        assert!(
            source.contains(label),
            "{where_} no longer cites `{label}`; update this test's citation table"
        );
        let cited = cited_lines(path, from, to);
        for needle in needles {
            assert!(
                cited.contains(needle),
                "{where_} cites `{label}` for {needle:?}, but LXMF {LXMF_VERSION} has there:\n{cited}"
            );
        }
    }

    // README: "written under /data/lxmd/storage/messages" and "within four hops".
    let lxmd_text = read(&lxmd_py);
    assert!(
        lxmd_text.contains("lxmdir       = storagedir+\"/messages\""),
        "lxmd no longer writes delivered messages to <config>/storage/messages; fix the README's operating note"
    );
    let router_text = read(&router_py);
    assert!(
        router_text.contains("AUTOPEER_MAXDEPTH     = 4"),
        "LXMRouter's autopeer depth default is no longer 4; fix the README's 'within four hops'"
    );

    // entrypoint.sh: "RNS writes identities with a plain open(path, \"wb\") (RNS/Identity.py:665)".
    let Some(rns) = upstream_rns_dir() else {
        eprintln!(
            "skipping: no pinned RNS clone; run scripts/mesh-interop/setup.sh (checks it out under ${{COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}}/reticulum) or set COYOTE_PN_UPSTREAM_RNS; cannot check the entrypoint's RNS/Identity.py citation"
        );
        return;
    };
    let rns_version = read(rns.join("RNS").join("_version.py"));
    assert!(
        rns_version.contains(&format!("\"{RNS_VERSION}\"")),
        "the pinned clone is not RNS {RNS_VERSION}: {rns_version:?}"
    );
    assert!(
        entrypoint.contains("RNS/Identity.py:665"),
        "entrypoint.sh no longer cites `RNS/Identity.py:665`; update this test"
    );
    let cited = cited_lines(&rns.join("RNS").join("Identity.py"), 665, 665);
    assert!(
        cited.contains("open(path, \"wb\")"),
        "entrypoint.sh cites `RNS/Identity.py:665` for the plain `open(path, \"wb\")`, but RNS {RNS_VERSION} has there:\n{cited}"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_lxmd_passthrough_targets_the_volume_config_dirs() {
    if skip_unless_live() {
        return;
    }
    // README: "every path seeds the volume first and then runs lxmd against
    // `/data/lxmd` and `/data/reticulum`, never against `~/.lxmd`." lxmd's remote
    // commands exit 201 when the config dir is missing and 202 when its identity is
    // missing, before they touch the network; anything else means both were found.
    let image = build_image();
    let lbl = label();

    let empty = Node::fresh(&image, "status-empty");
    let mount = format!("{}:/data", empty.volume);
    let out = docker(&[
        "run", "--rm", "--label", &lbl, "-v", &mount, &image, "lxmd", "--status",
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The seed block has created /data/lxmd by now; only the identity, which the
    // daemon mints on its first real start, is missing.
    assert_eq!(
        out.status.code(),
        Some(202),
        "on an empty volume `lxmd --status` must fail on the missing /data/lxmd/identity, not fall back to ~/.lxmd:\n{combined}"
    );
    assert!(
        combined.contains("Identity file not found"),
        "unexpected output:\n{combined}"
    );
    let seeded =
        empty.on_volume("cmp -s /opt/coyote-pn/lxmd.config /data/lxmd/config && echo SEEDED");
    assert!(
        seeded.contains("SEEDED"),
        "`lxmd --status` on an empty volume must have seeded /data/lxmd/config on its way in:\n{seeded}"
    );

    let node = Node::fresh(&image, "status-seeded");
    node.start(&[]);
    node.wait_started();
    docker_ok(&["stop", &node.name]);
    let mount = format!("{}:/data", node.volume);
    let out = docker(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-v",
        &mount,
        &image,
        "lxmd",
        "--status",
        "--timeout",
        "5",
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let code = out.status.code();
    assert!(
        code != Some(201) && code != Some(202),
        "on a cold-started volume `lxmd --status` must find /data/lxmd/config and /data/lxmd/identity (got exit {code:?}):\n{combined}"
    );
    assert!(
        !combined.contains("configuration directory does not exist")
            && !combined.contains("Identity file not found"),
        "the passthrough did not point lxmd at the volume:\n{combined}"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_unlisted_arguments_reach_the_daemon_not_a_tool() {
    if skip_unless_live() {
        return;
    }
    // README: "`rnsd`, `sh` and `bash` run that program instead of the daemon.
    // Anything else is appended to the daemon's command line ... The other RNS
    // tools (`rnstatus`, `rnpath`, `rnprobe`, `rnid`) are not passed through."
    let image = build_image();
    let lbl = label();
    for tool in ["rnstatus", "rnpath", "rnprobe", "rnid"] {
        // Bare tool name: any option after it (e.g. `--version`) would be parsed by
        // lxmd's argparse first and mask the rejection of the stray positional.
        let out = docker(&["run", "--rm", "--label", &lbl, &image, tool]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !out.status.success(),
            "`{tool}` must not run as a program; got exit {:?} with stdout:\n{stdout}",
            out.status.code()
        );
        assert!(
            stderr.contains("usage: lxmd")
                && stderr.contains(&format!("unrecognized arguments: {tool}")),
            "`{tool}` must land on lxmd's command line and be rejected there; stderr:\n{stderr}\nstdout:\n{stdout}"
        );
        assert!(
            !stdout.contains(&format!("{tool} ")),
            "`{tool}` must not have printed its own version banner:\n{stdout}"
        );
    }

    // README: "`static_peers = <hashes>` is the knob (... it is in `lxmd --exampleconfig`)"
    // and warns against `from_static_only = yes`; both reached through the `lxmd` passthrough.
    let example = docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        &image,
        "lxmd",
        "--exampleconfig",
    ]);
    for knob in ["static_peers", "from_static_only", "autopeer"] {
        assert!(
            example.lines().any(|l| l
                .trim_start_matches(['#', ' '])
                .starts_with(&format!("{knob} "))),
            "`lxmd --exampleconfig` must document `{knob}`:\n{example}"
        );
    }
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_docker_stop_ends_the_daemon_promptly_and_cleanly() {
    if skip_unless_live() {
        return;
    }
    // The daemon is PID 1 (exec in the entrypoint), so `docker stop`'s SIGTERM must
    // end it well inside Docker's 10 s grace period and without the SIGKILL fallback
    // (exit 137) or a signal death (143); an operator's `docker stop`/`restart`
    // must not lose the identity or the message store to a hard kill.
    let image = build_image();
    let node = Node::fresh(&image, "stop");
    node.start(&[]);
    node.wait_started();
    let started = Instant::now();
    docker_ok(&["stop", &node.name]);
    let took = started.elapsed();
    let state = docker_ok(&[
        "inspect",
        &node.name,
        "--format",
        "{{.State.Status}} {{.State.ExitCode}} {{.State.OOMKilled}}",
    ]);
    assert!(
        took < Duration::from_secs(8),
        "docker stop took {took:?}; the daemon did not honour SIGTERM before Docker's 10 s SIGKILL fallback"
    );
    assert_eq!(
        state.trim(),
        "exited 0 false",
        "the daemon must exit 0 on SIGTERM (137 = SIGKILL fallback, 143 = uncaught SIGTERM)"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_copying_only_the_lxmd_identity_moves_destinations_but_not_the_transport_identity() {
    if skip_unless_live() {
        return;
    }
    // README: "Copy the whole directory: `/data/lxmd/identity` alone moves the
    // propagation-node destination but not the relay's transport identity under
    // `/data/reticulum/storage/`."
    let image = build_image();
    let source = Node::fresh(&image, "idsrc");
    source.start(&[]);
    let original = destination_hashes(&source.wait_started());
    assert_eq!(
        original.len(),
        2,
        "expected router + node hashes, got {original:?}"
    );
    docker_ok(&["stop", &source.name]);
    let source_transport = source.on_volume("sha256sum /data/reticulum/storage/transport_identity");
    assert!(
        source_transport.contains("/data/reticulum/storage/transport_identity"),
        "the transport identity must live under /data/reticulum/storage/:\n{source_transport}"
    );

    let partial = Node::fresh(&image, "idcopy");
    let lbl = label();
    let src_mount = format!("{}:/src:ro", source.volume);
    let dst_mount = format!("{}:/data", partial.volume);
    docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-v",
        &src_mount,
        "-v",
        &dst_mount,
        &image,
        "sh",
        "-c",
        "mkdir -p /data/lxmd && cp /src/lxmd/identity /data/lxmd/identity",
    ]);
    partial.start(&[]);
    let logs = partial.wait_started();
    assert!(
        logs.contains("Loaded Primary Identity <"),
        "the copied identity file must be loaded, not replaced:\n{logs}"
    );
    assert_eq!(
        destination_hashes(&logs),
        original,
        "identity alone must carry both LXMF destination hashes"
    );
    let partial_transport =
        partial.on_volume("sha256sum /data/reticulum/storage/transport_identity");
    let digest = |s: &str| s.split_whitespace().next().unwrap_or("").to_string();
    assert_ne!(
        digest(&partial_transport),
        digest(&source_transport),
        "a volume seeded with only /data/lxmd/identity must mint a NEW transport identity, as the README warns"
    );
    assert_eq!(
        digest(&partial_transport).len(),
        64,
        "sha256sum output: {partial_transport:?}"
    );
}

// ---------------------------------------------------------------------------
// README operating notes: the bind-mount recipe, the `lxmd --status` operating
// note against a LIVE daemon, seeding on the non-daemon paths, and the
// store-and-forward wording.
// ---------------------------------------------------------------------------

/// Collapse every whitespace run to one space so a re-wrapped sentence still matches.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn usage_probe_docs_say_messages_are_posted_and_held_messages_are_fetched_back_automatically() {
    let root = one_line(&read(repo_root().join("README.md")));
    assert!(
        root.contains("held messages are fetched automatically every"),
        "root README must say held messages are fetched automatically:\n{root}"
    );
    assert!(
        !root.contains("fetching held messages back is not yet"),
        "root README must not still say fetch-back is not yet wired up"
    );
    assert!(
        root.contains("`.mesh sync`"),
        "root README must name `.mesh sync` as the manual fetch verb"
    );
    // A later file verb named `fetch` may appear here; only the sync sentence is pinned.
    for withdrawn in [
        "on demand with `.mesh fetch`",
        "(`.mesh fetch` still works)",
        "fetch only on `.mesh fetch`",
    ] {
        assert!(
            !root.contains(withdrawn),
            "root README must not name the withdrawn `.mesh fetch` verb as the manual fetch: {withdrawn}"
        );
    }
    let readme = one_line(&read(deployment_dir().join("README.md")));
    assert!(
        readme.contains("it posts the message to a propagation node"),
        "deployment README must say a message that cannot be delivered directly is posted to the node"
    );
    assert!(
        readme.contains("Held messages are fetched back automatically every"),
        "deployment README must state that fetch-back runs on a schedule in this build"
    );
    assert!(
        readme.contains("`0` turns the automatic path off"),
        "deployment README must say what an interval of 0 does"
    );
    assert!(
        readme.contains("A Coyote node with `announce: false` never fetches on its own"),
        "deployment README must say a Coyote node that does not announce never fetches on its own"
    );
    assert!(
        readme.contains(
            "Coyote's table of propagation nodes is not kept across a Coyote restart, so after a restart the first fetch waits for the node's next announce (up to `announce_interval`, 30 minutes with the shipped `lxmd.config`; `.mesh sync` is refused until then too)"
        ),
        "deployment README must say whose table is lost on restart and how long the first fetch can wait"
    );
    assert!(
        readme
            .contains("A `.mesh knock` to an unreachable peer is parked on the node the same way"),
        "deployment README must say a knock to an unreachable peer is held by the node too"
    );
    assert!(
        readme.contains(
            "a held message whose sender has not announced since the restart is kept on the node until it has been seen on three fetches and at least 15 minutes (one peer heartbeat) have passed; then it is dropped"
        ),
        "deployment README must state the deferral budget: three fetches and one heartbeat"
    );
    assert!(
        !readme.contains("Fetching held messages back is not yet triggered"),
        "deployment README must not still say fetch-back is not yet triggered"
    );
    // A later file verb named `fetch` may appear here; only the sync sentences are pinned.
    assert!(
        readme.contains("`.mesh sync` runs a fetch now"),
        "deployment README must name `.mesh sync` as the manual fetch verb"
    );
    for withdrawn in [
        "`.mesh fetch` runs a fetch now",
        "`.mesh fetch` is refused",
        "`.mesh fetch` does",
    ] {
        assert!(
            !readme.contains(withdrawn),
            "deployment README must not name the withdrawn `.mesh fetch` verb as the manual fetch: {withdrawn}"
        );
    }
}

#[test]
fn usage_probe_readme_status_note_matches_what_docker_exec_actually_does() {
    // The round-3 README claimed that both `lxmd --status` forms write into the
    // daemon's live `/data/reticulum/storage/` on exit. Live ground truth
    // (usage_probe_lxmd_status_against_a_live_daemon): inside the
    // daemon's network namespace the second instance never comes up. It dies while
    // creating the "Coyote Peers" TCPServerInterface with `[Errno 98] Address already
    // in use` (exit 255) and writes NOTHING under /data/reticulum/storage/. Only the
    // separate-namespace `docker run` form reaches the path wait, exits 200 and
    // rewrites destination_table/known_destinations/packet_hashlist.raw/tunnels.
    let readme = one_line(&read(deployment_dir().join("README.md")));
    assert!(
        !readme
            .contains("In both forms the second `RNS.Reticulum(configdir=/data/reticulum)` writes"),
        "the docker exec form does not write into /data/reticulum/storage/: it exits 255 on `Address already in use` before Reticulum is up. Only the `docker run` form writes on exit."
    );
    assert!(
        readme.contains("Address already in use") || readme.contains("cannot bind 4242"),
        "the exec-form note should name what the operator will actually see: the second instance fails to bind the daemon's 4242 listener"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_lxmd_status_against_a_live_daemon() {
    if skip_unless_live() {
        return;
    }
    // README: "`lxmd --status` is not usable from this image, in either form. A second
    // `docker run ... lxmd --status` ... exits 200 at the path wait with `Getting lxmd
    // statistics timed out` ... `docker exec ...` dies with `[Errno 98] Address already
    // in use`, exit 255 ... and writes nothing ... The log is the status surface."
    // Whatever the probe does, the daemon must still be running afterwards.
    let image = build_image();
    let lbl = label();
    let node = Node::fresh(&image, "live-status");
    node.start(&[]);
    node.wait_started();
    let mount = format!("{}:/data", node.volume);

    // --- `docker run` form: separate network namespace, no interface to the daemon.
    node.on_volume("touch /data/probe-marker");
    let out = docker(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-v",
        &mount,
        &image,
        "lxmd",
        "--status",
        "--timeout",
        "5",
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(200),
        "a second `docker run ... lxmd --status` against the live volume must exit 200 at the path wait:\n{combined}"
    );
    assert!(
        combined.contains("Getting lxmd statistics timed out"),
        "the run form should report the statistics timeout, not a config/identity error:\n{combined}"
    );
    let written =
        node.on_volume("find /data/reticulum/storage -type f -newer /data/probe-marker | sort");
    assert!(
        written.contains("known_destinations") || written.contains("destination_table"),
        "README: the run form's second Reticulum writes into the daemon's live storage on exit; nothing newer than the marker:\n{written}"
    );

    // --- `docker exec` form: same namespace, so the second instance collides with
    // the daemon's 4242 listener and never comes up.
    node.on_volume("touch /data/probe-marker");
    let out = docker(&[
        "exec",
        &node.name,
        "lxmd",
        "--config",
        "/data/lxmd",
        "--rnsconfig",
        "/data/reticulum",
        "--status",
        "--timeout",
        "5",
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "`docker exec ... lxmd --status` must not succeed (README: not usable in either form):\n{combined}"
    );
    assert!(
        combined.contains("Address already in use"),
        "inside the daemon's namespace the second instance must die on the 4242 bind, not reach a status query:\n{combined}"
    );
    assert!(
        !combined.contains("Getting lxmd statistics timed out"),
        "the exec form never reaches the path wait:\n{combined}"
    );
    thread::sleep(Duration::from_secs(2));
    let written =
        node.on_volume("find /data/reticulum/storage -type f -newer /data/probe-marker | sort");
    assert!(
        written.trim().is_empty(),
        "the exec form dies before Reticulum is up, so it must not rewrite the daemon's live storage; newer than marker:\n{written}"
    );

    // --- Neither probe may take the daemon down.
    let state = docker_ok(&["inspect", &node.name, "--format", "{{.State.Status}}"]);
    assert_eq!(
        state.trim(),
        "running",
        "a failed status probe must leave the daemon running"
    );
    let logs = node.logs();
    assert_eq!(
        logs.matches("Started lxmd version").count(),
        1,
        "the daemon must not have restarted:\n{logs}"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_version_and_exampleconfig_paths_seed_an_empty_volume_too() {
    if skip_unless_live() {
        return;
    }
    // README: "`docker run --rm coyote-pn lxmd --version`, `lxmd --exampleconfig` and a
    // bare `lxmd` all behave exactly like the same command without the `lxmd`: every
    // path seeds the volume first and then runs lxmd".
    let image = build_image();
    let lbl = label();
    let check = "set -e; \
        cmp -s /opt/coyote-pn/lxmd.config /data/lxmd/config && echo LXMD_SEEDED; \
        cmp -s /opt/coyote-pn/reticulum.config /data/reticulum/config && echo RNS_SEEDED; \
        test -f /data/lxmd/allowed && echo ALLOWED_PRESENT; \
        test ! -e /data/lxmd/identity && echo NO_IDENTITY_YET";
    for (suffix, command) in [
        ("seed-version", vec!["--version"]),
        ("seed-lxmd-version", vec!["lxmd", "--version"]),
        ("seed-example", vec!["lxmd", "--exampleconfig"]),
    ] {
        let node = Node::fresh(&image, suffix);
        let mount = format!("{}:/data", node.volume);
        let mut args = vec!["run", "--rm", "--label", &lbl, "-v", &mount, &image];
        args.extend(command.iter());
        let out = docker_ok(&args);
        if command.contains(&"--version") {
            assert_eq!(out.trim(), format!("lxmd {LXMF_VERSION}"), "{command:?}");
        } else {
            assert!(out.contains("[propagation]"), "{command:?} output:\n{out}");
        }
        let listing = node.on_volume(check);
        for expected in [
            "LXMD_SEEDED",
            "RNS_SEEDED",
            "ALLOWED_PRESENT",
            "NO_IDENTITY_YET",
        ] {
            assert!(
                listing.contains(expected),
                "after `docker run -v vol:/data IMG {}` the volume must be seeded like a daemon start ({expected} missing):\n{listing}",
                command.join(" ")
            );
        }
    }
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_bind_mount_recipe_runs_the_daemon_and_keeps_identities_private() {
    if skip_unless_live() {
        return;
    }
    // README: "The image runs as uid 1000. A bind mount instead of a named volume has
    // to be owned by that uid, and mode 700 keeps the identity files it will hold
    // from other users on the host: install -d -m 700 -o 1000 -g 1000 /srv/coyote-pn".
    #[cfg(not(unix))]
    {
        eprintln!("skipping: the bind-mount recipe is a unix ownership recipe");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let dir = env::temp_dir().join(format!("{}-bind", run_prefix()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        let owner = fs::metadata(&dir).unwrap().uid();
        if owner != 1000 {
            // `install -o 1000` needs root or uid 1000 itself; without either the
            // recipe cannot be reproduced faithfully here.
            let chown = Command::new("chown").arg("1000:1000").arg(&dir).output();
            if !matches!(chown, Ok(ref o) if o.status.success()) {
                eprintln!(
                    "skipping: cannot chown {} to 1000:1000 (running as uid {owner})",
                    dir.display()
                );
                let _ = fs::remove_dir_all(&dir);
                return;
            }
        }
        struct BindNode {
            name: String,
            dir: PathBuf,
        }
        impl Drop for BindNode {
            fn drop(&mut self) {
                let _ = docker(&["rm", "-f", &self.name]);
                let _ = fs::remove_dir_all(&self.dir);
            }
        }
        let image = build_image();
        let lbl = label();
        let bind = BindNode {
            name: format!("{}-bind", run_prefix()),
            dir: dir.clone(),
        };
        let mount = format!("{}:/data", dir.display());
        docker_ok(&[
            "run", "-d", "--name", &bind.name, "--label", &lbl, "-v", &mount, &image,
        ]);
        let deadline = Instant::now() + Duration::from_secs(60);
        let logs = loop {
            let out = docker(&["logs", &bind.name]);
            let logs = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if logs.contains("Started lxmd version") {
                break logs;
            }
            assert!(
                Instant::now() < deadline,
                "daemon on a 700/1000:1000 bind mount never started; log so far:\n{logs}"
            );
            thread::sleep(Duration::from_millis(500));
        };
        assert!(
            logs.contains(EMPTY_ALLOWED_WARNING),
            "the bind-mounted node must run the shipped (auth-on) config:\n{logs}"
        );
        let mode = |rel: &str| {
            let path = dir.join(rel);
            fs::metadata(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("lxmd/identity"), 0o600, "node identity on the host");
        assert_eq!(
            mode("reticulum/storage/transport_identity"),
            0o600,
            "transport identity on the host"
        );
        assert_eq!(mode("lxmd"), 0o700, "/data/lxmd on the host");
        assert_eq!(mode("reticulum"), 0o700, "/data/reticulum on the host");
        assert_eq!(
            fs::metadata(dir.join("lxmd/identity")).unwrap().uid(),
            1000,
            "files the daemon writes must belong to uid 1000 (the recipe's owner)"
        );
        assert_eq!(
            fs::read_to_string(dir.join("lxmd/config")).unwrap(),
            read(deployment_dir().join("lxmd.config")),
            "the bind mount must be seeded with the shipped lxmd.config"
        );
    }
}

// ---------------------------------------------------------------------------
// README multi-node and status notes: the corrected multi-node advice, the
// BuildKit note, the .gitattributes widening, the load-bearing /opt/coyote-pn
// mkdir and the exact exit codes / rewritten-file list the status note now
// promises.
// ---------------------------------------------------------------------------

#[test]
fn usage_probe_readme_multi_node_advice_names_the_knob_coyote_actually_honours() {
    // README: "`static_peers = <hashes>` is the knob ... Do not set `from_static_only
    // = yes` ... on a node Coyote should post to. It clears the propagation-node flag
    // in the node's announce (`LXMRouter.py:309`, ...), and Coyote only posts to nodes
    // that announce that flag, so `mesh__send` would answer `no_propagation_node`".
    let readme = one_line(&read(deployment_dir().join("README.md")));
    assert!(
        readme.contains("`static_peers = <hashes>` is the knob"),
        "README must name static_peers as the multi-node knob"
    );
    assert!(
        readme.contains("Do not set `from_static_only = yes`"),
        "README must warn against from_static_only on a node Coyote posts to"
    );
    assert!(
        !readme.contains("`from_static_only = yes` are the knobs"),
        "the round-3 recommendation of from_static_only must be gone"
    );
    assert!(
        readme.contains("`mesh__send` would answer `no_propagation_node`"),
        "README must name the exact answer the operator will see from Coyote"
    );

    // The shipped node config must not itself trip the warning it gives.
    let lxmd_config = read(deployment_dir().join("lxmd.config"));
    for (key, value) in section_keys(&lxmd_config, "propagation") {
        if key == "from_static_only" {
            assert!(
                !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "yes" | "true" | "1" | "on"
                ),
                "the shipped lxmd.config sets from_static_only = {value}, which hides the node from Coyote"
            );
        }
    }

    // Cross-check the README's claims about Coyote against the tool's own strings:
    // the answer literal exists on the mesh__send path, and the announce parser
    // reads the flag the README says it reads, citing the same upstream line.
    let mesh_tool = read(repo_root().join("src").join("function").join("mesh.rs"));
    assert!(
        mesh_tool.contains("\"no_propagation_node\""),
        "README promises `no_propagation_node`, but src/function/mesh.rs has no such answer literal"
    );
    assert!(
        mesh_tool.contains("mesh__send"),
        "README attributes the answer to `mesh__send`, which src/function/mesh.rs must define"
    );
    let parser = read(repo_root().join("src").join("mesh").join("propagation.rs"));
    assert!(
        parser.contains("LXMRouter.py:309") && parser.contains("NotAPropagationNode"),
        "README says Coyote only posts to nodes announcing the propagation-node flag (LXMRouter.py:309); src/mesh/propagation.rs must read that slot and refuse the rest"
    );
}

#[test]
fn usage_probe_build_note_and_line_ending_pins_match_the_dockerfile() {
    // README: "The build needs BuildKit, the default since Docker 23; older daemons
    // need `DOCKER_BUILDKIT=1`." The Dockerfile's `RUN --mount=type=secret` is
    // BuildKit-only syntax, so the note is load-bearing; if the mount ever goes, the
    // note may too, and vice versa.
    let dockerfile = read(deployment_dir().join("Dockerfile"));
    let readme = read(deployment_dir().join("README.md"));
    let buildkit_only = dockerfile.contains("--mount=type=");
    assert!(
        buildkit_only,
        "the Dockerfile no longer uses BuildKit-only syntax; drop the README's BuildKit note or update this test"
    );
    assert!(
        readme.contains("BuildKit") && readme.contains("`DOCKER_BUILDKIT=1`"),
        "a BuildKit-only Dockerfile must be documented as such, with the legacy-daemon escape hatch"
    );
    assert!(
        dockerfile.contains("--root-user-action=ignore"),
        "pip runs as root in the build stage on purpose; the warning must be silenced, not the user changed"
    );

    // .gitattributes pins LF for everything under deployment/propagation-node/,
    // including any future nested path (`**`, not `*`). Checked through git itself
    // so the glob semantics are git's, not ours.
    let attributes = read(repo_root().join(".gitattributes"));
    assert!(
        attributes.contains("deployment/propagation-node/** text eol=lf"),
        ".gitattributes must pin LF for the whole propagation-node tree"
    );
    let check = Command::new("git")
        .current_dir(repo_root())
        .args([
            "check-attr",
            "eol",
            "--",
            "deployment/propagation-node/entrypoint.sh",
            "deployment/propagation-node/nested/dir/future.sh",
        ])
        .output();
    match check {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let lf_lines = text.lines().filter(|l| l.ends_with(": eol: lf")).count();
            assert_eq!(
                lf_lines, 2,
                "git check-attr must report eol=lf for both a shipped and a nested path:\n{text}"
            );
        }
        _ => eprintln!(
            "skipping the git check-attr half: git is unavailable or this is not a checkout"
        ),
    }
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_seed_templates_are_readable_by_uid_1000() {
    if skip_unless_live() {
        return;
    }
    // Dockerfile: "The mkdir above is load-bearing: left to COPY, /opt/coyote-pn would
    // be created 0644 too and uid 1000 could not traverse it." The entrypoint seeds
    // the volume from /opt/coyote-pn as uid 1000, so the directory must be
    // traversable and the templates readable by that uid, and byte-identical to the
    // files in the tree.
    let image = build_image();
    let lbl = label();
    let out = docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        &image,
        "sh",
        "-c",
        "set -e; echo UID=$(id -u); echo DIR=$(stat -c %a /opt/coyote-pn); \
         echo ENTRY=$(stat -c %a /usr/local/bin/coyote-pn-entrypoint); \
         echo ---LXMD---; cat /opt/coyote-pn/lxmd.config; \
         echo ---RNS---; cat /opt/coyote-pn/reticulum.config",
    ]);
    assert!(
        out.contains("UID=1000\n"),
        "the image must run as uid 1000:\n{out}"
    );
    let dir_mode = out
        .lines()
        .find_map(|l| l.strip_prefix("DIR="))
        .expect("DIR= line");
    let others = u32::from_str_radix(dir_mode, 8).unwrap() & 0o007;
    assert_eq!(
        others & 0o005,
        0o005,
        "/opt/coyote-pn must be readable and traversable by uid 1000 (mode {dir_mode})"
    );
    let entry_mode = out
        .lines()
        .find_map(|l| l.strip_prefix("ENTRY="))
        .expect("ENTRY= line");
    assert_eq!(entry_mode, "755", "entrypoint must be COPY --chmod=0755");
    let (lxmd, rns) = out
        .split_once("---LXMD---\n")
        .and_then(|(_, rest)| rest.split_once("---RNS---\n"))
        .expect("both template dumps present");
    assert_eq!(
        lxmd,
        read(deployment_dir().join("lxmd.config")),
        "the lxmd.config template in the image must be the file in the tree"
    );
    assert_eq!(
        rns,
        read(deployment_dir().join("reticulum.config")),
        "the reticulum.config template in the image must be the file in the tree"
    );
}

#[test]
#[ignore = "needs docker and COYOTE_PN_IMAGE_TESTS=1"]
fn usage_probe_status_note_exit_codes_and_rewritten_files_are_exact() {
    if skip_unless_live() {
        return;
    }
    // README, verbatim promises this test pins exactly (the sibling live-status test
    // pins the messages and the daemon surviving): the `docker run` form "exits 200"
    // and "rewrites `destination_table`, `known_destinations`, `packet_hashlist.raw`
    // and `tunnels`"; the `docker exec` form dies with "exit 255 ... and writes
    // nothing"; "The daemon keeps running through both" -- so 4242 must still accept
    // a connection inside the daemon's namespace afterwards.
    let image = build_image();
    let lbl = label();
    let node = Node::fresh(&image, "status-exact");
    node.start(&[]);
    node.wait_started();
    let mount = format!("{}:/data", node.volume);

    node.on_volume("touch /data/probe-marker");
    let run_form = docker(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-v",
        &mount,
        &image,
        "lxmd",
        "--status",
        "--timeout",
        "5",
    ]);
    assert_eq!(
        run_form.status.code(),
        Some(200),
        "README: the `docker run` form exits 200:\n{}",
        String::from_utf8_lossy(&run_form.stderr)
    );
    let rewritten =
        node.on_volume("find /data/reticulum/storage -type f -newer /data/probe-marker | sort");
    for file in [
        "destination_table",
        "known_destinations",
        "packet_hashlist.raw",
        "tunnels",
    ] {
        assert!(
            rewritten.lines().any(|l| l.ends_with(&format!("/{file}"))),
            "README lists `{file}` among the files the run form rewrites; newer than the marker:\n{rewritten}"
        );
    }

    node.on_volume("touch /data/probe-marker");
    let exec_form = docker(&[
        "exec",
        &node.name,
        "lxmd",
        "--config",
        "/data/lxmd",
        "--rnsconfig",
        "/data/reticulum",
        "--status",
        "--timeout",
        "5",
    ]);
    assert_eq!(
        exec_form.status.code(),
        Some(255),
        "README: the `docker exec` form exits 255:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&exec_form.stdout),
        String::from_utf8_lossy(&exec_form.stderr)
    );
    thread::sleep(Duration::from_secs(2));
    let rewritten =
        node.on_volume("find /data/reticulum/storage -type f -newer /data/probe-marker | sort");
    assert!(
        rewritten.trim().is_empty(),
        "README: the exec form writes nothing; newer than the marker:\n{rewritten}"
    );

    // "The daemon keeps running through both": still the same process, still listening.
    let state = docker_ok(&["inspect", &node.name, "--format", "{{.State.Status}}"]);
    assert_eq!(state.trim(), "running");
    let accept = docker(&[
        "exec",
        &node.name,
        "python3",
        "-c",
        "import socket; socket.create_connection(('127.0.0.1', 4242), 5).close(); print('ACCEPTED')",
    ]);
    assert!(
        accept.status.success() && String::from_utf8_lossy(&accept.stdout).contains("ACCEPTED"),
        "the daemon's 4242 listener must still accept after both probes:\n{}",
        String::from_utf8_lossy(&accept.stderr)
    );
    let logs = node.logs();
    assert_eq!(
        logs.matches("Started lxmd version").count(),
        1,
        "the daemon must not have restarted:\n{logs}"
    );
    assert!(
        !logs.contains("Address already in use"),
        "the exec form's bind failure belongs to the probe, not the daemon's log:\n{logs}"
    );
}
