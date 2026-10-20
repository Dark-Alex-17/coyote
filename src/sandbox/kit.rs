use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::Value;

use crate::config::BASE_KIT_HANDLE;
use crate::sandbox::mcp_credentials::MCP_MIXIN_NAME;

pub(crate) const SYNTAX_LINE: &str = "# syntax=docker/sandbox-kit:3";
pub(crate) const SCHEMA_VERSION: &str = "3";
pub(crate) const KIND_WORKLOAD: &str = "workload";
pub(crate) const KIND_MIXIN: &str = "mixin";
pub(crate) const CAP_NETWORK_POLICY: &str = "com.docker.sandbox/network-policy@1";
pub(crate) const CAP_CREDENTIAL: &str = "com.docker.sandbox/credential@1";
pub(crate) const CAP_LIFECYCLE: &str = "com.docker.sandbox/lifecycle@1";
pub(crate) const CAP_AGENT_CONTEXT: &str = "com.docker.sandbox/agent-context@1";
pub(crate) const CAP_SBX: &str = "com.docker.sandbox/sbx@1";
pub(crate) const RESERVED_HANDLES: [&str; 2] = [BASE_KIT_HANDLE, MCP_MIXIN_NAME];

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

/// Removes a `sbx-mixin.yaml` left behind by a release that shipped the
/// built-in mixins in the v2 grammar. Neither installer removes a file it no
/// longer ships at that path, so without this the old copy would sit next to
/// the v3 kit forever.
///
/// The file is deleted only when its content proves it is one of ours: a
/// parsable mapping with `schemaVersion: "2"`, `kind: mixin` and a `name`
/// from `shipped_names`. Anything else at that path is user-authored and is
/// left alone. Returns whether a file was removed.
pub(crate) fn remove_shipped_v2_mixin(path: &Path, shipped_names: &[&str]) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let Ok(doc) = serde_yaml::from_str::<Value>(&content) else {
        return Ok(false);
    };
    if !doc.is_mapping()
        || doc.get("schemaVersion").and_then(Value::as_str) != Some("2")
        || doc.get("kind").and_then(Value::as_str) != Some(KIND_MIXIN)
    {
        return Ok(false);
    }
    let name = doc.get("name").and_then(Value::as_str);
    if !name.is_some_and(|name| shipped_names.contains(&name)) {
        return Ok(false);
    }
    fs::remove_file(path).with_context(|| format!("Failed to remove {}", path.display()))?;
    info!(
        "Removed shipped v2 sbx mixin superseded by a v3 kit: {}",
        path.display()
    );
    Ok(true)
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

    const SHIPPED_V2: &str = "schemaVersion: '2'\nkind: mixin\nname: built-in-tools\npermissions:\n  network:\n    allow: [r.jina.ai]\n";

    fn v2_file(root: &Path, yaml: &str) -> PathBuf {
        let path = root.join("sbx-mixin.yaml");
        fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn remove_shipped_v2_mixin_removes_a_shipped_v2_file() {
        let root = TempRoot::new("v2-remove");
        let path = v2_file(root.path(), SHIPPED_V2);
        assert!(remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn remove_shipped_v2_mixin_leaves_a_v3_file() {
        let root = TempRoot::new("v2-leave-v3");
        let path = v2_file(root.path(), &SHIPPED_V2.replace("'2'", "\"3\""));
        assert!(!remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn remove_shipped_v2_mixin_leaves_an_unknown_name() {
        let root = TempRoot::new("v2-leave-name");
        let path = v2_file(root.path(), &SHIPPED_V2.replace("built-in-tools", "mine"));
        assert!(!remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn remove_shipped_v2_mixin_leaves_unparsable_yaml() {
        let root = TempRoot::new("v2-leave-bad");
        let path = v2_file(root.path(), "{");
        assert!(!remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn remove_shipped_v2_mixin_leaves_an_integer_name() {
        let root = TempRoot::new("v2-leave-int");
        let path = v2_file(root.path(), &SHIPPED_V2.replace("built-in-tools", "7"));
        assert!(!remove_shipped_v2_mixin(&path, &["7"]).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn remove_shipped_v2_mixin_ignores_a_missing_file() {
        let root = TempRoot::new("v2-missing");
        let path = root.path().join("sbx-mixin.yaml");
        assert!(!remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
    }

    #[test]
    fn usage_probe_remove_shipped_v2_mixin_leaves_a_non_mixin_kind() {
        let root = TempRoot::new("v2-leave-kind");
        let path = v2_file(
            root.path(),
            &SHIPPED_V2.replace("kind: mixin", "kind: workload"),
        );
        assert!(!remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn usage_probe_remove_shipped_v2_mixin_leaves_a_sequence_document() {
        let root = TempRoot::new("v2-leave-seq");
        let path = v2_file(
            root.path(),
            "- schemaVersion: '2'\n- kind: mixin\n- name: built-in-tools\n",
        );
        assert!(!remove_shipped_v2_mixin(&path, &["built-in-tools"]).unwrap());
        assert!(path.exists());
    }

    #[test]
    fn usage_probe_remove_shipped_v2_mixin_never_removes_a_directory() {
        let root = TempRoot::new("v2-leave-dir");
        let path = root.path().join("sbx-mixin.yaml");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("inner"), SHIPPED_V2).unwrap();
        let outcome = remove_shipped_v2_mixin(&path, &["built-in-tools"]);
        assert!(
            !matches!(outcome, Ok(true)),
            "a directory must never be reported as removed"
        );
        assert!(path.is_dir());
        assert!(path.join("inner").is_file());
    }

    /// An unreadable file is the one shape the callers cannot classify, so
    /// it must surface as an error (the installers warn and continue) rather
    /// than as a silent "nothing to do", and the file itself must survive.
    #[cfg(unix)]
    #[test]
    fn usage_probe_remove_shipped_v2_mixin_reports_an_unreadable_file_as_an_error() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads everything, so the refusal cannot be staged there.
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id -u");
        if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
            eprintln!("skipping: running as root, unreadable files still read");
            return;
        }
        let root = TempRoot::new("v2-unreadable");
        let path = v2_file(root.path(), SHIPPED_V2);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let outcome = remove_shipped_v2_mixin(&path, &["built-in-tools"]);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            outcome.is_err(),
            "an unreadable candidate must be reported, not swallowed: {outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            SHIPPED_V2,
            "the unreadable file must be left in place"
        );
    }
}

#[cfg(test)]
mod shipped_kits {
    use super::*;
    use std::collections::HashSet;

    const SERVICES: [&str; 21] = [
        "openai",
        "anthropic",
        "gemini",
        "cohere",
        "groq",
        "openrouter",
        "ai21",
        "cloudflare",
        "deepinfra",
        "deepseek",
        "mistral",
        "perplexity",
        "voyageai",
        "xai",
        "jina",
        "ernie",
        "hunyuan",
        "minimax",
        "moonshot",
        "qianwen",
        "zhipuai",
    ];

    const BUILT_IN_TOOLS_KIT: &str = "assets/functions/kits/built-in-tools";
    const SISYPHUS_DDG_KIT: &str = "assets/agents/sisyphus/kits/sisyphus-ddg";
    const AGENT_PROBE_KIT: &str = "assets/agents/probe/kits/agent-probe";
    const SHIPPED_MIXINS: [&str; 3] = [BUILT_IN_TOOLS_KIT, SISYPHUS_DDG_KIT, AGENT_PROBE_KIT];
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

    fn shipped_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/sbx-kit")
            .join(BASE_KIT_HANDLE)
    }

    fn shipped() -> KitDir {
        load(&shipped_dir()).unwrap()
    }

    fn shipped_mixin(relative: &str) -> KitDir {
        load(&Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap()
    }

    fn shipped_yaml_text(kit: &KitDir) -> String {
        fs::read_to_string(kit.dir.join(format!("{}.yaml", kit.handle))).unwrap()
    }

    fn caps_of<'a>(kit: &'a KitDir, ty: &str) -> Vec<&'a Value> {
        kit.capabilities()
            .filter(|entry| cap_type(entry) == Some(ty))
            .collect()
    }

    fn only<'a>(kit: &'a KitDir, ty: &str) -> &'a Value {
        let found = caps_of(kit, ty);
        assert_eq!(found.len(), 1, "expected exactly one {ty}");
        found[0]
    }

    fn config<'a>(entry: &'a Value, key: &str) -> &'a Value {
        entry
            .get("config")
            .and_then(|c| c.get(key))
            .unwrap_or_else(|| panic!("config.{key} missing from {entry:?}"))
    }

    fn runtime_allow(kit: &KitDir) -> Vec<String> {
        string_items(allow_list(
            only(kit, CAP_NETWORK_POLICY).get("config"),
            PHASE_RUNTIME,
        ))
    }

    fn install_allow(kit: &KitDir) -> Vec<String> {
        string_items(allow_list(
            only(kit, CAP_NETWORK_POLICY).get("config"),
            PHASE_INSTALL,
        ))
    }

    fn injects(credential: &Value) -> &[Value] {
        config(credential, "apiKey")
            .get("inject")
            .and_then(Value::as_sequence)
            .unwrap()
    }

    fn service(credential: &Value) -> &str {
        config(credential, "service").as_str().unwrap()
    }

    fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{key} must be a string in {value:?}"))
    }

    #[test]
    fn base_kit_loads_as_workload_with_dockerfile() {
        let kit = shipped();
        kit.check_workload().unwrap();
        assert_eq!(kit.handle, BASE_KIT_HANDLE);
        assert_eq!(
            kit.dockerfile,
            Some(shipped_dir().join(format!("{BASE_KIT_HANDLE}.dockerfile")))
        );

        let text = shipped_yaml_text(&kit);
        assert_eq!(text.lines().next(), Some(SYNTAX_LINE));
        assert_eq!(str_of(&kit.doc, "schemaVersion"), SCHEMA_VERSION);
        assert_eq!(str_of(&kit.doc, "kind"), KIND_WORKLOAD);
        assert_eq!(str_of(&kit.doc, "version"), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn base_kit_has_no_v2_root_keys() {
        let kit = shipped();
        for key in V2_KEYS {
            assert!(kit.doc.get(key).is_none(), "v2 key {key} present");
        }
    }

    #[test]
    fn base_kit_capabilities_are_ordered_and_counted() {
        let kit = shipped();
        let types: Vec<&str> = kit.capabilities().filter_map(cap_type).collect();
        let mut expected = vec![CAP_SBX, CAP_NETWORK_POLICY];
        expected.extend(std::iter::repeat_n(CAP_CREDENTIAL, SERVICES.len()));
        expected.extend([CAP_LIFECYCLE, CAP_AGENT_CONTEXT]);
        assert_eq!(types, expected);
        assert_eq!(types.len(), 25);
        assert!(only(&kit, CAP_SBX).get("config").is_none());
    }

    #[test]
    fn base_kit_network_policy_allows_runtime_only() {
        let kit = shipped();
        let policy = only(&kit, CAP_NETWORK_POLICY);
        let allow = config(policy, "runtime").get("allow").unwrap();
        assert_eq!(allow.as_sequence().unwrap().len(), 48);
        assert_eq!(runtime_allow(&kit).len(), 48);
        assert!(policy.get("config").unwrap().get("install").is_none());
    }

    #[test]
    fn base_kit_credentials_are_optional_proxy_managed_runtime() {
        let kit = shipped();
        let allow = runtime_allow(&kit);
        let credentials = caps_of(&kit, CAP_CREDENTIAL);
        let services: Vec<&str> = credentials.iter().map(|c| service(c)).collect();
        assert_eq!(services, SERVICES);

        for credential in &credentials {
            assert_eq!(credential.get("optional"), Some(&Value::Bool(true)));
            assert_eq!(config(credential, "phase").as_str(), Some(PHASE_RUNTIME));
            let api_key = config(credential, "apiKey");
            assert_eq!(api_key.get("proxyManaged"), Some(&Value::Bool(true)));
            assert!(!str_of(api_key, "name").is_empty());
            for inject in injects(credential) {
                let domain = str_of(inject, "domain");
                assert!(
                    allow.iter().any(|allowed| allowed == domain),
                    "{domain} missing from runtime allow"
                );
            }
        }
    }

    #[test]
    fn base_kit_inject_domains_are_distinct() {
        let kit = shipped();
        let mut domains = Vec::new();
        for credential in caps_of(&kit, CAP_CREDENTIAL) {
            let expected = if service(credential) == "jina" { 2 } else { 1 };
            assert_eq!(
                injects(credential).len(),
                expected,
                "{}",
                service(credential)
            );
            domains.extend(
                injects(credential)
                    .iter()
                    .map(|inject| str_of(inject, "domain").to_string()),
            );
        }
        assert_eq!(domains.len(), 22);
        assert_eq!(domains.iter().collect::<HashSet<_>>().len(), 22);
    }

    #[test]
    fn base_kit_inject_headers_use_bearer_except_anthropic_and_gemini() {
        let kit = shipped();
        for credential in caps_of(&kit, CAP_CREDENTIAL) {
            let (header, format) = match service(credential) {
                "anthropic" => ("x-api-key", "%s"),
                "gemini" => ("x-goog-api-key", "%s"),
                _ => ("Authorization", "Bearer %s"),
            };
            for inject in injects(credential) {
                assert_eq!(str_of(inject, "header"), header);
                assert_eq!(str_of(inject, "format"), format);
                assert!(inject.get("scheme").is_none());
            }
            assert!(
                !serde_yaml::to_string(credential)
                    .unwrap()
                    .contains("scheme"),
                "{}",
                service(credential)
            );
        }
    }

    #[test]
    fn base_kit_lifecycle_configures_signing_and_bootstraps_config() {
        let kit = shipped();
        let lifecycle = only(&kit, CAP_LIFECYCLE);

        let install = config(lifecycle, "install").as_sequence().unwrap();
        assert_eq!(install.len(), 1);
        assert_eq!(str_of(&install[0], "user"), "0");
        assert!(str_of(&install[0], "command").contains("git config --system gpg.format ssh"));

        let startup = config(lifecycle, "startup").as_sequence().unwrap();
        assert_eq!(startup.len(), 1);
        assert_eq!(str_of(&startup[0], "user"), "1000");
        assert_eq!(startup[0].get("background"), Some(&Value::Bool(false)));
        assert_eq!(
            string_items(startup[0].get("env")),
            ["HOME".to_string(), "PATH".to_string()]
        );
        let startup_command = str_of(&startup[0], "command");
        assert!(startup_command.contains("/home/agent/.cargo/bin/coyote --info"));
        let dockerfile = fs::read_to_string(kit.dockerfile.as_ref().unwrap()).unwrap();
        let config_dir = dockerfile
            .split_whitespace()
            .find_map(|token| token.strip_prefix("COYOTE_CONFIG_DIR="))
            .unwrap();
        let probed = format!(
            "test -f \"{}/config.yaml\"",
            config_dir.replacen("/home/agent", "$HOME", 1)
        );
        assert!(startup_command.contains(&probed), "{startup_command}");

        let files = config(lifecycle, "files").as_sequence().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            str_of(&files[0], "path"),
            "/home/agent/.config/git/ssh-signing-key-command"
        );
        assert_eq!(str_of(&files[0], "mode"), "0755");
        assert_eq!(files[0].get("overwrite"), Some(&Value::Bool(true)));
        assert!(!str_of(&files[0], "content").is_empty());
    }

    #[test]
    fn base_kit_agent_context_writes_coyote_md() {
        let kit = shipped();
        let context = only(&kit, CAP_AGENT_CONTEXT);
        assert_eq!(config(context, "filename").as_str(), Some("COYOTE.md"));
        assert!(!config(context, "content").as_str().unwrap().is_empty());
    }

    #[test]
    fn base_kit_summary_counts_hooks_and_allow_lists() {
        assert_eq!(
            shipped().summary(),
            KitSummary {
                install_hooks: 1,
                runtime_allow: 48,
                install_allow: 0,
            }
        );
    }

    #[test]
    fn shipped_mixins_load_and_pass_check_mixin() {
        for relative in SHIPPED_MIXINS {
            let kit = shipped_mixin(relative);
            kit.check_mixin().unwrap();
            assert_eq!(kit.dockerfile, None, "{relative}");

            let text = shipped_yaml_text(&kit);
            assert_eq!(text.lines().next(), Some(SYNTAX_LINE), "{relative}");
            assert!(!text.contains("scheme"), "{relative}");
            assert!(!text.contains(CAP_SBX), "{relative}");
            assert_eq!(str_of(&kit.doc, "schemaVersion"), SCHEMA_VERSION);
            assert_eq!(str_of(&kit.doc, "kind"), KIND_MIXIN);
            assert_eq!(str_of(&kit.doc, "author"), "Dark-Alex-17");
            assert_eq!(
                str_of(&kit.doc, "sourceUrl"),
                "https://github.com/Dark-Alex-17/coyote"
            );
            assert!(!str_of(&kit.doc, "displayName").is_empty());
            assert!(!str_of(&kit.doc, "description").is_empty());
            assert!(kit.doc.get("version").is_none(), "{relative}");
            for key in V2_KEYS {
                assert!(kit.doc.get(key).is_none(), "{relative}: v2 key {key}");
            }
        }
    }

    #[test]
    fn shipped_mixin_dirs_hold_exactly_the_handle_yaml() {
        for relative in SHIPPED_MIXINS {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
            let handle = dir.file_name().unwrap().to_str().unwrap();
            let names: Vec<String> = fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.') && !name.ends_with('~'))
                .collect();
            assert_eq!(names, [format!("{handle}.yaml")], "{relative}");
        }
    }

    #[test]
    fn built_in_tools_mixin_allows_runtime_hosts_only() {
        let kit = shipped_mixin(BUILT_IN_TOOLS_KIT);
        assert_eq!(kit.handle, "built-in-tools");
        let types: Vec<&str> = kit.capabilities().filter_map(cap_type).collect();
        assert_eq!(types, [CAP_NETWORK_POLICY]);

        let allow = runtime_allow(&kit);
        assert_eq!(allow.len(), 19);
        assert!(allow.contains(&"export.arxiv.org:80".to_string()));
        assert!(
            only(&kit, CAP_NETWORK_POLICY)
                .get("config")
                .unwrap()
                .get("install")
                .is_none()
        );
        assert_eq!(
            kit.summary(),
            KitSummary {
                install_hooks: 0,
                runtime_allow: 19,
                install_allow: 0,
            }
        );
    }

    #[test]
    fn sisyphus_ddg_mixin_allows_search_hosts_and_contributes_an_agent_context_body() {
        let kit = shipped_mixin(SISYPHUS_DDG_KIT);
        assert_eq!(kit.handle, "sisyphus-ddg");
        let types: Vec<&str> = kit.capabilities().filter_map(cap_type).collect();
        assert_eq!(types, [CAP_NETWORK_POLICY, CAP_AGENT_CONTEXT]);

        let allow = runtime_allow(&kit);
        assert_eq!(allow.len(), 16);
        assert!(allow.contains(&"duckduckgo.com".to_string()));
        assert!(allow.contains(&"r.jina.ai".to_string()));

        let context = only(&kit, CAP_AGENT_CONTEXT);
        let context_config = context.get("config").unwrap();
        assert!(context_config.get("filename").is_none());
        assert!(context_config.get("directory").is_none());
        assert!(str_of(context_config, "content").contains("sbx policy allow network"));
        assert_eq!(
            kit.summary(),
            KitSummary {
                install_hooks: 0,
                runtime_allow: 16,
                install_allow: 0,
            }
        );
    }

    #[test]
    fn agent_probe_mixin_installs_hurl_and_grpcurl() {
        let kit = shipped_mixin(AGENT_PROBE_KIT);
        assert_eq!(kit.handle, "agent-probe");
        let types: Vec<&str> = kit.capabilities().filter_map(cap_type).collect();
        assert_eq!(types, [CAP_NETWORK_POLICY, CAP_LIFECYCLE]);

        let install = install_allow(&kit);
        assert_eq!(install.len(), 10);
        for host in PROBE_RELEASE_HOSTS.iter().chain(&PROBE_APT_MIRRORS) {
            assert!(install.contains(&host.to_string()), "{host}");
        }
        assert!(!install.iter().any(|host| host.contains('*')));
        assert_eq!(runtime_allow(&kit), PROBE_RELEASE_HOSTS);

        let lifecycle = only(&kit, CAP_LIFECYCLE);
        let lifecycle_config = lifecycle.get("config").unwrap();
        for key in ["startup", "files", "interactive"] {
            assert!(lifecycle_config.get(key).is_none(), "{key}");
        }
        let hooks = config(lifecycle, "install").as_sequence().unwrap();
        assert_eq!(hooks.len(), 2);
        for (hook, version_probe) in hooks.iter().zip(["hurl --version", "grpcurl -version"]) {
            assert_eq!(str_of(hook, "user"), "1000");
            assert_eq!(
                string_items(hook.get("env")),
                ["HOME".to_string(), "PATH".to_string()]
            );
            assert!(str_of(hook, "command").contains(version_probe));
            assert!(!str_of(hook, "description").is_empty());
        }
        assert_eq!(
            kit.summary(),
            KitSummary {
                install_hooks: 2,
                runtime_allow: 4,
                install_allow: 10,
            }
        );
    }
}
