use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::paths;
use crate::sandbox::kit::{self, KitSummary};

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredKit {
    pub(crate) dir: PathBuf,
    pub(crate) handle: String,
    pub(crate) label: String,
    pub(crate) summary: KitSummary,
}

/// Loads every mixin kit under the `kits/` dir of each discovery root, in
/// root order and then by directory name. A root without a `kits/` dir
/// contributes nothing; a kit that fails to load or is not a valid mixin
/// aborts the whole discovery, as does a reserved or repeated handle.
pub(crate) fn discover() -> Result<Vec<DiscoveredKit>> {
    let cwd = env::current_dir().ok();
    let mut out: Vec<DiscoveredKit> = Vec::new();

    for (kits_dir, label) in roots(cwd.as_deref()) {
        for dir in subdirs_of(&kits_dir) {
            let kit = kit::load(&dir)?;
            kit.check_mixin()?;
            if kit::is_reserved_handle(&kit.handle) {
                bail!(
                    "sbx kit at {} uses the reserved handle '{}'",
                    dir.display(),
                    kit.handle
                );
            }
            if let Some(seen) = out.iter().find(|seen| seen.handle == kit.handle) {
                bail!(
                    "sbx kit handle '{}' is declared twice: {} and {}",
                    kit.handle,
                    seen.dir.display(),
                    dir.display()
                );
            }
            let summary = kit.summary();
            out.push(DiscoveredKit {
                dir: kit.dir,
                handle: kit.handle,
                label: label.clone(),
                summary,
            });
        }
    }

    Ok(out)
}

/// The `kits/` dir of every discovery root paired with the label shown for
/// kits found there. Agent roots come before agent hook roots so the listing
/// groups by kind rather than interleaving per agent.
fn roots(cwd: Option<&Path>) -> Vec<(PathBuf, String)> {
    let mut roots = vec![
        (paths::kits_dir(&paths::config_dir()), "config".to_string()),
        (
            paths::kits_dir(&paths::functions_dir()),
            "functions".to_string(),
        ),
    ];

    let agents = subdirs_of(&paths::agents_data_dir());
    for agent in &agents {
        roots.push((
            paths::kits_dir(agent),
            format!("agents/{}", dir_name(agent)),
        ));
    }
    for agent in &agents {
        roots.push((
            paths::kits_dir(&agent.join("hooks")),
            format!("agents/{}/hooks", dir_name(agent)),
        ));
    }

    roots.push((paths::kits_dir(&paths::rags_dir()), "rags".to_string()));
    roots.push((paths::kits_dir(&paths::roles_dir()), "roles".to_string()));
    roots.push((
        paths::kits_dir(&paths::roles_dir().join("hooks")),
        "roles/hooks".to_string(),
    ));
    roots.push((paths::kits_dir(&paths::hooks_dir()), "hooks".to_string()));

    if let Some(cwd) = cwd {
        for kits_dir in paths::workspace_kit_roots(cwd) {
            let label = format!("workspace {}", kits_dir.display());
            roots.push((kits_dir, label));
        }
    }

    roots
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn log_discovery(kits: &[DiscoveredKit], disabled: bool) {
    if disabled {
        info!("Mixin discovery disabled via --no-mixins.");
        return;
    }

    if kits.is_empty() {
        info!("No sbx kits discovered.");
        return;
    }

    let header = format!("Applying {} sbx kit(s):", kits.len());
    info!("{header}");
    println!("{header}");

    for kit in kits {
        let line = discovery_line(kit);
        info!("{line}");
        println!("{line}");
    }
}

fn discovery_line(kit: &DiscoveredKit) -> String {
    format!(
        "  {} (adds: {} install hook(s), {} runtime domain(s), {} install domain(s))",
        kit.label, kit.summary.install_hooks, kit.summary.runtime_allow, kit.summary.install_allow,
    )
}

/// Writes a kit Coyote renders itself to `<cache>/sbx-kits/<handle>/`,
/// replacing whatever is there, and returns that directory.
pub(crate) fn write_generated_kit(handle: &str, yaml: &str) -> Result<PathBuf> {
    let dir = paths::generated_kit_dir(handle);
    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create generated kit dir {}", dir.display()))?;
    let path = dir.join(format!("{handle}.yaml"));
    crate::function::write_file_atomic(&path, yaml, None)
        .with_context(|| format!("Failed to write generated kit {}", path.display()))?;
    Ok(dir)
}

fn subdirs_of(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return result;
    };

    let mut entries: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    result.extend(entries.into_iter().map(|e| e.path()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CacheDirGuard, TestConfigDirGuard};
    use serial_test::serial;
    use std::time;

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
  - type: com.docker.sandbox/lifecycle@1
    config:
      install:
        - command: echo one
        - command: echo two
"#;

    const INSTALL_HOOK_ONLY_MIXIN: &str = r#"# syntax=docker/sandbox-kit:3
schemaVersion: "3"
kind: mixin
capabilities:
  - type: com.docker.sandbox/lifecycle@1
    config:
      install:
        - command: echo tools
"#;

    const LEGACY_MIXIN: &str = "name: legacy\nschemaVersion: \"2\"\nkind: mixin\n";

    fn unique_root(prefix: &str) -> PathBuf {
        let nanos = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-{prefix}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_kit(kits_dir: &Path, handle: &str, yaml: &str) -> PathBuf {
        let dir = kits_dir.join(handle);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{handle}.yaml")), yaml).unwrap();
        dir
    }

    /// Moves the process into a fresh temp dir so the workspace walk starts
    /// from a known place, and moves back (removing the dir) on drop. The
    /// root is canonicalized because `current_dir` reports resolved paths.
    struct CwdGuard {
        previous: PathBuf,
        root: PathBuf,
    }

    impl CwdGuard {
        fn new(label: &str) -> Self {
            let previous = env::current_dir().unwrap();
            let root = unique_root(label).canonicalize().unwrap();
            env::set_current_dir(&root).unwrap();
            Self { previous, root }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn labels(kits: &[DiscoveredKit]) -> Vec<&str> {
        kits.iter().map(|kit| kit.label.as_str()).collect()
    }

    fn assert_single(kits: &[DiscoveredKit], dir: &Path, handle: &str, label: &str) {
        assert_eq!(kits.len(), 1, "{kits:?}");
        assert_eq!(kits[0].dir, dir);
        assert_eq!(kits[0].handle, handle);
        assert_eq!(kits[0].label, label);
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_config_kits() {
        let config = TestConfigDirGuard::new("kits-config");
        let _cwd = CwdGuard::new("kits-config-cwd");
        let dir = write_kit(&paths::kits_dir(&config.path), "a", VALID_MIXIN);

        assert_single(&discover().unwrap(), &dir, "a", "config");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_functions_kits() {
        let config = TestConfigDirGuard::new("kits-functions");
        let _cwd = CwdGuard::new("kits-functions-cwd");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("functions")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&discover().unwrap(), &dir, "a", "functions");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_an_agent_kits() {
        let config = TestConfigDirGuard::new("kits-agent");
        let _cwd = CwdGuard::new("kits-agent-cwd");
        let agent = config.path.join("agents").join("researcher");
        let dir = write_kit(&paths::kits_dir(&agent), "a", VALID_MIXIN);

        assert_single(&discover().unwrap(), &dir, "a", "agents/researcher");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_an_agent_hooks_kits() {
        let config = TestConfigDirGuard::new("kits-agent-hooks");
        let _cwd = CwdGuard::new("kits-agent-hooks-cwd");
        let hooks = config.path.join("agents").join("researcher").join("hooks");
        let dir = write_kit(&paths::kits_dir(&hooks), "a", VALID_MIXIN);

        assert_single(&discover().unwrap(), &dir, "a", "agents/researcher/hooks");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_rags_kits() {
        let config = TestConfigDirGuard::new("kits-rags");
        let _cwd = CwdGuard::new("kits-rags-cwd");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("rags")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&discover().unwrap(), &dir, "a", "rags");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_roles_kits() {
        let config = TestConfigDirGuard::new("kits-roles");
        let _cwd = CwdGuard::new("kits-roles-cwd");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("roles")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&discover().unwrap(), &dir, "a", "roles");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_role_hooks_kits() {
        let config = TestConfigDirGuard::new("kits-role-hooks");
        let _cwd = CwdGuard::new("kits-role-hooks-cwd");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("roles").join("hooks")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&discover().unwrap(), &dir, "a", "roles/hooks");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_hooks_kits() {
        let config = TestConfigDirGuard::new("kits-hooks");
        let _cwd = CwdGuard::new("kits-hooks-cwd");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("hooks")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&discover().unwrap(), &dir, "a", "hooks");
    }

    #[test]
    #[serial]
    fn discover_finds_a_workspace_kit_from_a_nested_cwd() {
        let _config = TestConfigDirGuard::new("kits-workspace");
        let cwd = CwdGuard::new("kits-workspace-cwd");
        let kits = cwd.root.join(".coyote").join("kits");
        let dir = write_kit(&kits, "w", VALID_MIXIN);
        let nested = cwd.root.join("x").join("y");
        fs::create_dir_all(&nested).unwrap();
        env::set_current_dir(&nested).unwrap();

        assert_single(
            &discover().unwrap(),
            &dir,
            "w",
            &format!("workspace {}", kits.display()),
        );
    }

    #[test]
    #[serial]
    fn discover_visits_roots_in_documented_order_and_never_the_cache() {
        let config = TestConfigDirGuard::new("kits-order");
        let cwd = CwdGuard::new("kits-order-cwd");
        let cache = CacheDirGuard::new("kits-order");
        let agents = config.path.join("agents");
        let workspace_kits = cwd.root.join(".coyote").join("kits");

        write_kit(&paths::kits_dir(&config.path), "c", VALID_MIXIN);
        write_kit(
            &paths::kits_dir(&config.path.join("functions")),
            "f",
            VALID_MIXIN,
        );
        // Seeded out of alphabetical order so the sort is what puts alpha first.
        write_kit(&paths::kits_dir(&agents.join("beta")), "a2", VALID_MIXIN);
        write_kit(&paths::kits_dir(&agents.join("alpha")), "a1", VALID_MIXIN);
        write_kit(
            &paths::kits_dir(&agents.join("beta").join("hooks")),
            "ah2",
            VALID_MIXIN,
        );
        write_kit(
            &paths::kits_dir(&agents.join("alpha").join("hooks")),
            "ah1",
            VALID_MIXIN,
        );
        write_kit(
            &paths::kits_dir(&config.path.join("rags")),
            "r",
            VALID_MIXIN,
        );
        write_kit(
            &paths::kits_dir(&config.path.join("roles")),
            "ro",
            VALID_MIXIN,
        );
        write_kit(
            &paths::kits_dir(&config.path.join("roles").join("hooks")),
            "rh",
            VALID_MIXIN,
        );
        write_kit(
            &paths::kits_dir(&config.path.join("hooks")),
            "h",
            VALID_MIXIN,
        );
        write_kit(&workspace_kits, "w", VALID_MIXIN);
        write_kit(&paths::sbx_kits_dir(), "cached", VALID_MIXIN);

        let kits = discover().unwrap();

        assert_eq!(
            labels(&kits),
            vec![
                "config",
                "functions",
                "agents/alpha",
                "agents/beta",
                "agents/alpha/hooks",
                "agents/beta/hooks",
                "rags",
                "roles",
                "roles/hooks",
                "hooks",
                &format!("workspace {}", workspace_kits.display()),
            ]
        );
        let handles: Vec<&str> = kits.iter().map(|kit| kit.handle.as_str()).collect();
        assert_eq!(
            handles,
            vec![
                "c", "f", "a1", "a2", "ah1", "ah2", "r", "ro", "rh", "h", "w"
            ]
        );
        assert!(
            kits.iter().all(|kit| !kit.dir.starts_with(&cache.root)),
            "the sbx kit cache is not a discovery root: {kits:?}"
        );
    }

    #[test]
    #[serial]
    fn discover_aborts_when_the_yaml_stem_differs_from_the_dir_name() {
        let config = TestConfigDirGuard::new("kits-stem");
        let _cwd = CwdGuard::new("kits-stem-cwd");
        let dir = paths::kits_dir(&config.path).join("a");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.yaml"), VALID_MIXIN).unwrap();

        let err = format!("{:#}", discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_aborts_when_the_syntax_line_is_missing() {
        let config = TestConfigDirGuard::new("kits-syntax");
        let _cwd = CwdGuard::new("kits-syntax-cwd");
        let yaml = VALID_MIXIN.split_once('\n').unwrap().1;
        let dir = write_kit(&paths::kits_dir(&config.path), "a", yaml);

        let err = format!("{:#}", discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_aborts_on_a_workload_kit() {
        let config = TestConfigDirGuard::new("kits-workload");
        let _cwd = CwdGuard::new("kits-workload-cwd");
        let yaml = VALID_MIXIN.replace("kind: mixin", "kind: workload");
        let dir = write_kit(&paths::kits_dir(&config.path), "a", &yaml);

        let err = format!("{:#}", discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
        assert!(err.contains("workload"), "{err}");
    }

    #[test]
    #[serial]
    fn discover_ignores_regular_files_beside_kit_dirs() {
        let config = TestConfigDirGuard::new("kits-readme");
        let _cwd = CwdGuard::new("kits-readme-cwd");
        let kits = paths::kits_dir(&config.path);
        let dir = write_kit(&kits, "a", VALID_MIXIN);
        fs::write(kits.join("README.md"), "about these kits\n").unwrap();

        assert_single(&discover().unwrap(), &dir, "a", "config");
    }

    #[test]
    #[serial]
    fn discover_rejects_a_reserved_handle() {
        let config = TestConfigDirGuard::new("kits-reserved");
        let _cwd = CwdGuard::new("kits-reserved-cwd");
        let dir = write_kit(&paths::kits_dir(&config.path), "coyote", VALID_MIXIN);

        let err = format!("{:#}", discover().unwrap_err());
        assert!(err.contains("reserved handle 'coyote'"), "{err}");
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_rejects_a_handle_declared_under_two_roots() {
        let config = TestConfigDirGuard::new("kits-duplicate");
        let _cwd = CwdGuard::new("kits-duplicate-cwd");
        let first = write_kit(
            &paths::kits_dir(&config.path.join("functions")),
            "a",
            VALID_MIXIN,
        );
        let second = write_kit(
            &paths::kits_dir(&config.path.join("rags")),
            "a",
            VALID_MIXIN,
        );

        let err = format!("{:#}", discover().unwrap_err());
        assert!(err.contains("handle 'a' is declared twice"), "{err}");
        assert!(err.contains(&first.display().to_string()), "{err}");
        assert!(err.contains(&second.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_neither_reads_nor_warns_about_legacy_mixin_files() {
        crate::testing::install_log_collector();
        let config = TestConfigDirGuard::new("kits-no-legacy");
        let cwd = CwdGuard::new("kits-no-legacy-cwd");
        let root = &config.path;
        // The v2 file name is taken from the one path helper that still
        // knows it, so this test tracks the installers' cleanup target.
        let legacy = paths::global_tools_sbx_mixin_file();
        let legacy_name = legacy.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(legacy.parent().unwrap(), root.join("functions"));
        let legacy_files = [
            root.join(&legacy_name),
            legacy.clone(),
            root.join("functions").join("x").join(&legacy_name),
            root.join("agents").join("a").join(&legacy_name),
            root.join("agents")
                .join("a")
                .join(format!("docs.{legacy_name}")),
            root.join("hooks").join(format!("h.{legacy_name}")),
            root.join("rags").join(format!("r.{legacy_name}")),
            cwd.root.join(".coyote").join(&legacy_name),
        ];
        for path in &legacy_files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, LEGACY_MIXIN).unwrap();
        }
        let warns_before = crate::testing::warn_snapshot().len();

        let kits = discover().unwrap();

        assert!(kits.is_empty(), "{kits:?}");
        let warns = crate::testing::warn_snapshot();
        assert_eq!(warns.len(), warns_before, "{warns:?}");
        let markers = [root.display().to_string(), cwd.root.display().to_string()];
        assert!(
            warns
                .iter()
                .all(|line| markers.iter().all(|marker| !line.contains(marker))),
            "{warns:?}"
        );
    }

    #[test]
    #[serial]
    fn write_generated_kit_lands_a_loadable_mixin_in_the_cache_and_overwrites() {
        use crate::sandbox::mcp_credentials::render_mixin_yaml;

        let cache = CacheDirGuard::new("generated-kit");
        let first = render_mixin_yaml(&[], &["api.example.com".to_string()]).unwrap();

        let dir = write_generated_kit("coyote-mcp", &first).unwrap();

        assert_eq!(dir, paths::generated_kit_dir("coyote-mcp"));
        assert_eq!(dir, cache.root.join("sbx-kits").join("coyote-mcp"));
        let file = dir.join("coyote-mcp.yaml");
        assert_eq!(fs::read_to_string(&file).unwrap(), first);
        kit::load(&dir).unwrap().check_mixin().unwrap();

        let second = render_mixin_yaml(&[], &["other.example.com".to_string()]).unwrap();
        assert_ne!(first, second);
        assert_eq!(write_generated_kit("coyote-mcp", &second).unwrap(), dir);
        assert_eq!(fs::read_to_string(&file).unwrap(), second);
        kit::load(&dir).unwrap().check_mixin().unwrap();
    }

    #[test]
    #[serial]
    fn discovery_lines_name_the_root_and_the_summary_counts() {
        let config = TestConfigDirGuard::new("kits-log-line");
        let _cwd = CwdGuard::new("kits-log-line-cwd");
        let rags = config.path.join("rags");
        fs::create_dir_all(&rags).unwrap();
        crate::rag::generate_rag_kit(
            &rags.join("company-docs.yaml"),
            "https://store.example.com",
            "company-docs",
            Some("COMPANY_DOCS_API_KEY"),
            "api-key",
            "%s",
        )
        .unwrap();
        write_kit(
            &paths::kits_dir(&config.path.join("functions")),
            "tools",
            INSTALL_HOOK_ONLY_MIXIN,
        );

        let kits = discover().unwrap();

        assert_eq!(labels(&kits), vec!["functions", "rags"]);
        assert_eq!(kits[0].handle, "tools");
        assert_eq!(kits[1].handle, "rag-company-docs");
        assert!(kits[1].summary.runtime_allow >= 1, "{:?}", kits[1].summary);
        assert_eq!(
            discovery_line(&kits[0]),
            "  functions (adds: 1 install hook(s), 0 runtime domain(s), 0 install domain(s))"
        );
        assert_eq!(
            discovery_line(&kits[1]),
            "  rags (adds: 0 install hook(s), 1 runtime domain(s), 0 install domain(s))"
        );
    }
}
