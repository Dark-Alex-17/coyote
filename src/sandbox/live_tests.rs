//! Acceptance tests that drive the real `sbx` binary against Docker. A host
//! without the prerequisites passes them with a `skipping:` line. Anything
//! observed but too environment-dependent to assert on is printed under a
//! `record:` prefix for the person running the suite.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::kit::{self, KitSummary};
use super::mcp_credentials::{
    self, ApiKey, CredentialEntry, CredentialSpec, InjectRule, MCP_MIXIN_NAME,
};
use super::mixins::DiscoveredKit;

const SHIPPED_KITS: [&str; 4] = [
    "assets/sbx-kit/coyote",
    "assets/functions/kits/built-in-tools",
    "assets/agents/sisyphus/kits/sisyphus-ddg",
    "assets/agents/probe/kits/agent-probe",
];

/// Vault secret name behind the rendered `coyote-mcp` fixture.
const MCP_SECRET_NAME: &str = "live-mcp-token";

/// Everything the live suite needs from the host. Any missing piece turns
/// into a `skipping:` line rather than a failure, so the suite is safe to
/// run anywhere with `--ignored`.
fn live_prereqs() -> Result<(), String> {
    live_prereqs_in(&env::var_os("PATH").unwrap_or_default())
}

/// [`live_prereqs`] resolving every binary against `path` instead of the
/// process PATH, so the gate can be exercised without touching the
/// environment other tests spawn from.
fn live_prereqs_in(path: &std::ffi::OsStr) -> Result<(), String> {
    let cwd = env::current_dir().ok();
    let find = |binary: &str| {
        which::which_in(binary, Some(path), cwd.as_deref().unwrap_or(Path::new(".")))
    };
    let sbx = find(super::SBX_BINARY).map_err(|_| "sbx not on PATH".to_string())?;
    if !cfg!(unix) {
        return Err("the run transcript needs a unix `script`".to_string());
    }
    let out = Command::new(&sbx)
        .arg("version")
        .output()
        .map_err(|e| format!("sbx version failed to spawn: {e}"))?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    match super::parse_sbx_version(&text) {
        Some(v) if v >= super::MIN_SBX_VERSION => {}
        Some(found) => {
            return Err(format!(
                "sbx version {found:?} is below the minimum {:?}",
                super::MIN_SBX_VERSION
            ));
        }
        None => {
            return Err(format!(
                "could not read the sbx version from: {}",
                text.trim()
            ));
        }
    }
    find("script").map_err(|_| "script not on PATH".to_string())?;
    let docker = find("docker").map_err(|_| "docker not on PATH".to_string())?;
    let docker = Command::new(docker)
        .arg("info")
        .output()
        .map_err(|e| format!("docker info failed to spawn: {e}"))?;
    if !docker.status.success() {
        return Err("docker info failed".to_string());
    }
    Ok(())
}

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
        let root = env::temp_dir().join(format!(
            "coyote-live-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
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

/// Removes the sandbox when dropped, so a failed assertion or an early
/// return never leaves a `coyote-live-*` entry behind in `sbx ls`. The
/// resolved `sbx` path is captured up front so the drop does not depend on
/// the process PATH.
struct SandboxGuard {
    sbx: PathBuf,
    name: String,
}

impl SandboxGuard {
    fn new(sbx: PathBuf, name: &str) -> Self {
        Self {
            sbx,
            name: name.to_string(),
        }
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        println!("record: [rm] $ {} rm -f {}", self.sbx.display(), self.name);
        match Command::new(&self.sbx)
            .args(["rm", "-f", &self.name])
            .output()
        {
            Ok(out) => println!("record: [rm] exit status: {}", out.status),
            Err(err) => println!("record: [rm] failed to spawn: {err}"),
        }
    }
}

fn record_lines(label: &str, stream: &str, bytes: &[u8]) {
    for line in String::from_utf8_lossy(bytes).lines() {
        println!("record: [{label}] {stream}: {line}");
    }
}

fn sbx(label: &str, args: &[&str], cwd: Option<&Path>) -> Result<Output> {
    println!(
        "record: [{label}] $ {} {}",
        super::SBX_BINARY,
        shell_words::join(args)
    );
    let mut command = Command::new(super::SBX_BINARY);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let out = command
        .output()
        .with_context(|| format!("failed to spawn sbx {}", shell_words::join(args)))?;
    println!("record: [{label}] exit status: {}", out.status);
    record_lines(label, "stdout", &out.stdout);
    record_lines(label, "stderr", &out.stderr);
    Ok(out)
}

fn sbx_exec(label: &str, name: &str, script: &str) -> Result<Output> {
    sbx(label, &["exec", name, "sh", "-c", script], None)
}

/// `sbx run` streams the agent through a pseudo-terminal and writes nothing
/// the agent printed to a plain pipe, so the run is wrapped in `script` to
/// give it a real terminal whose transcript can be read back.
fn sbx_run_under_pty(label: &str, name: &str, agent_args: &[&str]) -> Result<Output> {
    let run_args = super::run_args(name);
    let mut inner: Vec<&str> = run_args.iter().map(String::as_str).collect();
    inner.push("--");
    inner.extend_from_slice(agent_args);
    let joined = format!("{} {}", super::SBX_BINARY, shell_words::join(&inner));
    println!("record: [{label}] $ script: {joined}");
    let mut command = Command::new("script");
    if cfg!(target_os = "macos") {
        command
            .arg("-q")
            .arg("/dev/null")
            .arg(super::SBX_BINARY)
            .args(&inner);
    } else {
        command
            .arg("-q")
            .arg("-e")
            .arg("-c")
            .arg(&joined)
            .arg("/dev/null");
    }
    let out = command
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("failed to spawn script for {joined}"))?;
    println!("record: [{label}] exit status: {}", out.status);
    record_lines(label, "stdout", &out.stdout);
    record_lines(label, "stderr", &out.stderr);
    Ok(out)
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn shipped_kit(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Renders the shape `plan_mcp_mixin` produces for one remote MCP server
/// whose bearer token the proxy injects: a proxy-managed credential with an
/// inject rule, and that rule's domain on the runtime allow list.
fn render_mcp_kit(root: &Path) -> Result<(PathBuf, String)> {
    let secret_name = MCP_SECRET_NAME;
    let service_id = mcp_credentials::secret_service_id(secret_name);
    let spec = CredentialSpec {
        secret_name: secret_name.to_string(),
        service_id: service_id.clone(),
        env_var: mcp_credentials::sandbox_secret_env_var(secret_name),
        proxy_managed: true,
        inject: vec![InjectRule {
            domain: "mcp.example.com:443".to_string(),
            header: "Authorization".to_string(),
            format: "Bearer %s".to_string(),
        }],
        custom_hosts: Vec::new(),
        custom_allow_entries: Vec::new(),
        servers: vec!["live".to_string()],
    };
    let yaml = mcp_credentials::render_mixin_yaml(&[spec], &[])?;
    let dir = root.join("kits").join(MCP_MIXIN_NAME);
    kit::install_kit_yaml(&dir, MCP_MIXIN_NAME, &yaml)?;
    Ok((dir, service_id))
}

/// A rag-shaped mixin: the description `rag::generate_rag_kit` writes and
/// one proxy-managed credential whose inject domain is the only allow entry.
fn render_rag_kit(root: &Path) -> Result<PathBuf> {
    let rag_name = "x";
    let id = mcp_credentials::secret_service_id(rag_name);
    let allow_entry = "qdrant.example.com:6333".to_string();
    let credential = CredentialEntry {
        service: id.clone(),
        description: format!("API key for the attached RAG '{rag_name}'"),
        api_key: ApiKey {
            name: "COYOTE_RAG_X_API_KEY".to_string(),
            proxy_managed: true,
            inject: vec![InjectRule {
                domain: allow_entry.clone(),
                header: "api-key".to_string(),
                format: "%s".to_string(),
            }],
        },
    };
    let yaml = mcp_credentials::render_mixin_document(
        &crate::rag::rag_kit_description(rag_name),
        vec![credential],
        &[allow_entry],
    )?;
    let handle = crate::config::paths::rag_kit_handle(&id);
    let dir = root.join("kits").join(&handle);
    kit::install_kit_yaml(&dir, &handle, &yaml)?;
    Ok(dir)
}

fn inspect(dir: &Path) -> Result<()> {
    let handle = dir
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("kit dir {} has no UTF-8 name", dir.display()))?;
    let dir_str = dir
        .to_str()
        .with_context(|| format!("kit dir {} is not UTF-8", dir.display()))?;
    let out = sbx(
        &format!("inspect {handle}"),
        &["kit", "inspect", dir_str],
        None,
    )?;
    if !out.status.success() {
        bail!("sbx kit inspect {dir_str} exited with {}", out.status);
    }
    let text = combined(&out);
    // The handle must come back as the kit's own `Name:` row; the dir path
    // echoed by sbx would make a bare substring match trivially true.
    let named = text.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix("Name:")
            .is_some_and(|rest| rest.trim() == handle)
    });
    if !named {
        bail!("sbx kit inspect {dir_str} output has no 'Name: {handle}' row:\n{text}");
    }
    Ok(())
}

#[test]
#[ignore = "needs sbx and Docker on the host; run with --ignored"]
fn inspect_accepts_every_shipped_and_generated_kit() -> Result<()> {
    if let Err(reason) = live_prereqs() {
        println!("skipping: {reason}");
        return Ok(());
    }

    let root = TempRoot::new("inspect");
    let mut dirs: Vec<PathBuf> = SHIPPED_KITS.iter().map(|rel| shipped_kit(rel)).collect();
    dirs.push(render_mcp_kit(root.path())?.0);
    dirs.push(render_rag_kit(root.path())?);

    for dir in &dirs {
        inspect(dir)?;
    }
    Ok(())
}

#[test]
#[ignore = "needs sbx and Docker on the host; run with --ignored"]
fn create_exec_run_round_trip() -> Result<()> {
    if let Err(reason) = live_prereqs() {
        println!("skipping: {reason}");
        return Ok(());
    }

    let root = TempRoot::new("round-trip");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace)?;
    let (mcp_dir, mcp_service) = render_mcp_kit(root.path())?;
    let tools = kit::load(&shipped_kit("assets/functions/kits/built-in-tools"))?;
    tools.check_mixin()?;
    let mixins = [DiscoveredKit {
        dir: tools.dir,
        handle: tools.handle,
        label: "Coyote built-in tools".to_string(),
        summary: KitSummary::default(),
        doc: tools.doc,
    }];
    let base_kit = shipped_kit("assets/sbx-kit/coyote");

    // The same gates the launcher runs between discovery and `sbx create`,
    // so a disagreement between Coyote's own checks and sbx shows up here.
    let base = kit::load(&base_kit)?;
    base.check_workload()?;
    let generated = kit::load(&mcp_dir)?;
    let secret_names =
        std::collections::BTreeMap::from([(mcp_service, MCP_SECRET_NAME.to_string())]);
    super::preflight::check(&base, &mixins, Some(&generated), &secret_names)?;

    let name = format!("coyote-live-{}", std::process::id());
    let args = super::build_create_args(&name, &base_kit, &mixins, Some(&mcp_dir))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let sbx_path = which::which(super::SBX_BINARY)?;
    let _guard = SandboxGuard::new(sbx_path, &name);
    let create = sbx("create", &args, Some(&workspace))?;
    assert!(
        create.status.success(),
        "sbx create exited with {}",
        create.status
    );

    let env_line = sbx_exec(
        "env",
        &name,
        r#"echo "$IS_SANDBOX|$COYOTE_CONFIG_DIR|$EDITOR""#,
    )?;
    assert!(
        env_line.status.success(),
        "sbx exec for the ENV probe exited with {}",
        env_line.status
    );
    let actual = String::from_utf8_lossy(&env_line.stdout).trim().to_string();
    assert_eq!(
        actual, "1|/home/agent/.config/coyote|nano",
        "base kit ENV did not survive into the sandbox; actual line: {actual}"
    );

    let signing = sbx_exec(
        "ssh-signing-key-command",
        &name,
        "test -f /home/agent/.config/git/ssh-signing-key-command \
         && stat -c %a /home/agent/.config/git/ssh-signing-key-command",
    )?;
    assert!(
        signing.status.success(),
        "ssh-signing-key-command is missing: {}",
        signing.status
    );
    assert_eq!(String::from_utf8_lossy(&signing.stdout).trim(), "755");

    // A real launch copies the host config dir into the sandbox after create
    // (`copy_host_files`); without a config file `coyote --info` would meet
    // the first-run prompt, so the probe seeds a dry-run config the same way.
    let seed = sbx_exec(
        "seed config",
        &name,
        concat!(
            "mkdir -p \"$COYOTE_CONFIG_DIR\" && ",
            "printf 'test-password\\n' > \"$COYOTE_CONFIG_DIR/vault-pass\" && ",
            "printf '%s\\n' 'model: dryrun:dry-model' 'dry_run: true' ",
            "\"vault_password_file: '$COYOTE_CONFIG_DIR/vault-pass'\" ",
            "'clients:' '  - type: openai' '    name: dryrun' \"    api_key: 'unused'\" ",
            "'    models:' '      - name: dry-model' '        max_input_tokens: 100000' ",
            "'        supports_function_calling: true' 'save: false' ",
            "> \"$COYOTE_CONFIG_DIR/config.yaml\" && cat \"$COYOTE_CONFIG_DIR/config.yaml\""
        ),
    )?;
    assert!(
        seed.status.success(),
        "seeding the sandbox config exited with {}",
        seed.status
    );

    let info = sbx_run_under_pty("run --info", &name, &["--info"])?;
    assert!(info.status.success(), "sbx run exited with {}", info.status);
    // `mcp_config_file` is one of the path rows `coyote --info` always prints
    // (see the info table in config/request_context.rs).
    let info_text = combined(&info);
    assert!(
        info_text.contains("mcp_config_file"),
        "coyote --info output lacks the mcp_config_file row:\n{info_text}"
    );

    sbx_exec(
        "agent-context",
        &name,
        r#"files=$(find / -name COYOTE.md -not -path "*/proc/*" 2>/dev/null); echo "$files"; echo ---; for f in $files; do echo "== $f"; head -c 600 "$f"; echo; done"#,
    )?;
    sbx_exec(
        "kit env",
        &name,
        // Custom-secret placeholders from the host registry ride into every
        // sandbox; their values are masked so the transcript shows only shape.
        r#"printf "OPENAI_API_KEY=[%s]\n" "$OPENAI_API_KEY"; env | grep -E "^(COYOTE_|IS_SANDBOX|GOOGLE_API_KEY|EDITOR|BASH_ENV)" | sed 's/=sbx-cs-.*/=sbx-cs-<masked>/' | sort"#,
    )?;
    sbx_exec(
        "hooks path",
        &name,
        "git config --system --get core.hooksPath || echo NO_HOOKS_PATH",
    )?;
    sbx_exec(
        "login shell",
        &name,
        r#"bash -lc "echo BASH_ENV=[$BASH_ENV]; echo PATH=$PATH; command -v coyote""#,
    )?;

    Ok(())
}

/// The skip gate itself, exercised without Docker or a real sbx: a private
/// PATH holding only stubs decides the outcome, and the process PATH is
/// never touched.
#[cfg(unix)]
mod skip_gate {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    fn stub(bin: &Path, name: &str, body: &str) {
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A PATH with one dir holding a stub `sbx` that prints `version_line`,
    /// plus no-op `script` and `docker` stubs so only the version decides.
    fn path_with_stub_sbx(root: &Path, version_line: &str) -> OsString {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        stub(&bin, "sbx", &format!("echo '{version_line}'"));
        stub(&bin, "script", "exit 0");
        stub(&bin, "docker", "exit 0");
        bin.into_os_string()
    }

    #[test]
    fn an_old_sbx_turns_into_a_skip_reason() {
        let root = TempRoot::new("skip-old-sbx");
        let path = path_with_stub_sbx(root.path(), "sbx version v0.1.0");

        let reason = live_prereqs_in(&path).unwrap_err();

        assert!(reason.contains("below the minimum"), "{reason}");
        assert!(reason.contains("(0, 1, 0)"), "{reason}");
    }

    #[test]
    fn a_current_sbx_with_stub_docker_passes_the_gate() {
        let root = TempRoot::new("skip-current-sbx");
        let path = path_with_stub_sbx(root.path(), "sbx version v0.49.1 abcdef");

        live_prereqs_in(&path).unwrap();
    }

    #[test]
    fn a_missing_sbx_turns_into_a_skip_reason() {
        let root = TempRoot::new("skip-no-sbx");
        let empty = root.path().join("empty");
        fs::create_dir_all(&empty).unwrap();

        assert_eq!(
            live_prereqs_in(empty.as_os_str()).unwrap_err(),
            "sbx not on PATH"
        );
    }

    #[test]
    fn an_unreadable_sbx_version_turns_into_a_skip_reason() {
        let root = TempRoot::new("skip-garbled-sbx");
        let path = path_with_stub_sbx(root.path(), "something else entirely");

        let reason = live_prereqs_in(&path).unwrap_err();

        assert!(
            reason.starts_with("could not read the sbx version"),
            "{reason}"
        );
    }

    #[test]
    fn a_failing_docker_info_turns_into_a_skip_reason() {
        let root = TempRoot::new("skip-docker-down");
        let path = path_with_stub_sbx(root.path(), "sbx version v0.49.1");
        stub(&PathBuf::from(&path), "docker", "exit 1");

        assert_eq!(live_prereqs_in(&path).unwrap_err(), "docker info failed");
    }

    #[test]
    fn a_missing_script_turns_into_a_skip_reason() {
        let root = TempRoot::new("skip-no-script");
        let path = path_with_stub_sbx(root.path(), "sbx version v0.49.1");
        fs::remove_file(PathBuf::from(&path).join("script")).unwrap();

        assert_eq!(live_prereqs_in(&path).unwrap_err(), "script not on PATH");
    }

    #[test]
    fn a_missing_docker_turns_into_a_skip_reason() {
        let root = TempRoot::new("skip-no-docker");
        let path = path_with_stub_sbx(root.path(), "sbx version v0.49.1");
        fs::remove_file(PathBuf::from(&path).join("docker")).unwrap();

        assert_eq!(live_prereqs_in(&path).unwrap_err(), "docker not on PATH");
    }

    /// The guard removes the sandbox on the unwind path too: a panic inside
    /// the scope that holds it still reaches `sbx rm -f <name>`.
    #[test]
    fn the_sandbox_guard_removes_the_sandbox_when_the_test_panics() {
        let root = TempRoot::new("guard-unwind");
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = root.path().join("sbx-calls.log");
        stub(&bin, "sbx", &format!("echo \"$@\" >> '{}'", log.display()));

        let sbx = bin.join("sbx");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = SandboxGuard::new(sbx.clone(), "coyote-live-unwind");
            panic!("the test body failed");
        }));

        assert!(outcome.is_err());
        assert_eq!(
            fs::read_to_string(&log).unwrap().trim(),
            "rm -f coyote-live-unwind"
        );
    }
}

/// Both live tests stay opt-in: the attribute that keeps them out of a plain
/// `cargo test` is pinned on the source text, beside each live `fn`.
#[test]
fn every_live_test_is_marked_ignore() {
    let source = include_str!("live_tests.rs");
    let attribute = "#[ignore = \"needs sbx and Docker on the host; run with --ignored\"]";
    for live_test in [
        "fn inspect_accepts_every_shipped_and_generated_kit()",
        "fn create_exec_run_round_trip()",
    ] {
        let at = source.find(live_test).unwrap();
        let preceding = &source[..at];
        let last_attribute = preceding.rfind("#[").unwrap();
        assert!(
            preceding[last_attribute..].starts_with(attribute),
            "{live_test}"
        );
    }
}
