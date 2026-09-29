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
//!   endings and no plan-tracker references.
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
//!   and labelled `coyote-pn-test=<pid>`, and is removed when the test ends.

use std::env;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
    let crlf_dropped_claim = readme
        .lines()
        .any(|l| l.contains("CRLF") && (l.contains("dropped") || l.contains("drops")));
    assert!(
        !crlf_dropped_claim,
        "README claims a CRLF-terminated `allowed` line is dropped; lxmd 0.9.6 accepts it (bytes.splitlines strips the CRLF). Live evidence: crlf_terminated_allowed_lines_are_accepted_by_lxmd"
    );
}

#[test]
fn readme_does_not_cite_upstreams_dockerfile() {
    let readme = read(deployment_dir().join("README.md"));
    for line in readme.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("dockerfile") {
            continue;
        }
        assert!(
            !lower.contains("markqvist")
                && !lower.contains("github.com/")
                && !lower.contains("http"),
            "README must not point at upstream's build-only Dockerfile: {line:?}"
        );
    }
    // The upstream references that ARE allowed: the LXMF README anchors.
    assert!(
        readme.contains("github.com/markqvist/LXMF#"),
        "README should link the upstream LXMF README for context"
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
    let mode = fs::metadata(deployment_dir().join("entrypoint.sh")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            mode.permissions().mode() & 0o111 != 0,
            "entrypoint.sh should be executable in the tree"
        );
    }
    let _ = mode;
}

// ---------------------------------------------------------------------------
// Live: build and run the image. Needs Docker; opt in with COYOTE_PN_IMAGE_TESTS=1.
// ---------------------------------------------------------------------------

const LIVE_SWITCH: &str = "COYOTE_PN_IMAGE_TESTS";

fn live_switch_is_on() -> bool {
    matches!(
        env::var(LIVE_SWITCH).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Print the conventional `skipping:` line and report whether to bail.
fn skip_unless_live() -> bool {
    if live_switch_is_on() {
        return false;
    }
    eprintln!(
        "skipping: set {LIVE_SWITCH}=1 and run `cargo test --test propagation_node_image -- --include-ignored` with a Docker daemon available"
    );
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

/// Build the image once per process (the daemon's layer cache makes repeats cheap)
/// and return its tag.
fn build_image() -> String {
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

    /// Start the daemon detached. `extra` is spliced before the image (ports etc.).
    fn start(&self, extra: &[&str]) {
        let lbl = label();
        let mount = format!("{}:/data", self.volume);
        let mut args = vec![
            "run", "-d", "--name", &self.name, "--label", &lbl, "-v", &mount,
        ];
        args.extend_from_slice(extra);
        args.push(&self.image);
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
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let logs = self.logs();
            if logs.contains(needle) {
                return logs;
            }
            assert!(
                Instant::now() < deadline,
                "{} never logged {needle:?}; log so far:\n{logs}",
                self.name
            );
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_started(&self) -> String {
        self.wait_for("LXMF Propagation Node started on <")
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
    let logs = loop {
        let logs = node.logs();
        if logs.matches("Started lxmd version").count() >= 2 {
            break logs;
        }
        thread::sleep(Duration::from_millis(500));
    };
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
    let stream = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    drop(stream);
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
