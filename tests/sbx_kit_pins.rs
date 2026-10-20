//! Pins the shipped sandbox kit files and the release plumbing that rewrites
//! them: the dockerfile base image and the kit version both track the crate
//! version, the release workflow seds target the v3 files, and the asset tree
//! holds exactly the two files the embedder expects. The three built-in mixin
//! kits are pinned alongside (host lists verbatim from their v2 predecessors,
//! hook bodies that parse under POSIX sh), and the asset tree is swept for
//! anything in the v2 grammar that a kit rewrite could have left behind.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

const KIT_DIR: &str = "assets/sbx-kit/coyote";
const YAML: &str = "coyote.yaml";
const DOCKERFILE: &str = "coyote.dockerfile";
const MIXIN_KIT_FILES: [&str; 3] = [
    "assets/functions/kits/built-in-tools/built-in-tools.yaml",
    "assets/agents/sisyphus/kits/sisyphus-ddg/sisyphus-ddg.yaml",
    "assets/agents/probe/kits/agent-probe/agent-probe.yaml",
];

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn kit_file(name: &str) -> String {
    read(&format!("{KIT_DIR}/{name}"))
}

fn instruction_lines(dockerfile: &str) -> Vec<&str> {
    dockerfile
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Finder and editor artefacts (`.DS_Store`, `name~`) are not part of the
/// tree and must not red the pin on a developer machine.
fn tracked_names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.') && !name.ends_with('~'))
        .collect()
}

/// Recursive counterpart of `tracked_names`. The artefact filter applies to
/// files only; dot-directories such as `assets/agents/.shared/` are
/// descended into so kits placed under them are swept too.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if !file_name(&path).starts_with('.') && !file_name(&path).ends_with('~') {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn file_name(path: &Path) -> &str {
    path.file_name().unwrap().to_str().unwrap()
}

fn relative_to_repo(path: &Path) -> String {
    path.strip_prefix(repo_path(""))
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every `*.yaml` under any `assets/**/kits/` directory plus every file under
/// `assets/sbx-kit/`.
fn kit_file_set() -> BTreeSet<PathBuf> {
    walk(&repo_path("assets"))
        .into_iter()
        .filter(|path| {
            let relative = relative_to_repo(path);
            relative.starts_with("assets/sbx-kit/")
                || (relative.contains("/kits/") && relative.ends_with(".yaml"))
        })
        .collect()
}

fn string_allow_entries(policy_config: &Value, phase: &str) -> Vec<String> {
    policy_config
        .get(phase)
        .and_then(|p| p.get("allow"))
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

#[test]
fn dockerfile_pins_image_to_crate_version() {
    let dockerfile = kit_file(DOCKERFILE);
    let expected_from = format!("FROM darkalex17/coyote:v{}", env!("CARGO_PKG_VERSION"));
    assert_eq!(
        dockerfile.lines().next(),
        Some("# syntax=docker/dockerfile:1")
    );
    assert!(
        instruction_lines(&dockerfile).contains(&expected_from.as_str()),
        "{dockerfile}"
    );
    assert_eq!(
        dockerfile
            .lines()
            .filter(|line| line.starts_with("FROM darkalex17/coyote:v"))
            .count(),
        1
    );
}

#[test]
fn dockerfile_sets_sandbox_env_in_one_instruction() {
    let dockerfile = kit_file(DOCKERFILE);
    let env_lines: Vec<&str> = instruction_lines(&dockerfile)
        .into_iter()
        .filter(|line| line.starts_with("ENV "))
        .collect();
    assert_eq!(env_lines.len(), 1, "{dockerfile}");
    let raw_tokens: Vec<&str> = env_lines[0]["ENV ".len()..].split_whitespace().collect();
    assert_eq!(raw_tokens.len(), 5, "{}", env_lines[0]);
    let tokens: BTreeSet<&str> = raw_tokens.into_iter().collect();
    let expected: BTreeSet<&str> = [
        "IS_SANDBOX=1",
        "COYOTE_LOG_LEVEL=INFO",
        "COYOTE_CONFIG_DIR=/home/agent/.config/coyote",
        "EDITOR=nano",
        "GOOGLE_API_KEY=proxy-managed",
    ]
    .into_iter()
    .collect();
    assert_eq!(tokens, expected);
}

#[test]
fn dockerfile_leaves_entrypoint_cmd_and_user_to_the_image() {
    let dockerfile = kit_file(DOCKERFILE);
    for line in instruction_lines(&dockerfile) {
        let instruction = line.split_whitespace().next().unwrap().to_ascii_uppercase();
        assert!(
            !["ENTRYPOINT", "CMD", "USER"].contains(&instruction.as_str()),
            "{line}"
        );
    }
}

#[test]
fn kit_yaml_version_tracks_crate_version() {
    let yaml = kit_file(YAML);
    assert_eq!(yaml.lines().next(), Some("# syntax=docker/sandbox-kit:3"));
    let doc: Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        doc.get("version").and_then(Value::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let quoted = format!("\nversion: \"{}\"\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(yaml.matches(&quoted).count(), 1, "{yaml}");
}

#[test]
fn release_workflow_bumps_both_kit_files() {
    // Cargo.toml excludes .github from the published crate; the packaged
    // tests must not panic on a tree that never had the workflow.
    if !repo_path(".github").is_dir() {
        eprintln!("skipping: .github is not part of this source tree");
        return;
    }
    let workflow = read(".github/workflows/release.yaml");
    let dockerfile_path = format!("{KIT_DIR}/{DOCKERFILE}");
    let yaml_path = format!("{KIT_DIR}/{YAML}");
    assert!(!workflow.contains("spec.yaml"));

    let sed_lines: Vec<&str> = workflow
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("sed -i "))
        .collect();
    assert_eq!(sed_lines.len(), 2, "{workflow}");
    for (pattern, target) in [
        ("s|^FROM darkalex17/coyote:v[^ ]*|", &dockerfile_path),
        ("s|^version: .*|", &yaml_path),
    ] {
        let line = sed_lines
            .iter()
            .find(|line| line.contains(pattern))
            .unwrap_or_else(|| panic!("sed {pattern} missing:\n{workflow}"));
        assert!(line.ends_with(target.as_str()), "{line}");
    }

    for prefix in [
        "git diff --name-only --",
        "git diff --quiet --",
        "git add -u --",
    ] {
        let lines: Vec<&str> = workflow
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with(prefix) || line.starts_with(&format!("if ! {prefix}")))
            .collect();
        assert_eq!(lines.len(), 1, "{prefix}:\n{workflow}");
        assert!(lines[0].contains(&dockerfile_path), "{}", lines[0]);
        assert!(lines[0].contains(&yaml_path), "{}", lines[0]);
    }
}

#[test]
fn gitattributes_force_lf_for_kit_assets() {
    let attributes = read(".gitattributes");
    let lines: Vec<&str> = attributes.lines().map(str::trim).collect();
    for row in [
        "assets/sbx-kit/** text eol=lf",
        "assets/functions/kits/** text eol=lf",
        "assets/agents/** text eol=lf",
    ] {
        assert!(lines.contains(&row), "{row} missing:\n{attributes}");
    }
}

#[test]
fn asset_tree_holds_exactly_the_base_kit() {
    let root = repo_path("assets/sbx-kit");
    assert_eq!(tracked_names(&root), BTreeSet::from(["coyote".to_string()]));
    assert!(root.join("coyote").is_dir());

    assert_eq!(
        tracked_names(&root.join("coyote")),
        BTreeSet::from([YAML.to_string(), DOCKERFILE.to_string()])
    );
}

#[test]
fn asset_files_are_ascii_with_lf_line_endings() {
    let shipped = [
        format!("{KIT_DIR}/{YAML}"),
        format!("{KIT_DIR}/{DOCKERFILE}"),
    ];
    for name in shipped.iter().map(String::as_str).chain(MIXIN_KIT_FILES) {
        let text = read(name);
        assert!(text.is_ascii(), "{name} holds non-ASCII bytes");
        assert!(!text.contains('\r'), "{name} holds CR bytes");
    }
}

#[test]
fn no_v2_mixin_file_survives_under_assets() {
    let leftovers: Vec<String> = walk(&repo_path("assets"))
        .iter()
        .filter(|path| file_name(path) == "sbx-mixin.yaml")
        .map(|path| relative_to_repo(path))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let specs: Vec<String> = walk(&repo_path("assets/sbx-kit"))
        .iter()
        .filter(|path| file_name(path) == "spec.yaml")
        .map(|path| relative_to_repo(path))
        .collect();
    assert!(specs.is_empty(), "{specs:?}");
}

#[test]
fn no_asset_declares_schema_version_two() {
    let offenders: Vec<String> = walk(&repo_path("assets"))
        .iter()
        .filter(|path| {
            let bytes = fs::read(path).unwrap();
            std::str::from_utf8(&bytes).is_ok_and(|text| {
                text.contains("schemaVersion: '2'") || text.contains("schemaVersion: \"2\"")
            })
        })
        .map(|path| relative_to_repo(path))
        .collect();
    assert!(offenders.is_empty(), "{offenders:?}");
}

#[test]
fn kit_file_set_is_exactly_the_shipped_kits_without_scheme() {
    let files = kit_file_set();
    let expected: BTreeSet<PathBuf> = MIXIN_KIT_FILES
        .iter()
        .map(|name| repo_path(name))
        .chain([
            repo_path(&format!("{KIT_DIR}/{YAML}")),
            repo_path(&format!("{KIT_DIR}/{DOCKERFILE}")),
        ])
        .collect();
    assert_eq!(files, expected);
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("scheme:"), "{}", path.display());
    }
}

#[test]
fn plain_http_allow_entries_are_exactly_the_known_ones() {
    let mut found = BTreeSet::new();
    for path in kit_file_set() {
        if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
            continue;
        }
        let doc: Value = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let capabilities = doc
            .get("capabilities")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .filter(|cap| {
                cap.get("type").and_then(Value::as_str)
                    == Some("com.docker.sandbox/network-policy@1")
            });
        for policy in capabilities {
            let Some(config) = policy.get("config") else {
                continue;
            };
            for phase in ["install", "runtime"] {
                for entry in string_allow_entries(config, phase) {
                    if entry.ends_with(":80") {
                        found.insert((relative_to_repo(&path), phase.to_string(), entry));
                    }
                }
            }
        }
    }
    let expected: BTreeSet<(String, String, String)> = [
        (
            "assets/functions/kits/built-in-tools/built-in-tools.yaml",
            "runtime",
            "export.arxiv.org:80",
        ),
        (
            "assets/agents/probe/kits/agent-probe/agent-probe.yaml",
            "install",
            "archive.ubuntu.com:80",
        ),
        (
            "assets/agents/probe/kits/agent-probe/agent-probe.yaml",
            "install",
            "security.ubuntu.com:80",
        ),
        (
            "assets/agents/probe/kits/agent-probe/agent-probe.yaml",
            "install",
            "ports.ubuntu.com:80",
        ),
    ]
    .into_iter()
    .map(|(file, phase, entry)| (file.to_string(), phase.to_string(), entry.to_string()))
    .collect();
    assert_eq!(found, expected);
}

const BUILT_IN_TOOLS_V2_HOSTS: [&str; 19] = [
    "r.jina.ai",
    "wttr.in",
    "export.arxiv.org",
    "export.arxiv.org:80",
    "doi.org",
    "en.wikipedia.org",
    "api.wolframalpha.com",
    "api.perplexity.ai",
    "api.tavily.com",
    "api.twilio.com",
    "api.githubcopilot.com",
    "mcp.atlassian.com",
    "duckduckgo.com",
    "html.duckduckgo.com",
    "lite.duckduckgo.com",
    "registry.npmjs.org",
    "ghcr.io",
    "registry-1.docker.io",
    "auth.docker.io",
];

const SISYPHUS_DDG_V2_HOSTS: [&str; 16] = [
    "duckduckgo.com",
    "html.duckduckgo.com",
    "lite.duckduckgo.com",
    "*.wikipedia.org",
    "github.com",
    "*.githubusercontent.com",
    "stackoverflow.com",
    "*.stackexchange.com",
    "developer.mozilla.org",
    "docs.python.org",
    "doc.rust-lang.org",
    "docs.rs",
    "crates.io",
    "pypi.org",
    "www.npmjs.com",
    "r.jina.ai",
];

const SISYPHUS_DDG_V2_INSTRUCTIONS: &str = "Web search runs against an enumerated network allow list. If fetching a\n\
search result is blocked by network policy, ask the user to run\n\
`sbx policy allow network <domain>` on the host to extend it.\n";

const PROBE_RELEASE_HOSTS: [&str; 4] = [
    "api.github.com:443",
    "github.com:443",
    "objects.githubusercontent.com:443",
    "release-assets.githubusercontent.com:443",
];

const PROBE_APT_MIRRORS: [&str; 6] = [
    "archive.ubuntu.com",
    "archive.ubuntu.com:80",
    "security.ubuntu.com",
    "security.ubuntu.com:80",
    "ports.ubuntu.com",
    "ports.ubuntu.com:80",
];

const PROBE_V2_HOOK_DESCRIPTIONS: [&str; 2] = [
    "Install Hurl (distro package preferred, GitHub tarball fallback) for the probe agent's HTTP usage-pattern tests",
    "Install grpcurl (static GitHub release binary) for the probe agent's gRPC probes",
];

fn mixin_doc(relative: &str) -> Value {
    serde_yaml::from_str(&read(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

fn capability<'a>(doc: &'a Value, ty: &str) -> &'a Value {
    let matches: Vec<&Value> = doc
        .get("capabilities")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter(|cap| cap.get("type").and_then(Value::as_str) == Some(ty))
        .collect();
    assert_eq!(matches.len(), 1, "expected exactly one {ty}");
    matches[0]
}

fn network_allow(doc: &Value, phase: &str) -> Vec<String> {
    string_allow_entries(
        capability(doc, "com.docker.sandbox/network-policy@1")
            .get("config")
            .unwrap(),
        phase,
    )
}

fn lifecycle_install_hooks(doc: &Value) -> Vec<Value> {
    capability(doc, "com.docker.sandbox/lifecycle@1")
        .get("config")
        .and_then(|c| c.get("install"))
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default()
}

/// The v3 rewrite changes the grammar, not the policy: every host list and
/// the instructions body are the v2 predecessor's, in the v2 order, and the
/// probe kit's install phase is exactly the release hosts followed by the apt
/// mirrors it newly needs.
#[test]
fn usage_probe_mixin_host_lists_are_the_v2_lists_verbatim() {
    let built_in_tools = mixin_doc(MIXIN_KIT_FILES[0]);
    assert_eq!(
        network_allow(&built_in_tools, "runtime"),
        BUILT_IN_TOOLS_V2_HOSTS
    );
    assert!(network_allow(&built_in_tools, "install").is_empty());

    let sisyphus_ddg = mixin_doc(MIXIN_KIT_FILES[1]);
    assert_eq!(
        network_allow(&sisyphus_ddg, "runtime"),
        SISYPHUS_DDG_V2_HOSTS
    );
    assert!(network_allow(&sisyphus_ddg, "install").is_empty());
    let context = capability(&sisyphus_ddg, "com.docker.sandbox/agent-context@1")
        .get("config")
        .unwrap();
    assert_eq!(
        context.get("content").and_then(Value::as_str),
        Some(SISYPHUS_DDG_V2_INSTRUCTIONS)
    );

    let agent_probe = mixin_doc(MIXIN_KIT_FILES[2]);
    let expected_install: Vec<String> = PROBE_RELEASE_HOSTS
        .iter()
        .chain(&PROBE_APT_MIRRORS)
        .map(|host| host.to_string())
        .collect();
    assert_eq!(network_allow(&agent_probe, "install"), expected_install);
    assert_eq!(network_allow(&agent_probe, "runtime"), PROBE_RELEASE_HOSTS);
    let hooks = lifecycle_install_hooks(&agent_probe);
    let descriptions: Vec<&str> = hooks
        .iter()
        .map(|hook| hook.get("description").and_then(Value::as_str).unwrap())
        .collect();
    assert_eq!(descriptions, PROBE_V2_HOOK_DESCRIPTIONS);
}

/// The probe kit's description promises its hooks run under `/bin/sh` (dash);
/// every install hook body must at least parse as POSIX shell on this host.
#[cfg(unix)]
#[test]
fn usage_probe_mixin_install_hooks_parse_as_posix_sh() {
    let agent_probe = mixin_doc(MIXIN_KIT_FILES[2]);
    let hooks = lifecycle_install_hooks(&agent_probe);
    assert_eq!(hooks.len(), 2);
    for hook in &hooks {
        let command = hook.get("command").and_then(Value::as_str).unwrap();
        assert!(command.starts_with("set -eu\n"), "{command}");
        let output = std::process::Command::new("sh")
            .arg("-n")
            .arg("-c")
            .arg(command)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "sh -n rejected the hook:\n{}\n{command}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The probe README links the kit file that ships and never the retired
/// `sbx-mixin.yaml` name.
#[test]
fn usage_probe_probe_readme_links_the_kit_file_that_ships() {
    let readme = read("assets/agents/probe/README.md");
    let link = "[`kits/agent-probe/agent-probe.yaml`](kits/agent-probe/agent-probe.yaml)";
    assert!(readme.contains(link), "README does not link the kit file");
    assert!(
        repo_path("assets/agents/probe/kits/agent-probe/agent-probe.yaml").is_file(),
        "README link target is missing"
    );
    assert!(
        !readme.contains("sbx-mixin.yaml"),
        "README still names the removed v2 file"
    );
}
