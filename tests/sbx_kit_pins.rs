//! Pins the shipped sandbox kit files and the release plumbing that rewrites
//! them: the dockerfile base image and the kit version both track the crate
//! version, the release workflow seds target the v3 files, and the asset tree
//! holds exactly the two files the embedder expects.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

const KIT_DIR: &str = "assets/sbx-kit/coyote";
const YAML: &str = "coyote.yaml";
const DOCKERFILE: &str = "coyote.dockerfile";

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
    for name in [YAML, DOCKERFILE] {
        let text = kit_file(name);
        assert!(text.is_ascii(), "{name} holds non-ASCII bytes");
        assert!(!text.contains('\r'), "{name} holds CR bytes");
    }
}
