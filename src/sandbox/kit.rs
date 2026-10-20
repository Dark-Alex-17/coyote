use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::Value;

pub(crate) const SYNTAX_LINE: &str = "# syntax=docker/sandbox-kit:3";
pub(crate) const SCHEMA_VERSION: &str = "3";
pub(crate) const KIND_WORKLOAD: &str = "workload";
pub(crate) const KIND_MIXIN: &str = "mixin";
pub(crate) const CAP_NETWORK_POLICY: &str = "com.docker.sandbox/network-policy@1";
pub(crate) const CAP_CREDENTIAL: &str = "com.docker.sandbox/credential@1";
pub(crate) const CAP_LIFECYCLE: &str = "com.docker.sandbox/lifecycle@1";
pub(crate) const CAP_AGENT_CONTEXT: &str = "com.docker.sandbox/agent-context@1";
pub(crate) const CAP_SBX: &str = "com.docker.sandbox/sbx@1";
pub(crate) const RESERVED_HANDLES: [&str; 2] = ["coyote", "coyote-mcp"];

const PHASE_INSTALL: &str = "install";
pub(crate) const PHASE_RUNTIME: &str = "runtime";
const LIFECYCLE_INSTALL_KEY: &str = "install";
const FIRST_LINE_DESCRIBE_LIMIT: usize = 80;
const V2_KEYS: [&str; 13] = [
    "name",
    "sandbox",
    "permissions",
    "credentials",
    "setup",
    "agentInstructions",
    "environment",
    "extends",
    "mixins",
    "locked",
    "volumes",
    "ports",
    "security",
];

pub(crate) fn is_reserved_handle(handle: &str) -> bool {
    RESERVED_HANDLES.contains(&handle)
}

#[derive(Debug, Clone)]
pub(crate) struct KitDir {
    pub(crate) dir: PathBuf,
    pub(crate) handle: String,
    pub(crate) doc: Value,
    pub(crate) dockerfile: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct KitSummary {
    pub(crate) install_hooks: usize,
    pub(crate) runtime_allow: usize,
    pub(crate) install_allow: usize,
}

pub(crate) fn load(dir: &Path) -> Result<KitDir> {
    if !dir.exists() {
        bail!("kit dir '{}' does not exist", dir.display());
    }
    if !dir.is_dir() {
        bail!("kit dir '{}' is not a directory", dir.display());
    }
    let handle = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("kit dir '{}' has no UTF-8 name", dir.display()))?
        .to_string();

    let mut yaml_names = Vec::new();
    let entries =
        fs::read_dir(dir).with_context(|| format!("Failed to read kit dir {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("Failed to read kit dir {}", dir.display()))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml")
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            yaml_names.push(name.to_string());
        }
    }
    yaml_names.sort();
    let yaml_name = format!("{handle}.yaml");
    match yaml_names.as_slice() {
        [only] if *only == yaml_name => {}
        _ => {
            let found = if yaml_names.is_empty() {
                "none".to_string()
            } else {
                yaml_names.join(", ")
            };
            bail!(
                "kit dir '{}' must hold exactly one '{yaml_name}' (found: {found})",
                dir.display()
            );
        }
    }

    let yaml_path = dir.join(&yaml_name);
    let content = fs::read_to_string(&yaml_path)
        .with_context(|| format!("Failed to read kit file {}", yaml_path.display()))?;
    let first_line = content.split('\n').next().unwrap_or("");
    let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
    if first_line != SYNTAX_LINE {
        bail!(
            "kit file {} first line must be '{SYNTAX_LINE}' (found: '{}')",
            yaml_path.display(),
            describe_first_line(first_line)
        );
    }

    let doc: Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse kit file {}", yaml_path.display()))?;
    if !doc.is_mapping() {
        bail!("kit file {} root must be a mapping", yaml_path.display());
    }
    if doc.get("schemaVersion").and_then(Value::as_str) != Some(SCHEMA_VERSION) {
        bail!(
            "kit dir '{}' schemaVersion must be the string \"{SCHEMA_VERSION}\"",
            dir.display()
        );
    }
    let kind = doc.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != KIND_WORKLOAD && kind != KIND_MIXIN {
        bail!(
            "kit dir '{}' has kind '{kind}'; expected '{KIND_WORKLOAD}' or '{KIND_MIXIN}'",
            dir.display()
        );
    }
    if let Some(key) = V2_KEYS.iter().find(|key| doc.get(**key).is_some()) {
        bail!(
            "kit dir '{}': v2 key '{key}' is not valid in a v3 kit",
            dir.display()
        );
    }

    let dockerfile = dir.join(dockerfile_name(&handle));
    let dockerfile = dockerfile.is_file().then_some(dockerfile);

    Ok(KitDir {
        dir: dir.to_path_buf(),
        handle,
        doc,
        dockerfile,
    })
}

impl KitDir {
    pub(crate) fn summary(&self) -> KitSummary {
        let mut out = KitSummary::default();
        for entry in self.capabilities() {
            let config = entry.get("config");
            match cap_type(entry) {
                Some(CAP_LIFECYCLE) => {
                    out.install_hooks += seq_len(config.and_then(|c| c.get(LIFECYCLE_INSTALL_KEY)));
                }
                Some(CAP_NETWORK_POLICY) => {
                    out.runtime_allow += seq_len(allow_list(config, PHASE_RUNTIME));
                    out.install_allow += seq_len(allow_list(config, PHASE_INSTALL));
                }
                _ => {}
            }
        }
        out
    }

    pub(crate) fn check_mixin(&self) -> Result<()> {
        let dir = self.dir.display();
        if self.kind() != KIND_MIXIN {
            bail!(
                "kit dir '{dir}' has kind '{}'; a mixin kit must have kind '{KIND_MIXIN}'",
                self.kind()
            );
        }

        let mut runtime_allow = Vec::new();
        let mut install_allow = Vec::new();
        for entry in self.capabilities() {
            let config = entry.get("config");
            match cap_type(entry) {
                Some(CAP_SBX) => {
                    bail!("kit dir '{dir}' declares '{CAP_SBX}', which a mixin kit may not")
                }
                Some(CAP_LIFECYCLE) if config.and_then(|c| c.get("interactive")).is_some() => {
                    bail!(
                        "kit dir '{dir}' lifecycle capability sets 'interactive', which a mixin kit may not"
                    )
                }
                Some(CAP_AGENT_CONTEXT)
                    if config.and_then(|c| c.get("filename")).is_some()
                        && config.and_then(|c| c.get("directory")).is_none() =>
                {
                    bail!(
                        "kit dir '{dir}' agent-context capability sets 'filename' without 'directory'"
                    )
                }
                Some(CAP_NETWORK_POLICY) => {
                    runtime_allow.extend(string_items(allow_list(config, PHASE_RUNTIME)));
                    install_allow.extend(string_items(allow_list(config, PHASE_INSTALL)));
                }
                _ => {}
            }
        }

        for entry in self
            .capabilities()
            .filter(|entry| cap_type(entry) == Some(CAP_CREDENTIAL))
        {
            let config = entry.get("config");
            let phases = self.credential_phases(config)?;
            let injects = config
                .and_then(|c| c.get("apiKey"))
                .and_then(|k| k.get("inject"))
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten();
            for inject in injects {
                let Some(domain) = inject.get("domain").and_then(Value::as_str) else {
                    bail!("kit dir '{dir}' credential inject entry must have a string domain");
                };
                for phase in &phases {
                    let allow = if phase == PHASE_INSTALL {
                        &install_allow
                    } else {
                        &runtime_allow
                    };
                    if !allow.iter().any(|allowed| allowed == domain) {
                        bail!(
                            "kit dir '{dir}' credential inject domain '{domain}' is missing from the {phase} allow list"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn check_workload(&self) -> Result<()> {
        let dir = self.dir.display();
        if self.kind() != KIND_WORKLOAD {
            bail!(
                "kit dir '{dir}' has kind '{}'; a workload kit must have kind '{KIND_WORKLOAD}'",
                self.kind()
            );
        }
        if self.dockerfile.is_none() {
            bail!(
                "kit dir '{dir}' is missing '{}', which a workload kit requires",
                dockerfile_name(&self.handle)
            );
        }
        Ok(())
    }

    fn kind(&self) -> &str {
        self.doc.get("kind").and_then(Value::as_str).unwrap_or("")
    }

    fn capabilities(&self) -> impl Iterator<Item = &Value> {
        self.doc
            .get("capabilities")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
    }

    /// A credential with no `phase` is injected at runtime only.
    fn credential_phases(&self, config: Option<&Value>) -> Result<Vec<String>> {
        let Some(phase) = config.and_then(|c| c.get("phase")) else {
            return Ok(vec![PHASE_RUNTIME.to_string()]);
        };
        let phases = match phase {
            Value::String(name) => vec![name.clone()],
            Value::Sequence(items) if !items.is_empty() => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow!(
                            "kit dir '{}' credential phase must be a string or a sequence of strings",
                            self.dir.display()
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            _ => bail!(
                "kit dir '{}' credential phase must be a string or a sequence of strings",
                self.dir.display()
            ),
        };
        if let Some(name) = phases
            .iter()
            .find(|name| *name != PHASE_INSTALL && *name != PHASE_RUNTIME)
        {
            bail!(
                "kit dir '{}' credential phase '{name}' is not '{PHASE_INSTALL}' or '{PHASE_RUNTIME}'",
                self.dir.display()
            );
        }
        Ok(phases)
    }
}

fn dockerfile_name(handle: &str) -> String {
    format!("{handle}.dockerfile")
}

/// The first line comes straight from an operator-authored file, so it is
/// bounded and reduced to printable ASCII before reaching the terminal: a
/// byte-order mark or a bidi override must not render as the expected line.
fn describe_first_line(line: &str) -> String {
    let mut out: String = line
        .chars()
        .take(FIRST_LINE_DESCRIBE_LIMIT)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    if line.chars().nth(FIRST_LINE_DESCRIBE_LIMIT).is_some() {
        out.push_str("...");
    }
    out
}

fn cap_type(entry: &Value) -> Option<&str> {
    entry.get("type").and_then(Value::as_str)
}

fn allow_list<'a>(config: Option<&'a Value>, phase: &str) -> Option<&'a Value> {
    config
        .and_then(|c| c.get(phase))
        .and_then(|p| p.get("allow"))
}

fn seq_len(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_sequence)
        .map_or(0, |seq| seq.len())
}

fn string_items(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const VALID_MIXIN: &str = r#"# syntax=docker/sandbox-kit:3
schemaVersion: "3"
kind: mixin
description: fixture
capabilities:
  - type: com.docker.sandbox/network-policy@1
    config:
      runtime:
        allow: [api.example.com, "*.example.org", pypi.org]
      install:
        allow: [deb.debian.org]
  - type: com.docker.sandbox/credential@1
    optional: true
    config:
      service: example
      phase: runtime
      apiKey:
        name: EXAMPLE_KEY
        proxyManaged: true
        inject:
          - domain: api.example.com
            header: Authorization
            format: Bearer %s
  - type: com.docker.sandbox/lifecycle@1
    config:
      install:
        - command: echo one
        - command: echo two
      startup:
        - command: echo up
"#;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            // The clock alone can collide between parallel tests sharing a label.
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!("coyote-kit-{label}-{nanos}-{seq}"));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn kit_dir(root: &Path, handle: &str, yaml: &str) -> PathBuf {
        let dir = root.join(handle);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{handle}.yaml")), yaml).unwrap();
        dir
    }

    fn minimal(kind: &str) -> String {
        format!("{SYNTAX_LINE}\nschemaVersion: \"3\"\nkind: {kind}\n")
    }

    fn load_err(dir: &Path) -> String {
        format!("{:#}", load(dir).unwrap_err())
    }

    fn mixin_err(yaml: &str) -> String {
        let root = TempRoot::new("mixin-err");
        let dir = kit_dir(root.path(), "h", yaml);
        format!("{:#}", load(&dir).unwrap().check_mixin().unwrap_err())
    }

    #[test]
    fn load_accepts_minimal_mixin() {
        let root = TempRoot::new("minimal");
        let dir = kit_dir(root.path(), "h", &minimal("mixin"));
        let kit = load(&dir).unwrap();
        assert_eq!(kit.handle, "h");
        assert_eq!(kit.dir, root.path().join("h"));
        assert_eq!(kit.dockerfile, None);
    }

    #[test]
    fn load_rejects_yaml_stem_not_matching_dir_name() {
        let root = TempRoot::new("stem");
        let dir = root.path().join("h");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("other.yaml"), minimal("mixin")).unwrap();
        let err = load_err(&dir);
        assert!(err.contains("must hold exactly one 'h.yaml'"), "{err}");
        assert!(err.contains("found: other.yaml"), "{err}");
    }

    #[test]
    fn load_rejects_two_yaml_files() {
        let root = TempRoot::new("two");
        let dir = kit_dir(root.path(), "h", &minimal("mixin"));
        fs::write(dir.join("extra.yaml"), minimal("mixin")).unwrap();
        let err = load_err(&dir);
        assert!(err.contains("must hold exactly one 'h.yaml'"), "{err}");
        assert!(err.contains("found: extra.yaml, h.yaml"), "{err}");
    }

    #[test]
    fn load_rejects_no_yaml_file() {
        let root = TempRoot::new("none");
        let dir = root.path().join("h");
        fs::create_dir_all(&dir).unwrap();
        let err = load_err(&dir);
        assert!(err.contains("must hold exactly one 'h.yaml'"), "{err}");
        assert!(err.contains("found: none"), "{err}");
    }

    #[test]
    fn load_rejects_missing_syntax_line() {
        let root = TempRoot::new("no-syntax");
        let dir = kit_dir(root.path(), "h", "schemaVersion: \"3\"\nkind: mixin\n");
        let err = load_err(&dir);
        assert!(err.contains("first line"), "{err}");
        assert!(err.contains("# syntax=docker/sandbox-kit:3"), "{err}");
    }

    #[test]
    fn load_rejects_wrong_syntax_line() {
        let root = TempRoot::new("wrong-syntax");
        let yaml = "# syntax=docker/sandbox-kit:2\nschemaVersion: \"3\"\nkind: mixin\n";
        let dir = kit_dir(root.path(), "h", yaml);
        let err = load_err(&dir);
        assert!(err.contains("first line"), "{err}");
        assert!(err.contains("# syntax=docker/sandbox-kit:3"), "{err}");
    }

    #[test]
    fn load_sanitizes_echoed_first_line() {
        let root = TempRoot::new("hostile-first-line");
        let yaml = format!(
            "{}\x1b[31m\nschemaVersion: \"3\"\nkind: mixin\n",
            "x".repeat(500)
        );
        let dir = kit_dir(root.path(), "h", &yaml);
        let err = load_err(&dir);
        assert!(err.contains("first line"), "{err}");
        assert!(!err.contains('\x1b'), "{err}");
        assert!(!err.contains(&"x".repeat(100)), "{err}");
        assert!(err.contains(&format!("{}...", "x".repeat(80))), "{err}");
        assert_eq!(describe_first_line("a\x1bb\tc\n"), "a?b?c?");
        assert_eq!(describe_first_line(SYNTAX_LINE), SYNTAX_LINE);
        assert_eq!(
            describe_first_line(&format!("\u{feff}{SYNTAX_LINE}")),
            format!("?{SYNTAX_LINE}")
        );
        assert_eq!(describe_first_line(&"x".repeat(80)), "x".repeat(80));
    }

    #[test]
    fn load_accepts_crlf_line_endings() {
        let root = TempRoot::new("crlf");
        let whole_file = format!("{SYNTAX_LINE}\r\nschemaVersion: \"3\"\r\nkind: mixin\r\n");
        let dir = kit_dir(root.path(), "whole", &whole_file);
        assert_eq!(load(&dir).unwrap().handle, "whole");

        let first_line_only = format!("{SYNTAX_LINE}\r\nschemaVersion: \"3\"\nkind: mixin\n");
        let dir = kit_dir(root.path(), "first", &first_line_only);
        assert_eq!(load(&dir).unwrap().handle, "first");
    }

    #[test]
    fn load_rejects_integer_schema_version() {
        let root = TempRoot::new("int-schema");
        let dir = kit_dir(
            root.path(),
            "h",
            &format!("{SYNTAX_LINE}\nschemaVersion: 3\nkind: mixin\n"),
        );
        let err = load_err(&dir);
        assert!(err.contains("schemaVersion"), "{err}");
        assert!(err.contains("\"3\""), "{err}");
    }

    #[test]
    fn load_rejects_schema_version_two() {
        let root = TempRoot::new("v2-schema");
        let dir = kit_dir(
            root.path(),
            "h",
            &format!("{SYNTAX_LINE}\nschemaVersion: \"2\"\nkind: mixin\n"),
        );
        let err = load_err(&dir);
        assert!(err.contains("schemaVersion"), "{err}");
        assert!(err.contains("\"3\""), "{err}");
    }

    #[test]
    fn load_rejects_unknown_kind() {
        let root = TempRoot::new("kind");
        let dir = kit_dir(root.path(), "h", &minimal("sandbox"));
        let err = load_err(&dir);
        assert!(err.contains("kind 'sandbox'"), "{err}");
    }

    #[test]
    fn load_rejects_each_v2_key() {
        let root = TempRoot::new("v2-keys");
        for key in V2_KEYS {
            let yaml = format!("{}{key}: x\n", minimal("mixin"));
            let dir = kit_dir(root.path(), key, &yaml);
            let err = load_err(&dir);
            assert!(
                err.contains(&format!("v2 key '{key}' is not valid in a v3 kit")),
                "{err}"
            );
        }
    }

    #[test]
    fn load_rejects_nonexistent_dir() {
        let root = TempRoot::new("missing");
        let dir = root.path().join("h");
        let err = load_err(&dir);
        assert!(
            err.contains(&format!("kit dir '{}'", dir.display())),
            "{err}"
        );
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn summary_counts_install_hooks_and_allow_lists() {
        let root = TempRoot::new("summary");
        let dir = kit_dir(root.path(), "h", VALID_MIXIN);
        assert_eq!(
            load(&dir).unwrap().summary(),
            KitSummary {
                install_hooks: 2,
                runtime_allow: 3,
                install_allow: 1,
            }
        );
    }

    #[test]
    fn summary_without_capabilities_is_zero() {
        let root = TempRoot::new("summary-empty");
        let dir = kit_dir(root.path(), "h", &minimal("mixin"));
        assert_eq!(load(&dir).unwrap().summary(), KitSummary::default());
    }

    #[test]
    fn check_mixin_rejects_workload_kind() {
        let err = mixin_err(&minimal("workload"));
        assert!(err.contains("kind 'workload'"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_sbx_capability() {
        let yaml = format!(
            "{}capabilities:\n  - type: com.docker.sandbox/sbx@1\n",
            minimal("mixin")
        );
        let err = mixin_err(&yaml);
        assert!(err.contains("com.docker.sandbox/sbx@1"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_interactive_lifecycle() {
        let yaml = format!(
            "{}capabilities:\n  - type: com.docker.sandbox/lifecycle@1\n    config:\n      interactive: [\"bash\"]\n",
            minimal("mixin")
        );
        let err = mixin_err(&yaml);
        assert!(err.contains("interactive"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_agent_context_filename_without_directory() {
        let yaml = format!(
            "{}capabilities:\n  - type: com.docker.sandbox/agent-context@1\n    config:\n      filename: AGENTS.md\n      content: hi\n",
            minimal("mixin")
        );
        let err = mixin_err(&yaml);
        assert!(err.contains("filename"), "{err}");
        assert!(err.contains("directory"), "{err}");
    }

    #[test]
    fn check_mixin_accepts_agent_context_with_filename_and_directory() {
        let root = TempRoot::new("agent-context-ok");
        let yaml = format!(
            "{}capabilities:\n  - type: com.docker.sandbox/agent-context@1\n    config:\n      filename: AGENTS.md\n      directory: /work\n      content: hi\n",
            minimal("mixin")
        );
        let dir = kit_dir(root.path(), "h", &yaml);
        load(&dir).unwrap().check_mixin().unwrap();
    }

    #[test]
    fn check_mixin_rejects_wildcard_allow_for_inject_domain() {
        let yaml = VALID_MIXIN.replace(
            "allow: [api.example.com, \"*.example.org\", pypi.org]",
            "allow: [\"*.example.com\"]",
        );
        let err = mixin_err(&yaml);
        assert!(err.contains("inject domain 'api.example.com'"), "{err}");
        assert!(err.contains("runtime"), "{err}");
        assert!(err.contains("allow list"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_inject_domain_missing_from_install_allow() {
        let yaml = VALID_MIXIN.replace("phase: runtime", "phase: [install, runtime]");
        let err = mixin_err(&yaml);
        assert!(err.contains("inject domain 'api.example.com'"), "{err}");
        assert!(err.contains("install"), "{err}");
        assert!(err.contains("allow list"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_unknown_phase() {
        let yaml = VALID_MIXIN.replace("phase: runtime", "phase: build");
        let err = mixin_err(&yaml);
        assert!(err.contains("phase 'build'"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_non_string_phase_item() {
        let yaml = VALID_MIXIN.replace("phase: runtime", "phase: [3]");
        let err = mixin_err(&yaml);
        assert!(err.contains("credential phase must be a string"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_empty_phase_list() {
        let yaml = VALID_MIXIN.replace("phase: runtime", "phase: []");
        let err = mixin_err(&yaml);
        assert!(err.contains("credential phase must be a string"), "{err}");
    }

    #[test]
    fn check_mixin_rejects_non_string_inject_domain() {
        let yaml = VALID_MIXIN.replace("- domain: api.example.com", "- domain: 443");
        let err = mixin_err(&yaml);
        assert!(
            err.contains("inject entry must have a string domain"),
            "{err}"
        );
    }

    #[test]
    fn check_mixin_rejects_inject_entry_without_domain() {
        let yaml = VALID_MIXIN.replace(
            "          - domain: api.example.com\n            header: Authorization",
            "          - header: Authorization",
        );
        let err = mixin_err(&yaml);
        assert!(
            err.contains("inject entry must have a string domain"),
            "{err}"
        );
    }

    #[test]
    fn check_mixin_accepts_valid_fixture() {
        let root = TempRoot::new("mixin-ok");
        let dir = kit_dir(root.path(), "h", VALID_MIXIN);
        load(&dir).unwrap().check_mixin().unwrap();
    }

    #[test]
    fn check_mixin_defaults_absent_phase_to_runtime() {
        let root = TempRoot::new("mixin-default-phase");
        let yaml = VALID_MIXIN.replace("      phase: runtime\n", "");
        assert!(!yaml.contains("phase"), "{yaml}");
        let dir = kit_dir(root.path(), "h", &yaml);
        load(&dir).unwrap().check_mixin().unwrap();
    }

    #[test]
    fn check_mixin_checks_default_phase_against_runtime_allow() {
        let yaml = VALID_MIXIN.replace("      phase: runtime\n", "").replace(
            "allow: [api.example.com, \"*.example.org\", pypi.org]",
            "allow: [\"*.example.com\"]",
        );
        let err = mixin_err(&yaml);
        assert!(err.contains("inject domain 'api.example.com'"), "{err}");
        assert!(err.contains("runtime"), "{err}");
    }

    #[test]
    fn check_workload_rejects_mixin_kind() {
        let root = TempRoot::new("workload-kind");
        let dir = kit_dir(root.path(), "h", &minimal("mixin"));
        fs::write(dir.join("h.dockerfile"), "FROM scratch\n").unwrap();
        let err = format!("{:#}", load(&dir).unwrap().check_workload().unwrap_err());
        assert!(err.contains("kind 'mixin'"), "{err}");
    }

    #[test]
    fn check_workload_rejects_missing_dockerfile() {
        let root = TempRoot::new("workload-no-dockerfile");
        let dir = kit_dir(root.path(), "h", &minimal("workload"));
        let err = format!("{:#}", load(&dir).unwrap().check_workload().unwrap_err());
        assert!(err.contains("h.dockerfile"), "{err}");
        assert!(
            err.contains(&format!("kit dir '{}'", dir.display())),
            "{err}"
        );
    }

    #[test]
    fn check_workload_ignores_other_dockerfile() {
        let root = TempRoot::new("workload-other-dockerfile");
        let dir = kit_dir(root.path(), "h", &minimal("workload"));
        fs::write(dir.join("other.dockerfile"), "FROM scratch\n").unwrap();
        let kit = load(&dir).unwrap();
        assert_eq!(kit.dockerfile, None);
        let err = format!("{:#}", kit.check_workload().unwrap_err());
        assert!(err.contains("h.dockerfile"), "{err}");
    }

    #[test]
    fn check_workload_accepts_handle_dockerfile() {
        let root = TempRoot::new("workload-ok");
        let dir = kit_dir(root.path(), "h", &minimal("workload"));
        fs::write(dir.join("h.dockerfile"), "FROM scratch\n").unwrap();
        let kit = load(&dir).unwrap();
        assert_eq!(kit.dockerfile, Some(dir.join("h.dockerfile")));
        kit.check_workload().unwrap();
    }

    #[test]
    fn is_reserved_handle_matches_only_coyote_handles() {
        assert!(is_reserved_handle("coyote"));
        assert!(is_reserved_handle("coyote-mcp"));
        assert!(!is_reserved_handle("rag-x"));
        assert!(!is_reserved_handle("rag"));
        assert!(!is_reserved_handle("tools"));
    }
}
