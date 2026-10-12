//! Usage coverage for the propagation-node image in `deployment/propagation-node/`.
//!
//! The image is an operator-facing artefact, so this file checks it the way an
//! operator meets it: the README's promises against the shipped text. Nothing
//! here reaches into `src/`; the expectations are written from the deployment
//! README and the shipped config files, not from the entrypoint.
//!
//! The tests pin the shipped text: the pip pins match what the interop harness
//! is verified against, the shipped `lxmd.config` enables the node and requires
//! auth, the README documents the private-network contract without citing
//! upstream's build-only Dockerfile, the root README points at the operator
//! docs, and every shipped file is plain ASCII with LF endings and no
//! plan-tracker references. The `file:line` citations the README and entrypoint
//! make into upstream are resolved against the interop harness checkout that
//! `scripts/mesh-interop/setup.sh` makes under
//! `${COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}` (the
//! `mesh-interop` CI job runs them there); without it they print `skipping:`.
//! Build and reachability of the running container are covered by
//! `scripts/image-smoke.sh`; the README's behavioural promises are checked
//! against the upstream source citations below.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        "README must say a CRLF-terminated `allowed` line is accepted; lxmd 0.9.6 strips the CRLF in bytes.splitlines"
    );
}

#[test]
fn operator_docs_do_not_cite_upstreams_dockerfile() {
    let docs = [
        ("deployment README", deployment_dir().join("README.md")),
        ("root README", repo_root().join("README.md")),
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
fn root_readme_points_at_the_operator_docs() {
    let root = read(repo_root().join("README.md"));
    assert!(
        root.contains("deployment/propagation-node/README.md"),
        "root README must mention that the propagation-node image exists"
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
// README entrypoint promises: the `lxmd` passthrough, the argument fallthrough,
// the shutdown path and the identity-portability fine print.
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

// ---------------------------------------------------------------------------
// README operating notes: the store-and-forward wording and the `lxmd --status`
// operating note.
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
    // An earlier README draft claimed that both `lxmd --status` forms write into the
    // daemon's live `/data/reticulum/storage/` on exit. Observed against a running
    // container: inside the daemon's network namespace the second instance never comes
    // up. It dies while creating the "Coyote Peers" TCPServerInterface with `[Errno 98]
    // Address already in use` (exit 255) and writes NOTHING under
    // /data/reticulum/storage/. Only the separate-namespace `docker run` form reaches
    // the path wait, exits 200 and rewrites
    // destination_table/known_destinations/packet_hashlist.raw/tunnels.
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

// ---------------------------------------------------------------------------
// README multi-node and build notes: the multi-node advice names `static_peers`
// as the knob and warns off `from_static_only`, and the BuildKit note and the
// .gitattributes line-ending pins match the Dockerfile.
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
        "the earlier recommendation of from_static_only must be gone"
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
