use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{BASE_KIT_HANDLE, paths};
use crate::sandbox::kit::{self, KitDir, KitSummary};

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredKit {
    pub(crate) dir: PathBuf,
    pub(crate) handle: String,
    pub(crate) label: String,
    pub(crate) summary: KitSummary,
    pub(crate) doc: serde_yaml::Value,
}

pub(crate) fn discover() -> Result<Vec<DiscoveredKit>> {
    discover_from(env::current_dir().ok().as_deref())
}

pub(crate) fn discover_from(cwd: Option<&Path>) -> Result<Vec<DiscoveredKit>> {
    let mut out: Vec<DiscoveredKit> = Vec::new();

    for (kits_dir, label) in roots(cwd) {
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
                doc: kit.doc,
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

pub(crate) fn log_discovery(kits: &[DiscoveredKit]) {
    if kits.is_empty() {
        info!("No sbx kits discovered.");
        return;
    }

    let header = discovery_header(kits.len());
    info!("{header}");
    println!("{header}");

    for kit in kits {
        let line = discovery_line(kit);
        info!("{line}");
        println!("{line}");
    }
}

fn discovery_header(count: usize) -> String {
    format!("Applying {count} sbx kit(s):")
}

fn discovery_line(kit: &DiscoveredKit) -> String {
    format!(
        "  {} (adds: {} install hook(s), {} runtime domain(s), {} install domain(s))",
        kit.label, kit.summary.install_hooks, kit.summary.runtime_allow, kit.summary.install_allow,
    )
}

/// Writes a kit Coyote renders itself as `<cache>/sbx-kits/<handle>/<handle>.yaml`,
/// rewriting `<handle>.yaml` in place or replacing an empty or dot-files-only
/// kit dir wholesale via `kit::install_kit_yaml`, and returns the kit loaded
/// back from disk once it checks out as a valid mixin kit.
pub(crate) fn write_generated_kit(handle: &str, yaml: &str) -> Result<KitDir> {
    if handle == BASE_KIT_HANDLE {
        bail!("generated kit handle '{handle}' is reserved for the base kit");
    }
    let dir = paths::generated_kit_dir(handle);
    kit::install_kit_yaml(&dir, handle, yaml)
        .with_context(|| format!("Failed to write generated kit {}", dir.display()))?;
    kit::load(&dir)
        .and_then(|kit| kit.check_mixin().map(|()| kit))
        .with_context(|| {
            format!(
                "generated kit at {} is not a valid mixin kit",
                dir.display()
            )
        })
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
    use crate::config::WORKSPACE_COYOTE_DIR_NAME;
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

    /// A fresh temp dir handed to `discover_from` as the workspace to walk
    /// from, removed on drop.
    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new(label: &str) -> Self {
            Self {
                root: unique_root(label),
            }
        }

        fn kits_dir(&self) -> PathBuf {
            paths::kits_dir(&self.root.join(WORKSPACE_COYOTE_DIR_NAME))
        }

        fn discover(&self) -> Result<Vec<DiscoveredKit>> {
            discover_from(Some(&self.root))
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
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
    fn discover_walks_the_workspace_from_the_process_cwd() {
        let _config = TestConfigDirGuard::new("kits-real-cwd");

        let kits = discover().unwrap();

        assert!(
            kits.iter().all(|kit| kit.label.starts_with("workspace ")),
            "{kits:?}"
        );
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_config_kits() {
        let config = TestConfigDirGuard::new("kits-config");
        let ws = Workspace::new("kits-config-ws");
        let dir = write_kit(&paths::kits_dir(&config.path), "a", VALID_MIXIN);

        assert_single(&ws.discover().unwrap(), &dir, "a", "config");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_functions_kits() {
        let config = TestConfigDirGuard::new("kits-functions");
        let ws = Workspace::new("kits-functions-ws");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("functions")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&ws.discover().unwrap(), &dir, "a", "functions");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_an_agent_kits() {
        let config = TestConfigDirGuard::new("kits-agent");
        let ws = Workspace::new("kits-agent-ws");
        let agent = config.path.join("agents").join("researcher");
        let dir = write_kit(&paths::kits_dir(&agent), "a", VALID_MIXIN);

        assert_single(&ws.discover().unwrap(), &dir, "a", "agents/researcher");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_an_agent_hooks_kits() {
        let config = TestConfigDirGuard::new("kits-agent-hooks");
        let ws = Workspace::new("kits-agent-hooks-ws");
        let hooks = config.path.join("agents").join("researcher").join("hooks");
        let dir = write_kit(&paths::kits_dir(&hooks), "a", VALID_MIXIN);

        assert_single(
            &ws.discover().unwrap(),
            &dir,
            "a",
            "agents/researcher/hooks",
        );
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_rags_kits() {
        let config = TestConfigDirGuard::new("kits-rags");
        let ws = Workspace::new("kits-rags-ws");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("rags")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&ws.discover().unwrap(), &dir, "a", "rags");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_roles_kits() {
        let config = TestConfigDirGuard::new("kits-roles");
        let ws = Workspace::new("kits-roles-ws");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("roles")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&ws.discover().unwrap(), &dir, "a", "roles");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_role_hooks_kits() {
        let config = TestConfigDirGuard::new("kits-role-hooks");
        let ws = Workspace::new("kits-role-hooks-ws");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("roles").join("hooks")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&ws.discover().unwrap(), &dir, "a", "roles/hooks");
    }

    #[test]
    #[serial]
    fn discover_finds_a_kit_under_hooks_kits() {
        let config = TestConfigDirGuard::new("kits-hooks");
        let ws = Workspace::new("kits-hooks-ws");
        let dir = write_kit(
            &paths::kits_dir(&config.path.join("hooks")),
            "a",
            VALID_MIXIN,
        );

        assert_single(&ws.discover().unwrap(), &dir, "a", "hooks");
    }

    #[test]
    #[serial]
    fn discover_finds_a_workspace_kit_from_a_nested_cwd() {
        let _config = TestConfigDirGuard::new("kits-workspace");
        let ws = Workspace::new("kits-workspace-ws");
        let kits = ws.kits_dir();
        let dir = write_kit(&kits, "w", VALID_MIXIN);
        let nested = ws.root.join("x").join("y");
        fs::create_dir_all(&nested).unwrap();

        assert_single(
            &discover_from(Some(&nested)).unwrap(),
            &dir,
            "w",
            &format!("workspace {}", kits.display()),
        );
    }

    #[test]
    #[serial]
    fn discover_visits_roots_in_documented_order_and_never_the_cache() {
        let config = TestConfigDirGuard::new("kits-order");
        let ws = Workspace::new("kits-order-ws");
        let agents = config.path.join("agents");
        let workspace_kits = ws.kits_dir();

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
        assert!(
            paths::sbx_kits_dir().starts_with(config.cache_root()),
            "the guard pins the sbx kit cache under its cache root, or the next write lands in the real cache"
        );
        write_kit(&paths::sbx_kits_dir(), "cached", VALID_MIXIN);

        let kits = ws.discover().unwrap();

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
            kits.iter()
                .all(|kit| !kit.dir.starts_with(config.cache_root())),
            "the sbx kit cache is not a discovery root: {kits:?}"
        );
    }

    #[test]
    #[serial]
    fn discover_aborts_when_the_yaml_stem_differs_from_the_dir_name() {
        let config = TestConfigDirGuard::new("kits-stem");
        let ws = Workspace::new("kits-stem-ws");
        let dir = paths::kits_dir(&config.path).join("a");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.yaml"), VALID_MIXIN).unwrap();

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_aborts_when_the_syntax_line_is_missing() {
        let config = TestConfigDirGuard::new("kits-syntax");
        let ws = Workspace::new("kits-syntax-ws");
        let yaml = VALID_MIXIN.split_once('\n').unwrap().1;
        let dir = write_kit(&paths::kits_dir(&config.path), "a", yaml);

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_aborts_on_a_workload_kit() {
        let config = TestConfigDirGuard::new("kits-workload");
        let ws = Workspace::new("kits-workload-ws");
        let yaml = VALID_MIXIN.replace("kind: mixin", "kind: workload");
        let dir = write_kit(&paths::kits_dir(&config.path), "a", &yaml);

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
        assert!(err.contains("workload"), "{err}");
    }

    #[test]
    #[serial]
    fn discover_ignores_regular_files_beside_kit_dirs() {
        let config = TestConfigDirGuard::new("kits-readme");
        let ws = Workspace::new("kits-readme-ws");
        let kits = paths::kits_dir(&config.path);
        let dir = write_kit(&kits, "a", VALID_MIXIN);
        fs::write(kits.join("README.md"), "about these kits\n").unwrap();

        assert_single(&ws.discover().unwrap(), &dir, "a", "config");
    }

    #[test]
    #[serial]
    fn discover_rejects_a_reserved_handle() {
        let config = TestConfigDirGuard::new("kits-reserved");
        let ws = Workspace::new("kits-reserved-ws");
        let dir = write_kit(&paths::kits_dir(&config.path), "coyote", VALID_MIXIN);

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains("reserved handle 'coyote'"), "{err}");
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discover_rejects_a_handle_declared_under_two_roots() {
        let config = TestConfigDirGuard::new("kits-duplicate");
        let ws = Workspace::new("kits-duplicate-ws");
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

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains("handle 'a' is declared twice"), "{err}");
        assert!(err.contains(&first.display().to_string()), "{err}");
        assert!(err.contains(&second.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn write_generated_kit_lands_a_loadable_mixin_in_the_cache_and_overwrites() {
        use crate::sandbox::mcp_credentials::render_mixin_yaml;

        let cache = CacheDirGuard::new("generated-kit");
        let first = render_mixin_yaml(&[], &["api.example.com".to_string()]).unwrap();

        let kit = write_generated_kit("coyote-mcp", &first).unwrap();
        let dir = kit.dir.clone();

        assert_eq!(kit.handle, "coyote-mcp");
        assert_eq!(dir, paths::generated_kit_dir("coyote-mcp"));
        assert_eq!(dir, cache.root.join("sbx-kits").join("coyote-mcp"));
        let file = dir.join("coyote-mcp.yaml");
        assert_eq!(fs::read_to_string(&file).unwrap(), first);
        kit.check_mixin().unwrap();

        let second = render_mixin_yaml(&[], &["other.example.com".to_string()]).unwrap();
        assert_ne!(first, second);
        assert_eq!(write_generated_kit("coyote-mcp", &second).unwrap().dir, dir);
        assert_eq!(fs::read_to_string(&file).unwrap(), second);
        kit::load(&dir).unwrap().check_mixin().unwrap();
    }

    #[test]
    fn write_generated_kit_refuses_the_base_kit_handle() {
        let err = format!(
            "{:#}",
            write_generated_kit(BASE_KIT_HANDLE, VALID_MIXIN).unwrap_err()
        );
        assert!(
            err.contains("handle 'coyote' is reserved for the base kit"),
            "{err}"
        );
    }

    #[test]
    #[serial]
    fn write_generated_kit_rejects_yaml_that_is_not_a_mixin_kit() {
        let cache = CacheDirGuard::new("generated-kit-invalid");
        let yaml = VALID_MIXIN.replace("kind: mixin", "kind: workload");

        let err = format!(
            "{:#}",
            write_generated_kit("coyote-mcp", &yaml).unwrap_err()
        );

        let dir = cache.root.join("sbx-kits").join("coyote-mcp");
        assert!(err.contains("is not a valid mixin kit"), "{err}");
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn discovery_lines_name_the_root_and_the_summary_counts() {
        let config = TestConfigDirGuard::new("kits-log-line");
        let ws = Workspace::new("kits-log-line-ws");
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

        let kits = ws.discover().unwrap();

        assert_eq!(labels(&kits), vec!["functions", "rags"]);
        assert_eq!(kits[0].handle, "tools");
        assert_eq!(kits[1].handle, "rag-company-docs");
        assert!(kits[1].summary.runtime_allow >= 1, "{:?}", kits[1].summary);
        assert_eq!(discovery_header(kits.len()), "Applying 2 sbx kit(s):");
        assert_eq!(
            discovery_line(&kits[0]),
            "  functions (adds: 1 install hook(s), 0 runtime domain(s), 0 install domain(s))"
        );
        assert_eq!(
            discovery_line(&kits[1]),
            "  rags (adds: 0 install hook(s), 1 runtime domain(s), 0 install domain(s))"
        );
    }

    #[test]
    #[serial]
    fn usage_probe_discover_aborts_on_a_mixin_dir_carrying_a_dockerfile() {
        let config = TestConfigDirGuard::new("kits-dockerfile");
        let ws = Workspace::new("kits-dockerfile-ws");
        let kits = paths::kits_dir(&config.path);
        write_kit(&kits, "a", VALID_MIXIN);
        let dir = write_kit(&kits, "b", VALID_MIXIN);
        fs::write(dir.join("b.dockerfile"), "FROM scratch\n").unwrap();

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
        assert!(err.contains("b.dockerfile"), "{err}");
    }

    #[test]
    #[serial]
    fn usage_probe_discover_aborts_on_an_empty_dir_under_kits() {
        let config = TestConfigDirGuard::new("kits-empty-dir");
        let ws = Workspace::new("kits-empty-dir-ws");
        let kits = paths::kits_dir(&config.path);
        write_kit(&kits, "a", VALID_MIXIN);
        let empty = kits.join("drafts");
        fs::create_dir_all(&empty).unwrap();

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains(&empty.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn usage_probe_discover_rejects_the_generated_kit_handle() {
        let config = TestConfigDirGuard::new("kits-reserved-mcp");
        let ws = Workspace::new("kits-reserved-mcp-ws");
        let dir = write_kit(&paths::kits_dir(&config.path), "coyote-mcp", VALID_MIXIN);

        let err = format!("{:#}", ws.discover().unwrap_err());
        assert!(err.contains("reserved handle 'coyote-mcp'"), "{err}");
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    #[serial]
    fn usage_probe_discover_lists_every_workspace_kits_dir_nearest_first() {
        let _config = TestConfigDirGuard::new("kits-workspace-chain");
        let ws = Workspace::new("kits-workspace-chain-ws");
        let outer = ws.kits_dir();
        let inner_root = ws.root.join("proj");
        let inner = paths::kits_dir(&inner_root.join(WORKSPACE_COYOTE_DIR_NAME));
        let outer_dir = write_kit(&outer, "outer", VALID_MIXIN);
        let inner_dir = write_kit(&inner, "inner", VALID_MIXIN);
        // A `.coyote` dir WITHOUT a `kits/` subdir between the two must not
        // contribute a root (and must not abort).
        let between = inner_root.join("src");
        fs::create_dir_all(between.join(WORKSPACE_COYOTE_DIR_NAME)).unwrap();
        let nested = between.join("deep");
        fs::create_dir_all(&nested).unwrap();

        let kits = discover_from(Some(&nested)).unwrap();

        assert_eq!(
            kits.iter().map(|kit| kit.dir.clone()).collect::<Vec<_>>(),
            vec![inner_dir, outer_dir]
        );
        assert_eq!(
            labels(&kits),
            vec![
                format!("workspace {}", inner.display()),
                format!("workspace {}", outer.display()),
            ]
        );
    }

    #[test]
    #[serial]
    fn usage_probe_discover_treats_an_absent_or_empty_kits_dir_as_nothing() {
        let config = TestConfigDirGuard::new("kits-none");
        let ws = Workspace::new("kits-none-ws");
        // Roots exist but carry no `kits/` dir at all ...
        for sub in ["functions", "agents/a", "rags", "roles", "hooks"] {
            fs::create_dir_all(config.path.join(sub)).unwrap();
        }
        assert!(ws.discover().unwrap().is_empty());
        // ... and an empty `kits/` dir contributes nothing either.
        fs::create_dir_all(paths::kits_dir(&config.path)).unwrap();
        fs::create_dir_all(paths::kits_dir(&config.path.join("functions"))).unwrap();
        assert!(ws.discover().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn usage_probe_write_generated_kit_leaves_only_the_yaml_behind() {
        use crate::sandbox::mcp_credentials::render_mixin_yaml;

        let cache = CacheDirGuard::new("generated-kit-residue");
        // Cold cache: the sbx-kits root does not exist yet.
        assert!(!cache.root.join("sbx-kits").exists());
        let first = render_mixin_yaml(&[], &["api.example.com".to_string()]).unwrap();
        let second = render_mixin_yaml(&[], &["other.example.com".to_string()]).unwrap();

        let dir = write_generated_kit("coyote-mcp", &first).unwrap().dir;
        write_generated_kit("coyote-mcp", &second).unwrap();
        write_generated_kit("coyote-mcp", &first).unwrap();

        let entries: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["coyote-mcp.yaml".to_string()], "{entries:?}");
        assert_eq!(
            fs::read_to_string(dir.join("coyote-mcp.yaml")).unwrap(),
            first
        );
        // The generated dir is a sibling of the base kit, never inside it.
        assert_eq!(dir.parent().unwrap(), paths::sbx_kits_dir());
        assert_ne!(dir, paths::base_kit_dir());
        let roots: Vec<String> = fs::read_dir(paths::sbx_kits_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(roots, vec!["coyote-mcp".to_string()], "{roots:?}");
        // The kit is staged beside sbx-kits; no stage dir may outlive a write.
        assert_eq!(listing(&cache.root), vec!["sbx-kits".to_string()]);
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    #[serial]
    fn usage_probe_write_generated_kit_refuses_the_base_handle_before_touching_the_base_kit() {
        let _cache = CacheDirGuard::new("generated-kit-base-handle");
        let base = crate::sandbox::extract_embedded_kit().unwrap();
        let yaml = base.join("coyote.yaml");
        let before = fs::read(&yaml).unwrap();
        let hash_before = fs::read_to_string(paths::base_kit_hash_file()).unwrap();
        let listing_before = listing(&base);
        assert_ne!(before, VALID_MIXIN.as_bytes());

        let err = format!(
            "{:#}",
            write_generated_kit(BASE_KIT_HANDLE, VALID_MIXIN).unwrap_err()
        );

        assert!(err.contains("reserved for the base kit"), "{err}");
        // The refusal happens before any write: the base kit is byte-identical,
        // no temp file is left beside it, and the hash still matches so the
        // next extraction keeps skipping the rewrite.
        assert_eq!(fs::read(&yaml).unwrap(), before);
        assert_eq!(listing(&base), listing_before);
        assert_eq!(
            fs::read_to_string(paths::base_kit_hash_file()).unwrap(),
            hash_before
        );
        assert_eq!(crate::sandbox::extract_embedded_kit().unwrap(), base);
        assert_eq!(fs::read(&yaml).unwrap(), before);
    }

    #[test]
    #[serial]
    fn usage_probe_write_generated_kit_recovers_after_a_rejected_write() {
        let cache = CacheDirGuard::new("generated-kit-recovery");
        let dir = cache.root.join("sbx-kits").join("coyote-mcp");
        let shown = dir.display().to_string();

        // Unparsable yaml is refused with the dir named; whatever is left on
        // disk must not load as a kit, so nothing downstream can consume it.
        let err = format!(
            "{:#}",
            write_generated_kit("coyote-mcp", "kind: [unterminated\n").unwrap_err()
        );
        assert!(err.contains(&shown), "{err}");
        assert!(kit::load(&dir).is_err());

        // A valid write for the same handle recovers without manual cleanup.
        assert_eq!(
            write_generated_kit("coyote-mcp", VALID_MIXIN).unwrap().dir,
            dir
        );
        assert_eq!(
            fs::read_to_string(dir.join("coyote-mcp.yaml")).unwrap(),
            VALID_MIXIN
        );
        kit::load(&dir).unwrap().check_mixin().unwrap();

        // The check reads the DIRECTORY back, not the string: a stray
        // `<handle>.dockerfile` beside a perfectly valid yaml is refused with
        // the dir named, and removing it is enough to recover.
        let dockerfile = dir.join("coyote-mcp.dockerfile");
        fs::write(&dockerfile, "FROM scratch\n").unwrap();
        let err = format!(
            "{:#}",
            write_generated_kit("coyote-mcp", VALID_MIXIN).unwrap_err()
        );
        assert!(err.contains("is not a valid mixin kit"), "{err}");
        assert!(err.contains(&shown), "{err}");
        assert!(err.contains("dockerfile"), "{err}");
        fs::remove_file(&dockerfile).unwrap();
        assert_eq!(
            write_generated_kit("coyote-mcp", VALID_MIXIN).unwrap().dir,
            dir
        );
        assert_eq!(listing(&dir), vec!["coyote-mcp.yaml".to_string()]);
    }

    /// An empty kit dir cannot be filled in place when it is read-only, and
    /// leaving it would abort discovery, so it is replaced wholesale.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn write_generated_kit_replaces_an_empty_kit_dir_it_cannot_write_into() {
        use std::os::unix::fs::PermissionsExt;
        struct RestorePerms(PathBuf);
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id -u");
        if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
            eprintln!("skipping: running as root, permission bits are bypassed");
            return;
        }
        let cache = CacheDirGuard::new("generated-kit-ro-empty");
        let dir = cache.root.join("sbx-kits").join("coyote-mcp");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = dir.join(".write-probe");
        if fs::write(&probe, "x").is_ok() {
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: permission bits are not enforced here");
            return;
        }
        let _restore = RestorePerms(dir.clone());

        let result = write_generated_kit("coyote-mcp", VALID_MIXIN);

        assert_eq!(result.unwrap().dir, dir);
        assert_eq!(listing(&dir), vec!["coyote-mcp.yaml".to_string()]);
        assert_eq!(listing(&cache.root), vec!["sbx-kits".to_string()]);
        kit::load(&dir).unwrap().check_mixin().unwrap();
    }

    #[test]
    #[serial]
    fn usage_probe_discover_from_without_a_cwd_scans_config_roots_but_no_workspace() {
        let config = TestConfigDirGuard::new("kits-no-cwd");
        let ws = Workspace::new("kits-no-cwd-ws");
        let config_kit = write_kit(&paths::kits_dir(&config.path), "a", VALID_MIXIN);
        let workspace_kit = write_kit(&ws.kits_dir(), "w", VALID_MIXIN);

        // No cwd at all: the eight config-side roots are still scanned, the
        // workspace walk contributes nothing.
        let kits = discover_from(None).unwrap();
        assert_eq!(labels(&kits), vec!["config"], "{kits:?}");
        assert_eq!(kits[0].dir, config_kit);
        assert!(kits.iter().all(|kit| !kit.label.starts_with("workspace ")));

        // With the workspace root as cwd both are found, config first.
        let kits = ws.discover().unwrap();
        assert_eq!(kits.len(), 2, "{kits:?}");
        assert_eq!(kits[0].dir, config_kit);
        assert_eq!(kits[1].dir, workspace_kit);
        assert_eq!(
            kits[1].label,
            format!("workspace {}", ws.kits_dir().display())
        );

        // A start path that does not exist yet still walks its EXISTING
        // ancestors: the workspace kit three levels up is found.
        let ghost = ws.root.join("x").join("y").join("z");
        assert!(!ghost.exists());
        let kits = discover_from(Some(&ghost)).unwrap();
        assert_eq!(kits.len(), 2, "{kits:?}");
        assert_eq!(kits[1].dir, workspace_kit);
    }
}
