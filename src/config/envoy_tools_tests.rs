//! Runs the built-in envoy's `tools.py` end to end through the shim
//! `Functions::init_agent` builds, the way the agent loop does.
#![cfg(unix)]

use super::envoy::EnvoySource;
use super::reserved_agents::{BuiltinSourceGuard, ENVOY_AGENT_NAME};
use super::{
    Agent, AppState, BuiltinAgentSource, BuiltinAgentUnavailable, RequestContext,
    UnavailableReason, WorkingMode, builtin_agent_dir, paths,
};
use crate::function::{
    Functions, builtin_agent_child_env, builtin_agent_env, inherited_process_env, run_llm_function,
};
use crate::testing::{EnvVarGuard, TestConfigDirGuard};
use crate::utils::{create_abort_signal, get_env_name, temp_file};
use anyhow::Result;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::future::Future;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

const MAX_READ_BYTES: usize = 262_144;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GLOB_RESULTS: usize = 500;
const MAX_LINE_CHARS: usize = 2000;
const MAX_PATTERN_CHARS: usize = 256;
const TRUNCATED_MARK: &str = " [truncated]";

fn run_async<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn init_named(name: &str) -> Result<Agent> {
    let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
    let app_config = Arc::clone(&ctx.app.config);
    let model = ctx.current_model().clone();
    run_async(Agent::init(
        app_config.as_ref(),
        ctx.app.as_ref(),
        &model,
        false,
        name,
        create_abort_signal(),
    ))
}

/// The interpreter the production probe would pin, so the fixtures run the
/// tools under exactly what the envoy would.
fn python3() -> Option<PathBuf> {
    super::envoy::python_runtime().ok()
}

/// Removes a directory the test would otherwise leave behind on failure.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    _config: TestConfigDirGuard,
    _env: [EnvVarGuard; 2],
    _source: BuiltinSourceGuard,
    source: Arc<EnvoySource>,
    python: PathBuf,
    dir: PathBuf,
    shim: PathBuf,
    parent: PathBuf,
    root: PathBuf,
    outside: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.source.remove_dir();
        let _ = fs::remove_dir_all(&self.parent);
        let _ = fs::remove_dir_all(&self.outside);
    }
}

fn write(path: &Path, content: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// `None` when no python3 is on PATH, in which case the caller skips; CI
/// must have one, so there the absence is a failure rather than a skip.
fn fixture(label: &str) -> Option<Fixture> {
    let Some(python) = python3() else {
        if env::var_os("CI").is_some() {
            panic!("python3 is required on CI");
        }
        eprintln!("skipping: python3 not available");
        return None;
    };
    Some(fixture_with_python(label, python))
}

/// Builds the envoy shim as if `python` were the probed interpreter.
fn fixture_with_python(label: &str, python: PathBuf) -> Fixture {
    let config = TestConfigDirGuard::new(label);
    let env = [
        EnvVarGuard::unset("ENVOY_DATA_DIR"),
        EnvVarGuard::unset("ENVOY_CONFIG_FILE"),
    ];
    let probed = python.clone();
    let source = Arc::new(EnvoySource::with_probes(
        Box::new(move || Ok(probed.clone())),
        Box::new(|_| Ok(())),
    ));
    let source_guard = BuiltinSourceGuard::new(source.clone());
    let dir = builtin_agent_dir(ENVOY_AGENT_NAME).unwrap_or_else(|| {
        let reason = super::builtin_agent_unavailable_reason(ENVOY_AGENT_NAME);
        panic!("the envoy materializes: {reason:?}")
    });
    Functions::init_agent(ENVOY_AGENT_NAME, &[]).expect("the envoy shim builds");
    let shim = dir.join("bin").join(ENVOY_AGENT_NAME);
    assert!(shim.is_file(), "{}", shim.display());

    let outside = temp_file("-envoy-outside-", "");
    write(&outside.join("secret.txt"), "top secret\n");
    write(&outside.join("d").join("s.txt"), "also secret\n");

    let parent = temp_file("-envoy-fixture-", "");
    let root = parent.join("root");
    write(&root.join("a.txt"), "alpha\nbeta\ngamma\n");
    write(&root.join("sub").join("b.rs"), "fn main() {}\n// needle\n");
    let line = format!("{}\n", "0123456789".repeat(10));
    write(
        &root.join("big.txt"),
        line.repeat(MAX_READ_BYTES / line.len() + 2),
    );
    write(&root.join("wide.txt"), format!("{}\n", "x".repeat(2500)));
    write(&root.join("bin.dat"), b"abc\x00def");
    write(&root.join(".env"), "SECRET=1\n");
    write(&root.join(".env.local"), "SECRET=2\n");
    write(&root.join(".git").join("config"), "[core]\n");
    for index in 0..600 {
        write(
            &root.join("many").join(format!("f{index:03}.txt")),
            "needle\n",
        );
    }
    write(&parent.join("root2").join("leak.txt"), "leaked\n");
    symlink("a.txt", root.join("link_in")).unwrap();
    symlink(outside.join("secret.txt"), root.join("link_out_file")).unwrap();
    symlink(outside.join("d"), root.join("link_out_dir")).unwrap();
    symlink(parent.join("root2"), root.join("sib")).unwrap();

    Fixture {
        _config: config,
        _env: env,
        _source: source_guard,
        source,
        python,
        dir,
        shim,
        parent,
        root,
        outside,
    }
}

/// The deny list exactly as the production env builder hands it over.
fn deny_dirs_env() -> String {
    builtin_agent_env(Some(ENVOY_AGENT_NAME))
        .into_iter()
        .find(|(key, _)| key == "ENVOY_DENY_DIRS")
        .map(|(_, value)| value)
        .expect("the envoy env names its deny dirs")
}

fn shim_command(
    fx: &Fixture,
    shim: &Path,
    root: Option<&Path>,
    func: &str,
    args: &Value,
    output_file: &Path,
) -> Command {
    let mut command = Command::new(shim);
    command
        .arg(func)
        .arg(args.to_string())
        .env_clear()
        .envs(builtin_agent_child_env(&inherited_process_env()))
        .env("ENVOY_DATA_DIR", &fx.dir)
        .env("ENVOY_FUNCTIONS_DIR", fx.dir.join("functions"))
        .env("ENVOY_DENY_DIRS", deny_dirs_env())
        .env("LLM_OUTPUT", output_file);
    match root {
        Some(root) => command.env("ENVOY_ROOT_DIR", root),
        None => command.env_remove("ENVOY_ROOT_DIR"),
    };
    command
}

fn call(fx: &Fixture, root: Option<&Path>, func: &str, args: Value) -> Value {
    let output_file = temp_file("-envoy-tools-", ".json");
    let output = shim_command(fx, &fx.shim, root, func, &args, &output_file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{func} {args}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&output_file).unwrap();
    fs::remove_file(&output_file).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{func} {args}: {err}: {text}"))
}

fn refused(v: &Value) -> &str {
    v["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a refusal: {v}"))
}

fn read(fx: &Fixture, path: &str) -> Value {
    call(fx, Some(&fx.root), "fs_read", json!({"path": path}))
}

fn paths_of(v: &Value) -> Vec<&str> {
    v["paths"]
        .as_array()
        .unwrap_or_else(|| panic!("expected paths: {v}"))
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect()
}

fn is_denied(rel: &str) -> bool {
    rel.split('/')
        .any(|part| part == ".git" || part == ".env" || part.starts_with(".env."))
}

#[test]
#[serial]
fn traversal_and_absolute_paths_are_refused() {
    let Some(fx) = fixture("envoy-tools-traversal") else {
        return;
    };
    let absolute = fx.root.join("a.txt").display().to_string();
    for path in [
        "../x",
        "sub/../a.txt",
        "sub/../../x",
        "sub/../../root2/leak.txt",
        "../root2/leak.txt",
        absolute.as_str(),
        "/etc/hostname",
    ] {
        let error = refused(&read(&fx, path)).to_string();
        if path.starts_with('/') {
            assert!(
                error.contains("absolute paths are refused"),
                "{path}: {error}"
            );
        }
        for secret in [&fx.root, &fx.parent, &fx.dir] {
            assert!(
                !error.contains(&secret.display().to_string()),
                "{path}: refusal names '{}': {error}",
                secret.display()
            );
        }
    }
}

#[test]
#[serial]
fn symlinks_leaving_the_root_are_refused_but_in_root_links_work() {
    let Some(fx) = fixture("envoy-tools-symlinks") else {
        return;
    };
    refused(&read(&fx, "link_out_file"));
    refused(&read(&fx, "link_out_dir/s.txt"));
    let linked = read(&fx, "link_in");
    assert!(
        linked["content"].as_str().unwrap().contains("1: alpha"),
        "{linked}"
    );
}

#[test]
#[serial]
fn a_sibling_directory_sharing_the_root_prefix_is_not_inside_it() {
    let Some(fx) = fixture("envoy-tools-sibling") else {
        return;
    };
    assert!(fx.parent.join("root2").join("leak.txt").is_file());
    refused(&read(&fx, "sib/leak.txt"));
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&listing);
    assert!(!paths.is_empty(), "{listing}");
    assert!(paths.iter().all(|p| !p.contains("leak.txt")), "{listing}");
}

#[test]
#[serial]
fn secret_hygiene_denies_env_files_and_the_git_dir() {
    let Some(fx) = fixture("envoy-tools-secrets") else {
        return;
    };
    let named_secrets = [
        ".mcp.json",
        ".coyote_password",
        "prod.env",
        "docker.env",
        "terraform.tfstate.backup",
        "vars.tfvars",
    ];
    write(
        &fx.root.join(".mcp.json"),
        "{\"headers\":{\"Authorization\":\"Bearer SECRET-TOKEN\"}}\n",
    );
    for name in &named_secrets[1..] {
        write(&fx.root.join(name), "SECRET-TOKEN\n");
    }
    for path in [
        ".env",
        ".env.local",
        ".git/config",
        "sub/../.git/config",
        "./.env",
    ]
    .into_iter()
    .chain(named_secrets)
    {
        refused(&read(&fx, path));
    }
    let matches = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "SECRET|core|Bearer"}),
    );
    let found: Vec<&str> = matches["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert!(found.iter().all(|p| !is_denied(p)), "{matches}");
    assert!(match_texts(&matches).is_empty(), "{matches}");
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&listing);
    assert!(paths.contains(&"a.txt"), "{listing}");
    assert!(paths.iter().all(|p| !is_denied(p)), "{listing}");
    for name in named_secrets {
        assert!(!paths.contains(&name), "{name} listed: {listing}");
    }
}

/// A deny entry may name a file rather than a directory (Coyote's config
/// and env files): the file is refused by resolved path even though its
/// name matches no rule, and its siblings stay readable.
#[test]
#[serial]
fn deny_entries_naming_files_inside_the_root_hide_exactly_those_files() {
    let Some(fx) = fixture("envoy-tools-deny-files") else {
        return;
    };
    let keys = fx.root.join("secrets").join("keys.json");
    let config = fx.root.join("coyote.yaml");
    write(&keys, "KEY=hunter2\n");
    write(&config, "api_key: sk-secret\n");
    write(&fx.root.join("secrets").join("notes.txt"), "public notes\n");
    let sep = ":";
    let deny = format!("{}{sep}{}", keys.display(), config.display());

    for path in ["secrets/keys.json", "coyote.yaml"] {
        let args = json!({"path": path});
        let (value, output) = call_within(
            &fx,
            Some(&fx.root),
            "fs_read",
            args.clone(),
            Some(&deny),
            30,
        );
        assert_clean_exit("fs_read", &args, &output);
        let value = value.unwrap();
        refused(&value);
        assert!(value.get("content").is_none(), "{path}: {value}");
    }
    let args = json!({"path": "secrets/notes.txt"});
    let (value, output) = call_within(
        &fx,
        Some(&fx.root),
        "fs_read",
        args.clone(),
        Some(&deny),
        30,
    );
    assert_clean_exit("fs_read", &args, &output);
    assert_eq!(value.unwrap()["content"], json!("1: public notes"));

    for (pattern, expected) in [("secrets/*", vec!["secrets/notes.txt"]), ("*.yaml", vec![])] {
        let args = json!({"pattern": pattern});
        let (listing, output) = call_within(
            &fx,
            Some(&fx.root),
            "fs_glob",
            args.clone(),
            Some(&deny),
            30,
        );
        assert_clean_exit("fs_glob", &args, &output);
        let listing = listing.unwrap();
        assert_eq!(paths_of(&listing), expected, "{listing}");
    }

    let args = json!({"pattern": "hunter2|sk-secret|public notes"});
    let (grep, output) = call_within(
        &fx,
        Some(&fx.root),
        "fs_grep",
        args.clone(),
        Some(&deny),
        30,
    );
    assert_clean_exit("fs_grep", &args, &output);
    let grep = grep.unwrap();
    assert_eq!(match_texts(&grep), vec!["public notes"], "{grep}");
}

/// The deny list is compared on normalized names, so a case-insensitive
/// filesystem (this repo's own mount included) cannot be used to read `.env`
/// as `.ENV`, and Windows trailing dots or `::$DATA` streams do not help
/// either.
#[test]
#[serial]
fn deny_names_hold_under_case_folding_and_windows_name_forms() {
    let Some(fx) = fixture("envoy-tools-casefold") else {
        return;
    };
    write(&fx.root.join("probe-case"), "probe\n");
    let case_insensitive = fx.root.join("PROBE-CASE").exists();
    write(&fx.root.join(".ssh").join("id_ed25519"), "PRIVATE KEY\n");
    write(&fx.root.join("server.pem"), "PRIVATE KEY\n");
    write(&fx.root.join("cert-12:00.pem"), "PRIVATE KEY\n");
    write(&fx.root.join(".aws").join("credentials"), "aws_secret\n");

    let case_variants = [
        ".ENV",
        ".Env.local",
        ".GIT/config",
        ".SSH/id_ed25519",
        ".ssh/ID_ED25519",
        "SERVER.PEM",
        ".Aws/credentials",
    ];
    let windows_forms = [
        ".env.",
        ".env ",
        ".env::$DATA",
        ".git./config",
        ".ssh /id_ed25519",
    ];
    // The colon-stripped form `cert-12` matches no rule; the name as written
    // must still be denied by its suffix.
    let colon_forms = ["cert-12:00.pem"];
    for path in case_variants
        .iter()
        .chain(&windows_forms)
        .chain(&colon_forms)
    {
        let result = read(&fx, path);
        assert!(
            refused(&result).starts_with("refused:"),
            "{path}: the deny check must fire before any existence check: {result}"
        );
        assert!(result.get("content").is_none(), "{path}: {result}");
    }
    if case_insensitive {
        for path in case_variants {
            assert!(
                fx.root.join(path).is_file(),
                "{path}: on this filesystem the variant opens the real file, so only the deny check stood in the way"
            );
        }
    }
    for (func, args) in [
        ("fs_grep", json!({"pattern": "SECRET", "path": ".Env"})),
        ("fs_grep", json!({"pattern": "core", "path": ".Git"})),
        ("fs_glob", json!({"pattern": "*", "path": ".GIT"})),
        ("fs_glob", json!({"pattern": "*", "path": ".Ssh"})),
    ] {
        let result = call(&fx, Some(&fx.root), func, args.clone());
        refused(&result);
        assert!(
            result.get("paths").is_none() && result.get("matches").is_none(),
            "{func} {args}: {result}"
        );
    }
    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "PRIVATE|aws_secret|SECRET"}),
    );
    assert!(match_texts(&grep).is_empty(), "{grep}");
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&listing);
    assert!(paths.contains(&"probe-case"), "{listing}");
    assert!(!paths.contains(&"cert-12:00.pem"), "{listing}");
    assert!(
        paths.iter().all(|p| {
            !p.contains(".ssh") && !p.contains(".aws") && !p.ends_with(".pem") && !is_denied(p)
        }),
        "{listing}"
    );
}

/// Coyote's own config and cache dirs hold API keys, OAuth tokens and
/// transcripts; when the REPL is launched from their parent (`$HOME`) the
/// tools must still refuse them, by resolved path rather than by name.
#[test]
#[serial]
fn coyotes_config_and_cache_dirs_are_refused_wherever_the_root_is() {
    let Some(fx) = fixture("envoy-tools-deny-dirs") else {
        return;
    };
    let config_dir = fx.root.join("cfg").join("coyote");
    let cache_dir = fx.root.join("cache").join("coyote");
    write(&config_dir.join("config.yaml"), "api_key: sk-secret\n");
    write(
        &config_dir.join("oauth_tokens").join("github.json"),
        "{\"access_token\": \"gho_secret\"}\n",
    );
    write(
        &cache_dir.join("transcripts").join("t.md"),
        "PRIVATE chat\n",
    );
    write(&fx.root.join("cfg").join("readme.txt"), "public\n");
    symlink(&config_dir, fx.root.join("cfg_alias")).unwrap();
    let _config = EnvVarGuard::set(get_env_name("config_dir"), &config_dir);
    let _cache = EnvVarGuard::set(get_env_name("cache_dir"), &cache_dir);
    assert_eq!(paths::config_dir(), config_dir);
    assert!(
        fx.root
            .join("cfg")
            .join("coyote")
            .join("config.yaml")
            .is_file()
    );

    for path in [
        "cfg/coyote/config.yaml",
        "cfg/coyote/oauth_tokens/github.json",
        "cache/coyote/transcripts/t.md",
        "cfg_alias/config.yaml",
        "cfg/coyote",
    ] {
        let result = read(&fx, path);
        let error = refused(&result).to_string();
        assert!(result.get("content").is_none(), "{path}: {result}");
        assert!(
            !error.contains(&fx.root.display().to_string()),
            "{path}: {error}"
        );
    }
    for (func, args) in [
        ("fs_glob", json!({"pattern": "*", "path": "cfg/coyote"})),
        ("fs_glob", json!({"pattern": "*", "path": "cfg_alias"})),
        (
            "fs_grep",
            json!({"pattern": "api_key", "path": "cache/coyote"}),
        ),
        (
            "fs_grep",
            json!({"pattern": "api_key", "path": "cfg/coyote/config.yaml"}),
        ),
    ] {
        let result = call(&fx, Some(&fx.root), func, args.clone());
        refused(&result);
        assert!(
            result.get("paths").is_none() && result.get("matches").is_none(),
            "{func} {args}: {result}"
        );
    }

    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&listing);
    assert!(paths.contains(&"cfg/readme.txt"), "{listing}");
    assert!(
        paths
            .iter()
            .all(|p| !p.starts_with("cfg/coyote") && !p.starts_with("cache/")),
        "{listing}"
    );
    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "api_key|gho_|PRIVATE"}),
    );
    assert!(match_texts(&grep).is_empty(), "{grep}");
    let grep = call(&fx, Some(&fx.root), "fs_grep", json!({"pattern": "public"}));
    assert_eq!(match_texts(&grep), vec!["public"], "{grep}");
}

#[test]
#[serial]
fn happy_paths_return_root_relative_posix_paths() {
    let Some(fx) = fixture("envoy-tools-happy") else {
        return;
    };
    let a = read(&fx, "a.txt");
    assert_eq!(a["path"], json!("a.txt"));
    assert_eq!(a["lines"], json!(3));
    assert_eq!(a["content"], json!("1: alpha\n2: beta\n3: gamma"));
    assert_eq!(a["truncated"], json!(false));

    let last = call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "sub/b.rs", "offset": 2, "limit": 1}),
    );
    assert_eq!(last["content"], json!("2: // needle"));
    assert_eq!(last["truncated"], json!(false), "{last}");
    let first = call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "sub/b.rs", "offset": 1, "limit": 1}),
    );
    assert_eq!(first["content"], json!("1: fn main() {}"));
    assert_eq!(first["truncated"], json!(true), "{first}");

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "needle", "path": "sub", "include": "*.rs"}),
    );
    let matches = grep["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{grep}");
    assert_eq!(matches[0]["path"], json!("sub/b.rs"));
    assert_eq!(matches[0]["line"], json!(2));
    assert_eq!(grep["truncated"], json!(false));
    assert_eq!(grep["budget_exhausted"], json!(false));

    let rs = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*.rs"}));
    assert_eq!(paths_of(&rs), vec!["sub/b.rs"]);
    let dot_slash = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "./*.rs"}));
    assert_eq!(paths_of(&dot_slash), vec!["sub/b.rs"], "{dot_slash}");
    let dot_slash_scoped = call(
        &fx,
        Some(&fx.root),
        "fs_glob",
        json!({"pattern": "././sub/*.rs", "path": "."}),
    );
    assert_eq!(
        paths_of(&dot_slash_scoped),
        vec!["sub/b.rs"],
        "{dot_slash_scoped}"
    );
    let none = call(
        &fx,
        Some(&fx.root),
        "fs_glob",
        json!({"pattern": "*.txt", "path": "sub"}),
    );
    assert!(paths_of(&none).is_empty(), "{none}");
}

/// `str.splitlines` also breaks on form feeds and Unicode separators, which
/// editors and `grep -n` do not; line numbers must agree with them.
#[test]
#[serial]
fn line_numbers_ignore_form_feeds_and_unicode_separators() {
    let Some(fx) = fixture("envoy-tools-line-breaks") else {
        return;
    };
    write(
        &fx.root.join("pages.txt"),
        "one\ntwo\x0cstill two\u{2028}same line\nthree\n",
    );

    let pages = read(&fx, "pages.txt");
    assert_eq!(pages["lines"], json!(3), "{pages}");
    assert_eq!(pages["total_lines_read"], json!(3), "{pages}");
    assert_eq!(
        pages["content"],
        json!("1: one\n2: two\x0cstill two\u{2028}same line\n3: three")
    );

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "still two|three", "path": "pages.txt"}),
    );
    let lines: Vec<_> = grep["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, vec![2, 3], "{grep}");
}

#[test]
#[serial]
fn caps_are_enforced_and_reported() {
    let Some(fx) = fixture("envoy-tools-caps") else {
        return;
    };
    let big = read(&fx, "big.txt");
    assert_eq!(big["truncated"], json!(true), "{big}");

    let wide = read(&fx, "wide.txt");
    let content = wide["content"].as_str().unwrap();
    assert!(content.ends_with(TRUNCATED_MARK), "{content}");
    assert!(
        content.len() <= "1: ".len() + MAX_LINE_CHARS + TRUNCATED_MARK.len(),
        "{}",
        content.len()
    );
    assert_eq!(wide["lines_clipped"], json!(1), "{wide}");
    assert_eq!(wide["truncated"], json!(false), "{wide}");
    let a = read(&fx, "a.txt");
    assert_eq!(a["lines_clipped"], json!(0), "{a}");

    let binary = refused(&read(&fx, "bin.dat")).to_string();
    assert!(binary.contains("binary"), "{binary}");

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "needle", "path": "many"}),
    );
    assert_eq!(grep["matches"].as_array().unwrap().len(), MAX_GREP_MATCHES);
    assert_eq!(grep["truncated"], json!(true));

    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "many/*"}));
    assert_eq!(paths_of(&listing).len(), MAX_GLOB_RESULTS);
    assert_eq!(listing["truncated"], json!(true));

    refused(&call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt", "offset": 0}),
    ));
}

/// A peer-supplied regex gets a length cap and the whole search a time
/// budget, so a pathological pattern cannot hold the tool until the
/// process-level timeout.
#[test]
#[serial]
fn patterns_are_capped_and_searches_report_their_budget() {
    let Some(fx) = fixture("envoy-tools-pattern-cap") else {
        return;
    };
    let at_cap = "a".repeat(MAX_PATTERN_CHARS);
    let over_cap = "a".repeat(MAX_PATTERN_CHARS + 1);
    for func in ["fs_grep", "fs_glob"] {
        let ok = call(&fx, Some(&fx.root), func, json!({"pattern": at_cap}));
        assert!(ok.get("error").is_none(), "{func} at the cap: {ok}");
        let error = refused(&call(
            &fx,
            Some(&fx.root),
            func,
            json!({"pattern": over_cap}),
        ))
        .to_string();
        assert!(error.contains("256"), "{func}: {error}");
    }
    let ok = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "alpha", "include": at_cap}),
    );
    assert!(ok.get("error").is_none(), "include at the cap: {ok}");
    let error = refused(&call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "alpha", "include": over_cap}),
    ))
    .to_string();
    assert!(
        error.contains("include") && error.contains("256"),
        "{error}"
    );
    let grep = call(&fx, Some(&fx.root), "fs_grep", json!({"pattern": "alpha"}));
    assert_eq!(grep["budget_exhausted"], json!(false), "{grep}");
    assert_eq!(grep["truncated"], json!(false), "{grep}");
    // Both a.txt and the in-root link_in -> a.txt carry the line.
    assert_eq!(match_texts(&grep), vec!["alpha", "alpha"]);
    let mut hit_paths: Vec<&str> = grep["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    hit_paths.sort_unstable();
    assert_eq!(hit_paths, vec!["a.txt", "link_in"]);
}

#[test]
#[serial]
fn a_missing_or_invalid_root_refuses_everything() {
    let Some(fx) = fixture("envoy-tools-no-root") else {
        return;
    };
    let file = fx.root.join("a.txt");
    for root in [
        None,
        Some(Path::new("")),
        Some(Path::new(".")),
        Some(file.as_path()),
    ] {
        for (func, args) in [
            ("fs_read", json!({"path": "a.txt"})),
            ("fs_grep", json!({"pattern": "alpha"})),
            ("fs_glob", json!({"pattern": "*"})),
        ] {
            let result = call(&fx, root, func, args);
            assert!(
                refused(&result).contains("ENVOY_ROOT_DIR"),
                "{func} with root {root:?}: {result}"
            );
            assert!(result.get("paths").is_none(), "{result}");
        }
    }
    for (func, args) in [
        ("fs_read", json!({"path": "a.txt"})),
        ("fs_grep", json!({"pattern": "alpha"})),
        ("fs_glob", json!({"pattern": "*"})),
    ] {
        let (value, output) = call_without_deny_dirs(&fx, func, args.clone());
        assert_clean_exit(func, &args, &output);
        let value = value.unwrap();
        assert!(
            refused(&value).contains("ENVOY_DENY_DIRS"),
            "{func} with no deny list: {value}"
        );
        assert!(
            value.get("content").is_none() && value.get("paths").is_none(),
            "{value}"
        );
    }
}

#[test]
#[serial]
fn the_production_path_pins_the_root_to_the_process_cwd() {
    let Some(fx) = fixture("envoy-tools-cwd") else {
        return;
    };
    let cwd = env::current_dir().unwrap();
    assert!(cwd.join("Cargo.toml").is_file(), "{}", cwd.display());
    let probe_name = format!("envoy-probe-{}", uuid::Uuid::new_v4());
    let probe_dir = RemoveOnDrop(cwd.join("target").join(&probe_name));
    write(&probe_dir.0.join("probe.txt"), "probe\n");
    let rel = format!("target/{probe_name}/probe.txt");

    let run = |path: &str| -> Value {
        let output = run_llm_function(
            fx.shim.display().to_string(),
            vec!["fs_read".into(), json!({"path": path}).to_string()],
            HashMap::new(),
            Some(ENVOY_AGENT_NAME.to_string()),
            None,
            false,
            None,
        )
        .unwrap()
        .expect("the shim writes to LLM_OUTPUT");
        serde_json::from_str(&output).unwrap()
    };

    let probe = run(&rel);
    assert_eq!(probe["path"], json!(rel), "{probe}");
    assert_eq!(probe["content"], json!("1: probe"));
    refused(&run(".git/config"));
    let manifest = run("Cargo.toml");
    assert_eq!(manifest["path"], json!("Cargo.toml"), "{manifest}");
}

fn run_in(fx: &Fixture, shim: &Path, cwd: &Path) -> (Output, PathBuf) {
    let output_file = temp_file("-envoy-tools-", ".json");
    let output = shim_command(
        fx,
        shim,
        Some(cwd),
        "fs_glob",
        &json!({"pattern": "*"}),
        &output_file,
    )
    .current_dir(cwd)
    .output()
    .unwrap();
    (output, output_file)
}

#[test]
#[serial]
fn the_builtin_shim_ignores_a_venv_in_the_cwd() {
    let Some(fx) = fixture("envoy-tools-venv") else {
        return;
    };
    let fake_cwd = RemoveOnDrop(temp_file("-envoy-venv-cwd-", ""));
    let venv_python = fake_cwd.0.join(".venv").join("bin").join("python");
    write(&venv_python, "#!/bin/sh\necho VENV_ADOPTED >&2\nexit 99\n");
    fs::set_permissions(&venv_python, fs::Permissions::from_mode(0o755)).unwrap();

    let script = fx.dir.join("bin").join("run-envoy.py");
    let wrapper_text = fs::read_to_string(&fx.shim).unwrap();
    let script_text = fs::read_to_string(&script).unwrap();
    for (name, text) in [("envoy", &wrapper_text), ("run-envoy.py", &script_text)] {
        for hazard in [".venv", "activate", "execv"] {
            assert!(!text.contains(hazard), "{name} {hazard}: {text}");
        }
    }
    assert!(wrapper_text.starts_with("#!/bin/sh\n"), "{wrapper_text}");
    let exec_line = wrapper_text
        .lines()
        .find(|line| line.starts_with("exec \""))
        .unwrap_or_else(|| panic!("no quoted exec line: {wrapper_text}"));
    assert!(
        exec_line.contains(&format!("\"{}\" -I ", fx.python.display())),
        "{exec_line}"
    );
    assert!(!script_text.starts_with("#!"), "{script_text}");
    assert_eq!(
        fs::metadata(&fx.shim).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let (output, output_file) = run_in(&fx, &fx.shim, &fake_cwd.0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("VENV_ADOPTED"), "{stderr}");
    let listing: Value = serde_json::from_str(&fs::read_to_string(&output_file).unwrap()).unwrap();
    fs::remove_file(&output_file).unwrap();
    assert!(listing.get("paths").is_some(), "{listing}");

    // Control: a user agent's shim, built by the same code path, still
    // re-execs into the cwd venv.
    let control_dir = paths::agents_data_dir().join("control");
    write(
        &control_dir.join("tools.py"),
        "#!/usr/bin/env python3\n\n\ndef fs_glob(pattern: str) -> dict:\n    \"\"\"List.\n\n    Args:\n        pattern: Glob.\n    \"\"\"\n    return {\"paths\": []}\n",
    );
    Functions::init_agent("control", &[]).expect("the control shim builds");
    let control_shim = paths::agent_bin_dir("control").join("control");
    let control_text = fs::read_to_string(&control_shim).unwrap();
    assert!(control_text.contains("execv"), "{control_text}");
    assert!(
        !paths::agent_bin_dir("control")
            .join("run-control.py")
            .exists(),
        "a user agent's Python shim is the script itself"
    );
    let (output, output_file) = run_in(&fx, &control_shim, &fake_cwd.0);
    let _ = fs::remove_file(&output_file);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(99), "{stderr}");
    assert!(stderr.contains("VENV_ADOPTED"), "{stderr}");
}

/// The probed interpreter path is quoted into the launcher, so a path with
/// a space (a Windows-style install dir, a user's `~/py dir`) still runs.
#[test]
#[serial]
fn an_interpreter_path_with_a_space_still_launches_the_tools() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not available");
        return;
    };
    let spaced_home = RemoveOnDrop(temp_file("-envoy-spaced-", ""));
    let spaced = spaced_home.0.join("py dir").join("python3");
    fs::create_dir_all(spaced.parent().unwrap()).unwrap();
    symlink(&python, &spaced).unwrap();
    let fx = fixture_with_python("envoy-tools-spaced", spaced.clone());

    let wrapper_text = fs::read_to_string(&fx.shim).unwrap();
    assert!(
        wrapper_text.contains(&format!("exec \"{}\" -I ", spaced.display())),
        "{wrapper_text}"
    );
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*.rs"}));
    assert_eq!(paths_of(&listing), vec!["sub/b.rs"]);
    let a = read(&fx, "a.txt");
    assert_eq!(a["content"], json!("1: alpha\n2: beta\n3: gamma"), "{a}");
}

#[test]
#[serial]
fn a_missing_interpreter_is_reported_to_the_human_path() {
    let _config = TestConfigDirGuard::new("envoy-tools-no-python");
    let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
    let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
    let candidates = vec!["python3".to_string(), "python".to_string()];
    let missing = candidates.clone();
    let source = Arc::new(EnvoySource::with_probes(
        Box::new(move || {
            Err(UnavailableReason::RuntimeMissing {
                candidates: missing.clone(),
            })
        }),
        Box::new(|_| Ok(())),
    ));
    let _source = BuiltinSourceGuard::new(source.clone());

    let err = init_named(ENVOY_AGENT_NAME).unwrap_err();
    let unavailable = err
        .downcast_ref::<BuiltinAgentUnavailable>()
        .unwrap_or_else(|| panic!("{err:#}"));
    assert_eq!(
        unavailable.reason,
        UnavailableReason::RuntimeMissing {
            candidates: candidates.clone()
        }
    );
    assert!(format!("{err:#}").contains("python3"), "{err:#}");

    let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
    let app = ctx.app.config.clone();
    let err =
        run_async(ctx.use_agent(&app, ENVOY_AGENT_NAME, None, create_abort_signal())).unwrap_err();
    assert!(format!("{err:#}").contains("python3"), "{err:#}");
    assert!(ctx.agent.is_none());
    assert!(builtin_agent_dir(ENVOY_AGENT_NAME).is_none());
    source.remove_dir();
}

#[test]
#[serial]
fn an_unusable_interpreter_names_what_was_tried_on_the_human_path() {
    let _config = TestConfigDirGuard::new("envoy-tools-old-python");
    let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
    let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
    let tried = vec![(
        "/opt/old/bin/python3".to_string(),
        "older than Python 3.9".to_string(),
    )];
    let reported = tried.clone();
    let source = Arc::new(EnvoySource::with_probes(
        Box::new(move || {
            Err(UnavailableReason::RuntimeUnusable {
                tried: reported.clone(),
            })
        }),
        Box::new(|_| panic!("the exec probe must not run without an interpreter")),
    ));
    let _source = BuiltinSourceGuard::new(source.clone());

    let err = init_named(ENVOY_AGENT_NAME).unwrap_err();
    let unavailable = err
        .downcast_ref::<BuiltinAgentUnavailable>()
        .unwrap_or_else(|| panic!("{err:#}"));
    assert_eq!(
        unavailable.reason,
        UnavailableReason::RuntimeUnusable { tried }
    );
    let text = format!("{err:#}");
    assert!(text.contains("/opt/old/bin/python3"), "{text}");
    assert!(text.contains("older than Python 3.9"), "{text}");
    assert!(builtin_agent_dir(ENVOY_AGENT_NAME).is_none());
    source.remove_dir();
}

/// Runs the shim without asserting success; `None` when it wrote no JSON.
fn call_raw(fx: &Fixture, root: Option<&Path>, func: &str, args: Value) -> (Output, Option<Value>) {
    let output_file = temp_file("-envoy-tools-", ".json");
    let output = shim_command(fx, &fx.shim, root, func, &args, &output_file)
        .output()
        .unwrap();
    let value = fs::read_to_string(&output_file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let _ = fs::remove_file(&output_file);
    (output, value)
}

fn match_texts(v: &Value) -> Vec<String> {
    v["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("expected matches: {v}"))
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect()
}

/// The denied set (`.env`, `.env.*`, anything under `.git/`) is a property
/// of the file, so reaching one through an in-root symlink must be refused
/// by every tool, not only by `fs_read`.
#[test]
#[serial]
fn denied_files_reached_through_in_root_symlinks_are_refused_by_every_tool() {
    let Some(fx) = fixture("envoy-tools-alias-denied") else {
        return;
    };
    symlink(".env", fx.root.join("alias_env")).unwrap();
    symlink(".git/config", fx.root.join("alias_gitcfg")).unwrap();
    symlink(".git", fx.root.join("alias_git")).unwrap();

    for path in ["alias_env", "alias_gitcfg", "alias_git/config"] {
        refused(&read(&fx, path));
    }

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "SECRET|core"}),
    );
    let texts = match_texts(&grep);
    assert!(
        texts
            .iter()
            .all(|t| !t.contains("SECRET=1") && !t.contains("[core]")),
        "fs_grep surfaced a denied file's content through an in-root symlink: {grep}"
    );
    let single = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "SECRET", "path": "alias_env"}),
    );
    refused(&single);
    let listed = call(
        &fx,
        Some(&fx.root),
        "fs_glob",
        json!({"pattern": "*", "path": "alias_gitcfg"}),
    );
    refused(&listed);
}

/// The root is `realpath`'d, so a root handed over through a symlink
/// confines exactly like its target and reports the same relative paths.
#[test]
#[serial]
fn a_symlinked_root_confines_like_its_target() {
    let Some(fx) = fixture("envoy-tools-rootlink") else {
        return;
    };
    let rootlink = fx.parent.join("rootlink");
    symlink(&fx.root, &rootlink).unwrap();

    let a = call(&fx, Some(&rootlink), "fs_read", json!({"path": "a.txt"}));
    assert_eq!(a["path"], json!("a.txt"), "{a}");
    assert_eq!(a["content"], json!("1: alpha\n2: beta\n3: gamma"));
    for path in ["../root2/leak.txt", "sib/leak.txt", "link_out_file", ".env"] {
        let error = refused(&call(
            &fx,
            Some(&rootlink),
            "fs_read",
            json!({"path": path}),
        ))
        .to_string();
        assert!(
            !error.contains(&fx.parent.display().to_string()),
            "{path}: {error}"
        );
    }
    let rs = call(&fx, Some(&rootlink), "fs_glob", json!({"pattern": "*.rs"}));
    assert_eq!(paths_of(&rs), vec!["sub/b.rs"]);
    let grep = call(
        &fx,
        Some(&rootlink),
        "fs_grep",
        json!({"pattern": "top secret|also secret|leaked|SECRET"}),
    );
    assert!(match_texts(&grep).is_empty(), "{grep}");
}

/// `fs_glob`/`fs_grep` never follow out-of-root symlinked dirs, whether
/// they meet one while walking or are pointed straight at it via `path`;
/// in-root dir symlinks stay readable by `fs_read` and are not walked twice.
/// The walk also prunes dirs whose real path leaves the root (a Windows
/// junction is not a link to `islink`); on unix that is indistinguishable
/// from the symlink prune, so `sub/b.rs` being listed is what shows real
/// in-root dirs are still walked.
#[test]
#[serial]
fn grep_and_glob_never_follow_symlinks_out_of_the_root() {
    let Some(fx) = fixture("envoy-tools-walk") else {
        return;
    };
    symlink("sub", fx.root.join("sub_alias")).unwrap();

    for (func, args) in [
        ("fs_glob", json!({"pattern": "*", "path": "link_out_dir"})),
        ("fs_glob", json!({"pattern": "*", "path": "sib"})),
        ("fs_glob", json!({"pattern": "*", "path": "link_out_file"})),
        (
            "fs_grep",
            json!({"pattern": "secret", "path": "link_out_dir"}),
        ),
        (
            "fs_grep",
            json!({"pattern": "secret", "path": "link_out_file"}),
        ),
        ("fs_grep", json!({"pattern": "leaked", "path": "sib"})),
        (
            "fs_grep",
            json!({"pattern": "leaked", "path": "sib/leak.txt"}),
        ),
    ] {
        let result = call(&fx, Some(&fx.root), func, args.clone());
        refused(&result);
        assert!(
            result.get("paths").is_none() && result.get("matches").is_none(),
            "{func} {args}: {result}"
        );
    }

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "top secret|also secret|leaked"}),
    );
    assert!(match_texts(&grep).is_empty(), "{grep}");
    let everything = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&everything);
    assert!(
        paths.iter().all(|p| !p.starts_with("link_out_dir/")
            && !p.starts_with("sib/")
            && !p.contains("s.txt")),
        "{everything}"
    );

    let via_alias = read(&fx, "sub_alias/b.rs");
    assert_eq!(via_alias["path"], json!("sub_alias/b.rs"), "{via_alias}");
    assert!(via_alias["content"].as_str().unwrap().contains("needle"));
    let rs = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*.rs"}));
    assert_eq!(
        paths_of(&rs),
        vec!["sub/b.rs"],
        "an in-root dir symlink must not double-list"
    );
}

/// `.env`, `.env.*`, `.envrc` and `.git/` are denied at any depth, and
/// denied dirs cannot be used as the `path` of a walk.
#[test]
#[serial]
fn nested_denied_names_are_refused_at_any_depth() {
    let Some(fx) = fixture("envoy-tools-nested-denied") else {
        return;
    };
    write(&fx.root.join("sub").join(".env"), "NESTED_SECRET=2\n");
    write(&fx.root.join("sub").join(".env.prod"), "NESTED_SECRET=3\n");
    write(
        &fx.root.join("sub").join(".git").join("HEAD"),
        "ref: nested\n",
    );
    write(&fx.root.join(".envrc"), "use flake\n");

    for path in [
        "sub/.env",
        "sub/.env.prod",
        "sub/.git/HEAD",
        "sub/.git",
        ".git",
        ".envrc",
    ] {
        refused(&read(&fx, path));
    }
    for (func, args) in [
        ("fs_glob", json!({"pattern": "*", "path": ".git"})),
        ("fs_glob", json!({"pattern": "*", "path": "sub/.git"})),
        ("fs_grep", json!({"pattern": "core", "path": ".git"})),
        ("fs_grep", json!({"pattern": "NESTED", "path": "sub/.env"})),
    ] {
        refused(&call(&fx, Some(&fx.root), func, args));
    }

    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&listing);
    assert!(paths.iter().all(|p| !is_denied(p)), "{listing}");
    assert!(!paths.contains(&".envrc"), "{listing}");
    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "NESTED_SECRET|ref: nested|use flake"}),
    );
    assert!(match_texts(&grep).is_empty(), "{grep}");
}

/// Refusals and malformed input come back as `{"error": ...}` results for
/// the cases the contract names; for anything else the shim must at least
/// never answer a wrong request with a success-shaped result.
#[test]
#[serial]
fn malformed_requests_never_yield_a_success_shaped_result() {
    let Some(fx) = fixture("envoy-tools-malformed") else {
        return;
    };
    write(&fx.root.join("latin1.txt"), b"caf\xe9 au lait\nok\n");

    let bad_regex = call(&fx, Some(&fx.root), "fs_grep", json!({"pattern": "["}));
    assert!(
        refused(&bad_regex).contains("invalid pattern"),
        "{bad_regex}"
    );
    let dir = read(&fx, "sub");
    assert!(refused(&dir).contains("directory"), "{dir}");
    let dot = read(&fx, ".");
    refused(&dot);
    refused(&read(&fx, "nope.txt"));
    refused(&call(&fx, Some(&fx.root), "fs_read", json!({"path": 123})));
    refused(&call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt", "offset": 1, "limit": 0}),
    ));

    let latin1 = read(&fx, "latin1.txt");
    assert_eq!(latin1["lines"], json!(2), "{latin1}");
    assert!(
        latin1["content"].as_str().unwrap().contains("2: ok"),
        "non UTF-8 bytes must be replaced, not fatal: {latin1}"
    );

    let past_eof = call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt", "offset": 99}),
    );
    assert_eq!(past_eof["lines"], json!(0), "{past_eof}");
    assert_eq!(past_eof["content"], json!(""), "{past_eof}");

    for (func, args) in [
        ("fs_read", json!({"path": "a.txt", "offset": "2"})),
        ("fs_read", json!({"path": "a.txt", "limit": "x"})),
        ("fs_read", json!({"path": "a.txt", "offset": 1.5})),
        ("fs_read", json!({"path": "a\u{0}b"})),
        ("fs_grep", json!({"pattern": 123})),
        ("fs_glob", json!({"pattern": null})),
        ("fs_read", json!({"path": "a.txt", "bogus": 1})),
    ] {
        let (output, value) = call_raw(&fx, Some(&fx.root), func, args.clone());
        let success_shaped = value.as_ref().is_some_and(|v| v.get("error").is_none());
        assert!(
            !(output.status.success() && success_shaped),
            "{func} {args}: answered a malformed request with a success result: {value:?}"
        );
    }
}

/// Wrong argument types and unreadable files come back as `{"error": ...}`
/// with exit 0, never as a traceback: `run_llm_function` returns stderr to
/// the model, and a traceback would name the envoy dir and the root.
#[test]
#[serial]
fn type_errors_and_unreadable_files_are_reported_without_a_traceback() {
    let Some(fx) = fixture("envoy-tools-no-traceback") else {
        return;
    };
    let mut cases = vec![
        ("fs_read", json!({"path": "a.txt", "offset": "2"})),
        ("fs_read", json!({"path": "a.txt", "offset": 1.5})),
        ("fs_read", json!({"path": "a.txt", "limit": true})),
        ("fs_read", json!({"path": "a.txt", "offset": [1]})),
        ("fs_grep", json!({"pattern": 123})),
        ("fs_grep", json!({"pattern": null})),
        ("fs_grep", json!({"pattern": "a", "include": 5})),
        ("fs_grep", json!({"pattern": "a", "path": ["sub"]})),
        ("fs_glob", json!({"pattern": null})),
        ("fs_glob", json!({"pattern": "*", "path": 7})),
    ];
    let locked = fx.root.join("locked.txt");
    write(&locked, "hidden\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let locked_is_unreadable = fs::read(&locked).is_err();
    if locked_is_unreadable {
        cases.push(("fs_read", json!({"path": "locked.txt"})));
    } else {
        eprintln!("skipping the mode-000 case: this user can read it anyway");
    }

    for (func, args) in &cases {
        let (output, value) = call_raw(&fx, Some(&fx.root), func, args.clone());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{func} {args}: exit {:?}: {stderr}",
            output.status.code()
        );
        assert!(!stderr.contains("Traceback"), "{func} {args}: {stderr}");
        let value = value.unwrap_or_else(|| panic!("{func} {args}: no JSON result"));
        let error = refused(&value).to_string();
        assert!(value.get("content").is_none(), "{func} {args}: {value}");
        for secret in [&fx.root, &fx.parent, &fx.dir] {
            assert!(
                !error.contains(&secret.display().to_string()),
                "{func} {args}: refusal names '{}': {error}",
                secret.display()
            );
        }
    }
    if locked_is_unreadable {
        let error = refused(&read(&fx, "locked.txt")).to_string();
        assert!(error.contains("unreadable: locked.txt"), "{error}");
        let grep = call(
            &fx,
            Some(&fx.root),
            "fs_grep",
            json!({"pattern": "hidden|alpha"}),
        );
        let texts = match_texts(&grep);
        assert!(
            texts.contains(&"alpha".to_string()) && !texts.contains(&"hidden".to_string()),
            "an unreadable file is skipped, not fatal: {grep}"
        );
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
}

/// A FIFO named as `path` must be refused up front rather than opened: a
/// read on it would block until the process-level timeout.
#[test]
#[serial]
fn a_fifo_is_refused_instead_of_read() {
    let Some(fx) = fixture("envoy-tools-fifo") else {
        return;
    };
    let fifo = fx.root.join("pipe");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success(), "mkfifo failed");
    for (func, args) in [
        ("fs_read", json!({"path": "pipe"})),
        ("fs_grep", json!({"pattern": "x", "path": "pipe"})),
        ("fs_glob", json!({"pattern": "*", "path": "pipe"})),
    ] {
        let result = call(&fx, Some(&fx.root), func, args.clone());
        assert!(
            refused(&result).contains("not a regular file"),
            "{func} {args}: {result}"
        );
    }
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    assert!(!paths_of(&listing).contains(&"pipe"), "{listing}");
}

/// The shim advertises exactly `fs_read(path, offset?, limit?)`,
/// `fs_grep(pattern, path?, include?)` and `fs_glob(pattern, path?)`, each
/// with a description drawn from its docstring.
#[test]
#[serial]
fn the_envoy_shim_declares_exactly_the_three_read_only_tools_with_their_signatures() {
    let Some(_fx) = fixture("envoy-tools-declarations") else {
        return;
    };
    let functions = Functions::init_agent(ENVOY_AGENT_NAME, &[]).unwrap();
    let mut names: Vec<&str> = functions
        .declarations()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["fs_glob", "fs_grep", "fs_read"]);

    let expected: [(&str, &[&str], &[&str]); 3] = [
        ("fs_read", &["path"], &["offset", "limit"]),
        ("fs_grep", &["pattern"], &["path", "include"]),
        ("fs_glob", &["pattern"], &["path"]),
    ];
    for (name, required, optional) in expected {
        let decl = functions
            .declarations()
            .iter()
            .find(|d| d.name == name)
            .unwrap();
        assert!(
            !decl.description.trim().is_empty(),
            "{name} needs a description"
        );
        let props = decl.parameters.properties.as_ref().unwrap();
        let mut prop_names: Vec<&str> = props.keys().map(String::as_str).collect();
        prop_names.sort_unstable();
        let mut all: Vec<&str> = required.iter().chain(optional).copied().collect();
        all.sort_unstable();
        assert_eq!(prop_names, all, "{name} parameters");
        let mut got_required = decl.parameters.required.clone().unwrap_or_default();
        got_required.sort_unstable();
        assert_eq!(got_required, required.to_vec(), "{name} required");
        for prop in optional {
            assert!(
                !got_required.iter().any(|r| r == prop),
                "{name}: {prop} must be optional"
            );
        }
    }
}

/// The shim protocol accepts the tool arguments through `LLM_TOOL_DATA_FILE`
/// as well (the spill path for long argument strings), and that variable
/// must survive the hermetic child env.
#[test]
#[serial]
fn arguments_spilled_to_a_data_file_reach_the_tools_through_the_hermetic_env() {
    let Some(fx) = fixture("envoy-tools-spill") else {
        return;
    };
    let data_file = temp_file("-envoy-tool-data-", ".json");
    fs::write(&data_file, json!({"path": "a.txt", "limit": 1}).to_string()).unwrap();
    let output_file = temp_file("-envoy-tools-", ".json");
    let output = shim_command(
        &fx,
        &fx.shim,
        Some(&fx.root),
        "fs_read",
        &json!({"path": "sub/b.rs"}),
        &output_file,
    )
    .env("LLM_TOOL_DATA_FILE", &data_file)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_str(&fs::read_to_string(&output_file).unwrap()).unwrap();
    let _ = fs::remove_file(&output_file);
    let _ = fs::remove_file(&data_file);
    assert_eq!(value["path"], json!("a.txt"), "{value}");
    assert_eq!(value["content"], json!("1: alpha"), "{value}");

    let child = builtin_agent_child_env(&HashMap::from([(
        "LLM_TOOL_DATA_FILE".to_string(),
        "x".to_string(),
    )]));
    assert_eq!(
        child.get("LLM_TOOL_DATA_FILE").map(String::as_str),
        Some("x")
    );
}

/// Runs the shim with a wall-clock bound so a hang shows up as a failure
/// instead of stalling the suite; returns the parsed result and stderr.
/// `deny_dirs` replaces the production deny list; `None` keeps it.
fn call_within(
    fx: &Fixture,
    root: Option<&Path>,
    func: &str,
    args: Value,
    deny_dirs: Option<&str>,
    secs: u64,
) -> (Option<Value>, Output) {
    let output_file = temp_file("-envoy-tools-", ".json");
    let mut command = shim_command(fx, &fx.shim, root, func, &args, &output_file);
    if let Some(list) = deny_dirs {
        command.env("ENVOY_DENY_DIRS", list);
    }
    wait_within(command, &output_file, func, &args, secs)
}

/// Like `call_within` with the `ENVOY_DENY_DIRS` key absent altogether.
fn call_without_deny_dirs(fx: &Fixture, func: &str, args: Value) -> (Option<Value>, Output) {
    let output_file = temp_file("-envoy-tools-", ".json");
    let mut command = shim_command(fx, &fx.shim, Some(&fx.root), func, &args, &output_file);
    command.env_remove("ENVOY_DENY_DIRS");
    wait_within(command, &output_file, func, &args, 30)
}

fn wait_within(
    mut command: Command,
    output_file: &Path,
    func: &str,
    args: &Value,
    secs: u64,
) -> (Option<Value>, Output) {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = fs::remove_file(output_file);
            panic!("{func} {args}: the shim did not finish within {secs}s");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().unwrap();
    let value = fs::read_to_string(output_file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let _ = fs::remove_file(output_file);
    (value, output)
}

fn assert_clean_exit(func: &str, args: &Value, output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{func} {args}: exit {:?}: {stderr}",
        output.status.code()
    );
    assert!(!stderr.contains("Traceback"), "{func} {args}: {stderr}");
}

fn leaks(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(paths) = v["paths"].as_array() {
        for p in paths {
            let p = p.as_str().unwrap();
            if is_denied(p)
                || p.starts_with("..")
                || p.starts_with('/')
                || p.contains("leak.txt")
                || p.contains("s.txt")
                || p.contains("secret.txt")
            {
                out.push(p.to_string());
            }
        }
    }
    if let Some(matches) = v["matches"].as_array() {
        for m in matches {
            let p = m["path"].as_str().unwrap();
            let t = m["text"].as_str().unwrap();
            if is_denied(p)
                || p.starts_with("..")
                || p.starts_with('/')
                || t.contains("SECRET")
                || t.contains("[core]")
                || t.contains("secret")
                || t.contains("leaked")
            {
                out.push(format!("{p}: {t}"));
            }
        }
    }
    out
}

/// The confinement is a property of every resolved path, so a glob
/// `pattern` or grep `include` naming `..`, an absolute prefix, a denied
/// name or an out-of-root link must not surface anything the same path
/// would have been refused for.
#[test]
#[serial]
fn glob_patterns_and_grep_includes_cannot_escape_or_unmask_denied_entries() {
    let Some(fx) = fixture("envoy-tools-pattern-escape") else {
        return;
    };
    let glob_patterns = [
        "../*",
        "../root2/*",
        "sub/../../root2/*",
        "/etc/*",
        "/*",
        "**/.env*",
        ".env",
        ".env.*",
        ".git/*",
        "**/.git/**",
        "link_out_dir/*",
        "sib/*",
        "**/leak.txt",
        "*/../.env",
    ];
    for pattern in glob_patterns {
        let args = json!({"pattern": pattern});
        let (value, output) = call_within(&fx, Some(&fx.root), "fs_glob", args.clone(), None, 30);
        assert_clean_exit("fs_glob", &args, &output);
        let value = value.unwrap_or_else(|| panic!("fs_glob {args}: no JSON result"));
        if value.get("error").is_none() {
            assert!(leaks(&value).is_empty(), "fs_glob {args}: {value}");
        }
    }
    let includes = [
        ".env", ".env*", "*.env*", "../*", "*", "config", "**", "leak.txt", "s.txt",
    ];
    for include in includes {
        let args = json!({"pattern": "SECRET|core|secret|leaked", "include": include});
        let (value, output) = call_within(&fx, Some(&fx.root), "fs_grep", args.clone(), None, 30);
        assert_clean_exit("fs_grep", &args, &output);
        let value = value.unwrap_or_else(|| panic!("fs_grep {args}: no JSON result"));
        if value.get("error").is_none() {
            assert!(leaks(&value).is_empty(), "fs_grep {args}: {value}");
        }
    }
    for path in ["", ".", "./", "sub/.."] {
        let args = json!({"pattern": "*.rs", "path": path});
        let (value, output) = call_within(&fx, Some(&fx.root), "fs_glob", args.clone(), None, 30);
        assert_clean_exit("fs_glob", &args, &output);
        let value = value.unwrap();
        if value.get("error").is_none() {
            assert_eq!(
                paths_of(&value),
                vec!["sub/b.rs"],
                "fs_glob {args}: {value}"
            );
        } else {
            assert!(
                path == "sub/..",
                "fs_glob {args}: a root-equivalent path must work: {value}"
            );
        }
    }
}

/// `ENVOY_DENY_DIRS` is os.pathsep-joined and compared case-folded. A list
/// with empty, relative or missing entries must neither crash nor weaken
/// the root confinement, and an unset list still confines to the root; a
/// case variant of a denied dir is still refused.
#[test]
#[serial]
fn deny_dirs_tolerate_junk_entries_and_compare_case_folded() {
    let Some(fx) = fixture("envoy-tools-deny-junk") else {
        return;
    };
    write(&fx.root.join("probe-case"), "probe\n");
    let case_insensitive = fx.root.join("PROBE-CASE").exists();
    let vault = fx.root.join("vault").join("coyote");
    write(&vault.join("config.yaml"), "api_key: sk-secret\n");
    write(&fx.root.join("vault").join("readme.txt"), "public\n");
    let sep = ":";

    let junk = format!(
        "{sep}{sep}nonexistent-relative{sep}.{sep}{}{sep}{}/{sep}{}",
        fx.root.join("does-not-exist").display(),
        vault.display(),
        fx.root.join("big.txt").display(),
    );
    let file_entry = format!("{}", fx.root.join("a.txt").display());
    for (func, args) in [
        ("fs_read", json!({"path": "vault/coyote/config.yaml"})),
        ("fs_glob", json!({"pattern": "*", "path": "vault/coyote"})),
        (
            "fs_grep",
            json!({"pattern": "api_key", "path": "vault/coyote"}),
        ),
    ] {
        let (value, output) = call_within(&fx, Some(&fx.root), func, args.clone(), Some(&junk), 30);
        assert_clean_exit(func, &args, &output);
        let value = value.unwrap();
        refused(&value);
        assert!(
            value.get("content").is_none()
                && value.get("paths").is_none()
                && value.get("matches").is_none(),
            "{func} {args}: {value}"
        );
    }
    let (value, output) = call_within(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt"}),
        Some(&junk),
        30,
    );
    assert_clean_exit("fs_read", &json!({"path": "a.txt"}), &output);
    let value = value.unwrap();
    assert_eq!(
        value["content"],
        json!("1: alpha\n2: beta\n3: gamma"),
        "junk deny entries must not deny the root: {value}"
    );
    let (value, _) = call_within(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "api_key|public"}),
        Some(&junk),
        30,
    );
    assert_eq!(match_texts(&value.unwrap()), vec!["public"]);
    // A deny entry naming a file denies exactly that file.
    let (value, output) = call_within(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt"}),
        Some(&file_entry),
        30,
    );
    assert_clean_exit("fs_read", &json!({"path": "a.txt"}), &output);
    let value = value.unwrap();
    refused(&value);
    assert!(value.get("content").is_none(), "{value}");

    if case_insensitive {
        let upper = format!("{}", fx.root.join("vault").join("COYOTE").display());
        for (list, path) in [
            (upper.as_str(), "vault/coyote/config.yaml"),
            (junk.as_str(), "vault/COYOTE/config.yaml"),
            (junk.as_str(), "Vault/Coyote/config.yaml"),
        ] {
            let args = json!({"path": path});
            let (value, output) =
                call_within(&fx, Some(&fx.root), "fs_read", args.clone(), Some(list), 30);
            assert_clean_exit("fs_read", &args, &output);
            let value = value.unwrap();
            refused(&value);
            assert!(value.get("content").is_none(), "{list} / {path}: {value}");
        }
    }

    // Set but empty: nothing extra to deny, the root still confines.
    for (func, args) in [
        ("fs_read", json!({"path": "../root2/leak.txt"})),
        ("fs_read", json!({"path": "link_out_file"})),
        ("fs_read", json!({"path": ".env"})),
        ("fs_grep", json!({"pattern": "leaked", "path": "sib"})),
    ] {
        let (value, output) = call_within(&fx, Some(&fx.root), func, args.clone(), Some(""), 30);
        assert_clean_exit(func, &args, &output);
        refused(&value.unwrap());
    }
    let (value, _) = call_within(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt"}),
        Some(""),
        30,
    );
    assert_eq!(
        value.unwrap()["lines"],
        json!(3),
        "an empty deny list still serves the root"
    );
    // Absent altogether: the tool was not launched by Coyote, so it refuses.
    let (value, _) = call_without_deny_dirs(&fx, "fs_read", json!({"path": "a.txt"}));
    let value = value.unwrap();
    assert!(refused(&value).contains("ENVOY_DENY_DIRS"), "{value}");
}

/// Symlink loops, self-links and chains are resolved like any other link;
/// a loop is an error, never a hang or a traceback, and a chain that ends
/// outside the root is refused.
#[test]
#[serial]
fn symlink_loops_self_links_and_chains_are_resolved_without_hanging() {
    let Some(fx) = fixture("envoy-tools-loops") else {
        return;
    };
    symlink("loop", fx.root.join("loop")).unwrap();
    symlink("ping", fx.root.join("pong")).unwrap();
    symlink("pong", fx.root.join("ping")).unwrap();
    symlink(".", fx.root.join("self")).unwrap();
    symlink("..", fx.root.join("up")).unwrap();
    symlink("chain2", fx.root.join("chain1")).unwrap();
    symlink(fx.outside.join("secret.txt"), fx.root.join("chain2")).unwrap();
    symlink("inner2", fx.root.join("inner1")).unwrap();
    symlink("sub/b.rs", fx.root.join("inner2")).unwrap();
    symlink("dangling-target", fx.root.join("dangling")).unwrap();

    for (func, args) in [
        ("fs_read", json!({"path": "loop"})),
        ("fs_read", json!({"path": "ping"})),
        ("fs_read", json!({"path": "loop/x"})),
        ("fs_read", json!({"path": "chain1"})),
        ("fs_read", json!({"path": "up/root2/leak.txt"})),
        ("fs_read", json!({"path": "self/../root2/leak.txt"})),
        ("fs_read", json!({"path": "dangling"})),
        ("fs_glob", json!({"pattern": "*", "path": "loop"})),
        ("fs_glob", json!({"pattern": "*", "path": "up"})),
        ("fs_grep", json!({"pattern": "x", "path": "ping"})),
        ("fs_grep", json!({"pattern": "leaked", "path": "up"})),
    ] {
        let (value, output) = call_within(&fx, Some(&fx.root), func, args.clone(), None, 30);
        assert_clean_exit(func, &args, &output);
        let value = value.unwrap_or_else(|| panic!("{func} {args}: no JSON result"));
        let error = refused(&value).to_string();
        for secret in [&fx.root, &fx.parent, &fx.outside] {
            assert!(
                !error.contains(&secret.display().to_string()),
                "{func} {args}: refusal names '{}': {error}",
                secret.display()
            );
        }
    }

    let via_self = read(&fx, "self/a.txt");
    assert_eq!(
        via_self["content"],
        json!("1: alpha\n2: beta\n3: gamma"),
        "{via_self}"
    );
    let via_chain = read(&fx, "inner1");
    assert!(
        via_chain["content"].as_str().unwrap().contains("needle"),
        "{via_chain}"
    );

    let (listing, output) = call_within(
        &fx,
        Some(&fx.root),
        "fs_glob",
        json!({"pattern": "*.rs"}),
        None,
        30,
    );
    assert_clean_exit("fs_glob", &json!({"pattern": "*.rs"}), &output);
    let listing = listing.unwrap();
    let paths = paths_of(&listing);
    assert!(paths.contains(&"sub/b.rs"), "{listing}");
    assert!(
        paths
            .iter()
            .all(|p| !p.starts_with("self/") && !p.starts_with("up/") && !p.starts_with("loop/")),
        "the walk followed a self or parent link: {listing}"
    );
    assert_eq!(
        paths.iter().filter(|p| p.ends_with("b.rs")).count(),
        1,
        "sub/b.rs must be listed once under its own path (inner1/inner2 are file links, not .rs names): {listing}"
    );
    assert_eq!(listing["truncated"], json!(false), "{listing}");
    let (grep, output) = call_within(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "top secret|leaked|alpha"}),
        None,
        30,
    );
    assert_clean_exit("fs_grep", &json!({}), &output);
    let grep = grep.unwrap();
    let texts = match_texts(&grep);
    assert!(texts.iter().all(|t| t == "alpha"), "{grep}");
    assert!(!texts.is_empty(), "{grep}");
}

/// The root is `realpath`'d: a trailing separator or a symlinked root is
/// fine, a dangling link or a link to a file is "not a directory". File
/// names with spaces, unicode, tabs and newlines round-trip through the
/// JSON protocol on every tool.
#[test]
#[serial]
fn odd_root_forms_and_file_names_round_trip() {
    let Some(fx) = fixture("envoy-tools-odd-names") else {
        return;
    };
    let names = [
        "with space.txt",
        "h\u{e9}llo w\u{f6}rld.md",
        "tab\there.txt",
        "new\nline.txt",
        "caf\u{e9}/\u{4e2d}\u{6587}.rs",
    ];
    for name in names {
        write(&fx.root.join(name), "oddneedle\n");
    }
    let trailing = format!("{}/", fx.root.display());
    let a = call(
        &fx,
        Some(Path::new(&trailing)),
        "fs_read",
        json!({"path": "a.txt"}),
    );
    assert_eq!(a["path"], json!("a.txt"), "{a}");
    assert_eq!(a["lines"], json!(3), "{a}");

    let dangling_root = fx.parent.join("dangling-root");
    symlink(fx.parent.join("nowhere"), &dangling_root).unwrap();
    let file_root = fx.parent.join("file-root");
    symlink(fx.root.join("a.txt"), &file_root).unwrap();
    for root in [&dangling_root, &file_root] {
        let result = call(&fx, Some(root), "fs_glob", json!({"pattern": "*"}));
        assert!(
            refused(&result).contains("ENVOY_ROOT_DIR"),
            "root {}: {result}",
            root.display()
        );
    }

    for name in names {
        let r = read(&fx, name);
        assert_eq!(r["path"], json!(name), "{r}");
        assert_eq!(r["content"], json!("1: oddneedle"), "{r}");
    }
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    let paths = paths_of(&listing);
    for name in names {
        assert!(paths.contains(&name), "{name:?} missing from {listing}");
    }
    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "oddneedle"}),
    );
    let mut hit: Vec<&str> = grep["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    hit.sort_unstable();
    let mut want: Vec<&str> = names.to_vec();
    want.sort_unstable();
    assert_eq!(hit, want, "{grep}");
    let scoped = call(
        &fx,
        Some(&fx.root),
        "fs_glob",
        json!({"pattern": "*.rs", "path": "caf\u{e9}"}),
    );
    assert_eq!(
        paths_of(&scoped),
        vec!["caf\u{e9}/\u{4e2d}\u{6587}.rs"],
        "{scoped}"
    );
}

/// Edge arguments the contract does not name must still come back as a
/// result, never a traceback or a hang: empty patterns, negative or huge
/// offsets and limits, and a binary file met by the grep walk.
#[test]
#[serial]
fn edge_arguments_and_binary_files_never_break_the_protocol() {
    let Some(fx) = fixture("envoy-tools-edge-args") else {
        return;
    };
    write(&fx.root.join("blob.bin"), b"prefix\x00needle\nalpha\n");
    let cases = [
        ("fs_grep", json!({"pattern": ""})),
        ("fs_glob", json!({"pattern": ""})),
        ("fs_grep", json!({"pattern": "^"})),
        ("fs_grep", json!({"pattern": "needle|alpha"})),
        ("fs_grep", json!({"pattern": "needle", "path": "blob.bin"})),
        ("fs_read", json!({"path": "a.txt", "offset": -1})),
        ("fs_read", json!({"path": "a.txt", "limit": -5})),
        (
            "fs_read",
            json!({"path": "a.txt", "offset": 1, "limit": 1000000000}),
        ),
        ("fs_read", json!({"path": "a.txt", "offset": 1e20})),
        ("fs_read", json!({"path": ""})),
        ("fs_read", json!({"path": "   "})),
        ("fs_glob", json!({"pattern": "*", "path": "a.txt"})),
        ("fs_grep", json!({"pattern": "a", "include": ""})),
    ];
    for (func, args) in cases {
        let (value, output) = call_within(&fx, Some(&fx.root), func, args.clone(), None, 30);
        assert_clean_exit(func, &args, &output);
        let value = value.unwrap_or_else(|| panic!("{func} {args}: no JSON result"));
        if value.get("error").is_none() {
            assert!(leaks(&value).is_empty(), "{func} {args}: {value}");
            if let Some(matches) = value["matches"].as_array() {
                assert!(matches.len() <= MAX_GREP_MATCHES, "{func} {args}: {value}");
            }
            if let Some(paths) = value["paths"].as_array() {
                assert!(paths.len() <= MAX_GLOB_RESULTS, "{func} {args}: {value}");
            }
        }
    }
    let huge = call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt", "offset": 1, "limit": 1000000000}),
    );
    assert_eq!(huge["lines"], json!(3), "{huge}");
    assert_eq!(huge["truncated"], json!(false), "{huge}");
    for offset in [-1, 0] {
        refused(&call(
            &fx,
            Some(&fx.root),
            "fs_read",
            json!({"path": "a.txt", "offset": offset}),
        ));
    }
    refused(&call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "a.txt", "limit": -5}),
    ));
}

/// The in-tool budget only fires between lines, so it is driven to zero
/// here by importing the module directly (`-I` leaves the cwd off
/// `sys.path`, hence the insert) and the exhausted branch is checked for
/// shape.
#[test]
#[serial]
fn an_exhausted_search_budget_reports_a_truncated_result() {
    let Some(fx) = fixture("envoy-tools-budget-exhausted") else {
        return;
    };
    const SCRIPT: &str = "import sys, json; sys.path.insert(0, \".\"); import tools; \
                          tools.SEARCH_BUDGET_SECONDS = 0.0; \
                          print(json.dumps(tools.fs_grep(\"needle\", \"many\")))";
    let output = Command::new(&fx.python)
        .args(["-I", "-c", SCRIPT])
        .current_dir(&fx.dir)
        .env_clear()
        .envs(builtin_agent_child_env(&inherited_process_env()))
        .env("ENVOY_ROOT_DIR", &fx.root)
        .env("ENVOY_DENY_DIRS", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["budget_exhausted"], json!(true), "{value}");
    assert_eq!(value["truncated"], json!(true), "{value}");
    assert!(value["matches"].is_array(), "{value}");
}

/// A walk that meets more entries than `MAX_SCAN_ENTRIES` stops early and
/// says so through `scan_truncated`, returning what it saw so far rather
/// than nothing; the cap is driven down by importing the module directly.
/// The walk is scoped to `many` because at the root the first five entries
/// are all directories and the partial result would be empty.
#[test]
#[serial]
fn an_oversized_walk_reports_scan_truncated_with_a_bounded_partial_result() {
    let Some(fx) = fixture("envoy-tools-scan-truncated") else {
        return;
    };
    const SCAN_CAP: usize = 5;
    let script = format!(
        "import sys, json; sys.path.insert(0, \".\"); import tools; \
         tools.MAX_SCAN_ENTRIES = {SCAN_CAP}; \
         print(json.dumps([tools.fs_glob(\"*\", \"many\"), tools.fs_grep(\"needle\", \"many\")]))"
    );
    let output = Command::new(&fx.python)
        .args(["-I", "-c", &script])
        .current_dir(&fx.dir)
        .env_clear()
        .envs(builtin_agent_child_env(&inherited_process_env()))
        .env("ENVOY_ROOT_DIR", &fx.root)
        .env("ENVOY_DENY_DIRS", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let values: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let [glob, grep] = values.as_slice() else {
        panic!("{values:?}");
    };
    assert_eq!(glob["scan_truncated"], json!(true), "{glob}");
    let paths = paths_of(glob);
    assert!(!paths.is_empty() && paths.len() <= SCAN_CAP, "{glob}");
    assert_eq!(grep["scan_truncated"], json!(true), "{grep}");
    let matches = match_texts(grep);
    assert!(!matches.is_empty() && matches.len() <= SCAN_CAP, "{grep}");
}

/// A Windows junction is a directory that `islink` does not report but that
/// resolves elsewhere. Modelled here with plain directories whose `realpath`
/// is redirected inside the module: one onto the in-root `.git`, one onto a
/// directory outside the root. Neither may be walked by `fs_glob`/`fs_grep`.
#[test]
#[serial]
fn junction_like_dirs_are_pruned_by_their_resolved_target() {
    let Some(fx) = fixture("envoy-tools-junction") else {
        return;
    };
    write(&fx.root.join("j_git").join("config"), "[core] junction\n");
    write(
        &fx.root.join("j_out").join("s.txt"),
        "outside via junction\n",
    );
    write(&fx.root.join("plain").join("p.txt"), "plain needle\n");
    const SCRIPT: &str = r#"
import sys, json, os
sys.path.insert(0, ".")
import tools
real = os.path.realpath
root = real(os.environ["ENVOY_ROOT_DIR"])
outside = real(os.environ["JUNCTION_OUTSIDE"])
def junction(path):
    p = real(path)
    for name, target in (("j_git", os.path.join(root, ".git")), ("j_out", outside)):
        base = os.path.join(root, name)
        if p == base or p.startswith(base + os.sep):
            return target + p[len(base):]
    return p
tools.os.path.realpath = junction
tools.MAX_GLOB_RESULTS = 10000
print(json.dumps([tools.fs_glob("*"), tools.fs_grep("junction|plain needle")]))
"#;
    let output = Command::new(&fx.python)
        .args(["-I", "-c", SCRIPT])
        .current_dir(&fx.dir)
        .env_clear()
        .envs(builtin_agent_child_env(&inherited_process_env()))
        .env("ENVOY_ROOT_DIR", &fx.root)
        .env("JUNCTION_OUTSIDE", &fx.outside)
        .env("ENVOY_DENY_DIRS", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let values: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let [glob, grep] = values.as_slice() else {
        panic!("{values:?}");
    };
    let paths = paths_of(glob);
    assert!(paths.contains(&"plain/p.txt"), "{glob}");
    assert!(
        paths
            .iter()
            .all(|p| !p.starts_with("j_git/") && !p.starts_with("j_out/")),
        "{glob}"
    );
    let texts = match_texts(grep);
    assert_eq!(texts, vec!["plain needle"], "{grep}");
}

/// One entry per node under `root` (symlinks unfollowed): relative path,
/// kind, size and mtime. Two equal snapshots mean nothing was created,
/// removed or rewritten.
fn snapshot(root: &Path) -> Vec<(String, String, u64, Option<std::time::SystemTime>)> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut Vec<(String, String, u64, Option<std::time::SystemTime>)>,
    ) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let kind = if meta.file_type().is_symlink() {
                "link"
            } else if meta.is_dir() {
                "dir"
            } else {
                "file"
            };
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((rel, kind.to_string(), meta.len(), meta.modified().ok()));
            if kind == "dir" {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The tools are read-only and scoped to the root. Driving all three
/// across the whole fixture (reads, a full grep walk, a full glob walk,
/// refusals and cap hits) must leave the root byte-for-byte as it was,
/// drop no cache or state files into it, and answer a repeated call
/// identically: the tools carry no state between invocations.
#[test]
#[serial]
fn the_tools_leave_the_root_untouched_and_answer_identically_on_repeat() {
    let Some(fx) = fixture("envoy-tools-read-only") else {
        return;
    };
    let before = snapshot(&fx.root);
    assert!(
        !before.iter().any(|(rel, ..)| rel.contains("__pycache__")),
        "fixture root starts without a python cache"
    );

    let calls: Vec<(&str, Value)> = vec![
        ("fs_read", json!({"path": "a.txt"})),
        ("fs_read", json!({"path": "big.txt"})),
        ("fs_read", json!({"path": "wide.txt"})),
        ("fs_read", json!({"path": "bin.dat"})),
        ("fs_read", json!({"path": ".env"})),
        ("fs_read", json!({"path": "link_out_file"})),
        ("fs_read", json!({"path": "../root2/leak.txt"})),
        ("fs_grep", json!({"pattern": "needle|alpha|SECRET"})),
        ("fs_grep", json!({"pattern": "needle", "path": "many"})),
        ("fs_glob", json!({"pattern": "**/*"})),
        ("fs_glob", json!({"pattern": "*", "path": "sub"})),
    ];
    let first: Vec<Value> = calls
        .iter()
        .map(|(func, args)| call(&fx, Some(&fx.root), func, args.clone()))
        .collect();
    let second: Vec<Value> = calls
        .iter()
        .map(|(func, args)| call(&fx, Some(&fx.root), func, args.clone()))
        .collect();
    for ((func, args), (a, b)) in calls.iter().zip(first.iter().zip(second.iter())) {
        assert_eq!(a, b, "{func} {args}: a repeated call answered differently");
    }

    let after = snapshot(&fx.root);
    assert_eq!(
        before,
        after,
        "the read-only tools changed the root:\n{:#?}",
        after
            .iter()
            .filter(|entry| !before.contains(entry))
            .collect::<Vec<_>>()
    );
    // The outside dirs the fixture links to are equally off limits.
    assert!(fx.outside.join("secret.txt").is_file());
    assert_eq!(
        fs::read_to_string(fx.outside.join("secret.txt")).unwrap(),
        "top secret\n"
    );
}

/// The everyday pattern is grep, then read the hit: `fs_grep` reports
/// `{path, line, text}` and `fs_read(path, offset=line, limit=1)` must hand
/// back that same line under the same number, whatever the file's line
/// endings, whether or not it ends in a newline, and when the line is
/// wider than `MAX_LINE_CHARS` in multibyte characters (the cap is in
/// characters and the truncated line stays valid text). An empty file is
/// a zero-line success, not an error.
#[test]
#[serial]
fn grep_hits_read_back_at_their_reported_line_for_crlf_unterminated_and_wide_files() {
    let Some(fx) = fixture("envoy-tools-grep-then-read") else {
        return;
    };
    write(&fx.root.join("crlf.txt"), b"alpha\r\nbeta\r\ngamma\r\n");
    write(&fx.root.join("noeol.txt"), b"one\ntwo");
    write(&fx.root.join("empty.txt"), b"");
    let wide_line = "\u{e9}".repeat(MAX_LINE_CHARS + 500);
    write(&fx.root.join("wide_mb.txt"), format!("{wide_line}\ntail\n"));

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "beta$|two$|tail|\u{e9}", "include": "*.txt", "path": "."}),
    );
    assert!(grep.get("error").is_none(), "{grep}");
    let matches = grep["matches"].as_array().unwrap();
    let mut seen: Vec<(String, u64)> = matches
        .iter()
        .map(|m| {
            (
                m["path"].as_str().unwrap().to_string(),
                m["line"].as_u64().unwrap(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("a.txt".to_string(), 2),
            ("crlf.txt".to_string(), 2),
            ("noeol.txt".to_string(), 2),
            ("wide_mb.txt".to_string(), 1),
            ("wide_mb.txt".to_string(), 2),
        ],
        "{grep}"
    );
    for m in matches {
        let path = m["path"].as_str().unwrap();
        let line = m["line"].as_u64().unwrap();
        let text = m["text"].as_str().unwrap();
        assert!(
            !text.contains('\r'),
            "{path}:{line}: carriage return leaked: {text:?}"
        );
        let read = call(
            &fx,
            Some(&fx.root),
            "fs_read",
            json!({"path": path, "offset": line, "limit": 1}),
        );
        assert!(read.get("error").is_none(), "{read}");
        assert_eq!(read["lines"], json!(1), "{read}");
        assert_eq!(
            read["content"],
            json!(format!("{line}: {text}")),
            "{path}:{line}: grep and read disagree"
        );
    }

    let wide = call(
        &fx,
        Some(&fx.root),
        "fs_read",
        json!({"path": "wide_mb.txt", "offset": 1, "limit": 1}),
    );
    let content = wide["content"].as_str().unwrap();
    assert!(content.ends_with(TRUNCATED_MARK), "{content}");
    assert_eq!(
        content.chars().count(),
        "1: ".len() + MAX_LINE_CHARS + TRUNCATED_MARK.len(),
        "the width cap counts characters, not bytes"
    );
    assert!(
        content.trim_end_matches(TRUNCATED_MARK).ends_with('\u{e9}'),
        "the cut must land on a character boundary: {content}"
    );
    let wide_hit = matches
        .iter()
        .find(|m| m["path"] == json!("wide_mb.txt") && m["line"] == json!(1))
        .unwrap();
    assert!(
        wide_hit["text"].as_str().unwrap().chars().count() <= MAX_LINE_CHARS + TRUNCATED_MARK.len(),
        "{wide_hit}"
    );

    let crlf = read(&fx, "crlf.txt");
    assert_eq!(crlf["lines"], json!(3), "{crlf}");
    assert_eq!(
        crlf["content"],
        json!("1: alpha\n2: beta\n3: gamma"),
        "{crlf}"
    );
    let noeol = read(&fx, "noeol.txt");
    assert_eq!(noeol["lines"], json!(2), "{noeol}");
    assert_eq!(noeol["content"], json!("1: one\n2: two"), "{noeol}");
    let empty = read(&fx, "empty.txt");
    assert!(empty.get("error").is_none(), "{empty}");
    assert_eq!(empty["lines"], json!(0), "{empty}");
    assert_eq!(empty["content"], json!(""), "{empty}");
    assert_eq!(empty["truncated"], json!(false), "{empty}");
}

/// The exec probe decides where the envoy lives. Coyote spawns children from
/// many threads (hooks, tools, MCP servers), and a script written and run
/// back to back can be refused with `Text file busy` while another thread's
/// child still holds the write end across its fork. That is not a `noexec`
/// mount: a writable, executable temp dir must always be chosen as the
/// primary, never silently swapped for the cache dir or reported unusable.
#[test]
#[serial]
fn the_exec_probe_is_not_fooled_by_concurrent_process_spawns() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    if python3().is_none() {
        if env::var_os("CI").is_some() {
            panic!("python3 is required on CI");
        }
        eprintln!("skipping: python3 not available");
        return;
    }
    let _config = TestConfigDirGuard::new("envoy-exec-race-cfg");
    let cache_dir = temp_file("-envoy-exec-race-cache-", "");
    let _cache_guard = RemoveOnDrop(cache_dir.clone());
    let _cache = EnvVarGuard::set(get_env_name("cache_dir"), &cache_dir);
    let temp_dir = dunce::canonicalize(env::temp_dir()).unwrap();

    // A child holds its inherited descriptors from fork until exec; a long
    // PATH search in the child keeps that window open for milliseconds.
    let slow_path = (0..600)
        .map(|i| format!("/nonexistent-envoy-probe-{i}"))
        .chain(std::iter::once("/bin".to_string()))
        .collect::<Vec<_>>()
        .join(":");
    let stop = Arc::new(AtomicBool::new(false));
    let storm: Vec<_> = (0..8)
        .map(|_| {
            let stop = Arc::clone(&stop);
            let slow_path = slow_path.clone();
            thread::spawn(move || {
                use std::process::Stdio;
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true")
                        .env_clear()
                        .env("PATH", &slow_path)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            })
        })
        .collect();

    let mut spurious = Vec::new();
    for attempt in 0..40 {
        let source = EnvoySource::new();
        match source.agent_dir(ENVOY_AGENT_NAME) {
            Some(dir) => {
                let canonical = dunce::canonicalize(&dir).unwrap_or(dir.clone());
                if !canonical.starts_with(&temp_dir) {
                    spurious.push(format!(
                        "attempt {attempt}: fell back to the cache dir {}",
                        dir.display()
                    ));
                }
                assert!(
                    dir.join("bin").is_dir() && dir.join("tools.py").is_file(),
                    "attempt {attempt}: {}",
                    dir.display()
                );
            }
            None => spurious.push(format!(
                "attempt {attempt}: unavailable: {:?}",
                source.unavailable_reason(ENVOY_AGENT_NAME)
            )),
        }
        source.remove_dir();
    }
    stop.store(true, Ordering::Relaxed);
    for handle in storm {
        handle.join().unwrap();
    }
    assert!(
        spurious.is_empty(),
        "a writable, executable temp dir was misjudged {} time(s) out of 40:\n{}",
        spurious.len(),
        spurious.join("\n")
    );
}

/// Every name the deny list documents is refused by `fs_read` and hidden
/// from `fs_glob` and `fs_grep`, in any case, at any depth; names that
/// merely resemble one (`.gitignore`, `Dockerfile`, `keyring.py`) stay
/// readable, so the list does not over-match.
#[test]
#[serial]
fn the_documented_deny_list_is_enforced_exactly_by_all_three_tools() {
    let Some(fx) = fixture("envoy-tools-deny-list") else {
        return;
    };
    let denied_files = [
        ".envrc",
        ".mcp.json",
        ".coyote_password",
        ".git-credentials",
        ".pgpass",
        ".bash_history",
        ".zsh_history",
        ".netrc",
        ".npmrc",
        ".pypirc",
        "id_rsa",
        "id_rsa.pub",
        "id_ed25519_work",
        "id_ecdsa",
        "id_dsa.bak",
        "server.pem",
        "server.key",
        "bundle.p12",
        "bundle.pfx",
        "terraform.tfstate",
        "terraform.tfstate.backup",
        "prod.tfvars",
        "prod.env",
        "sub/deep/.netrc",
        "sub/deep/client.KEY",
        "sub/deep/ID_RSA",
    ];
    let denied_dirs = [
        ".kube",
        ".password-store",
        ".terraform.d",
        ".ssh",
        ".aws",
        ".gnupg",
        ".docker",
        ".coyote",
        "sub/.Kube",
    ];
    let allowed = [
        ".gitignore",
        ".gitattributes",
        ".github/workflows/ci.yml",
        ".editorconfig",
        "Dockerfile",
        "docker-compose.yml",
        "keyring.py",
        "environment.txt",
        "aws.rs",
        "terraform/main.tf",
        "envelope.json",
        "pgpass.md",
        "history.txt",
        "identity.rs",
        "public.pem.txt",
    ];
    for name in denied_files {
        write(&fx.root.join(name), "MARKER-SECRET\n");
    }
    for dir in denied_dirs {
        write(&fx.root.join(dir).join("inner.txt"), "MARKER-SECRET\n");
    }
    for name in allowed {
        write(&fx.root.join(name), "MARKER-PUBLIC\n");
    }
    // Keep the full listing under the glob cap so every name is observable.
    fs::remove_dir_all(fx.root.join("many")).unwrap();

    for name in denied_files {
        let result = read(&fx, name);
        assert!(refused(&result).starts_with("refused:"), "{name}: {result}");
        assert!(result.get("content").is_none(), "{name}: {result}");
        let upper = name.to_uppercase();
        let result = read(&fx, &upper);
        assert!(
            refused(&result).starts_with("refused:"),
            "{upper}: {result}"
        );
    }
    for dir in denied_dirs {
        let inner = format!("{dir}/inner.txt");
        let result = read(&fx, &inner);
        assert!(
            refused(&result).starts_with("refused:"),
            "{inner}: {result}"
        );
        for (func, args) in [
            ("fs_glob", json!({"pattern": "*", "path": dir})),
            ("fs_grep", json!({"pattern": "MARKER", "path": dir})),
        ] {
            let result = call(&fx, Some(&fx.root), func, args.clone());
            refused(&result);
            assert!(
                result.get("paths").is_none() && result.get("matches").is_none(),
                "{func} {args}: {result}"
            );
        }
    }
    for name in allowed {
        let result = read(&fx, name);
        assert!(result.get("error").is_none(), "{name}: {result}");
        assert_eq!(
            result["content"],
            json!("1: MARKER-PUBLIC"),
            "{name}: {result}"
        );
    }

    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "MARKER-SECRET"}),
    );
    assert!(grep.get("error").is_none(), "{grep}");
    assert!(match_texts(&grep).is_empty(), "{grep}");
    let public = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "MARKER-PUBLIC"}),
    );
    let public_paths: Vec<&str> = public["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    for name in allowed {
        assert!(public_paths.contains(&name), "{name} missing from {public}");
    }

    // `*` crosses `/` in this tool's documented grammar, so it lists every depth.
    let listing = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*"}));
    assert!(listing.get("error").is_none(), "{listing}");
    let paths = paths_of(&listing);
    for name in denied_files {
        assert!(!paths.contains(&name), "{name} listed: {listing}");
    }
    for dir in denied_dirs {
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with(&format!("{dir}/")) || *p == dir),
            "{dir} listed: {listing}"
        );
    }
    for name in allowed {
        assert!(paths.contains(&name), "{name} not listed: {listing}");
    }
}

/// The launcher runs the interpreter with `-I -B`: the env cannot redirect
/// the module search path and no bytecode cache lands in the envoy dir, so
/// the dir stays exactly what was materialized plus the shim it built.
#[test]
#[serial]
fn the_launcher_isolates_the_interpreter_and_writes_no_bytecode_cache() {
    let Some(fx) = fixture("envoy-tools-launcher") else {
        return;
    };
    let poison = temp_file("-envoy-poison-", "");
    let _poison_guard = RemoveOnDrop(poison.clone());
    for module in ["json", "re", "os", "pathlib", "fnmatch"] {
        write(
            &poison.join(format!("{module}.py")),
            "raise SystemExit('poisoned module import')\n",
        );
    }
    write(
        &poison.join("startup.py"),
        "raise SystemExit('startup ran')\n",
    );
    write(
        &poison.join("usercustomize.py"),
        "raise SystemExit('usercustomize ran')\n",
    );

    let output_file = temp_file("-envoy-tools-", ".json");
    let args = json!({"pattern": "needle", "path": "sub"});
    let output = shim_command(
        &fx,
        &fx.shim,
        Some(&fx.root),
        "fs_grep",
        &args,
        &output_file,
    )
    .env("PYTHONPATH", &poison)
    .env("PYTHONSTARTUP", poison.join("startup.py"))
    .env("PYTHONUSERBASE", &poison)
    .env("PYTHONHOME", &poison)
    .env("PYTHONDONTWRITEBYTECODE", "")
    .output()
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        !stderr.contains("poisoned") && !stderr.contains(" ran"),
        "{stderr}"
    );
    let result: Value = serde_json::from_str(&fs::read_to_string(&output_file).unwrap()).unwrap();
    fs::remove_file(&output_file).unwrap();
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(
        match_texts(&result),
        vec!["// needle".to_string()],
        "{result}"
    );

    for _ in 0..2 {
        read(&fx, "a.txt");
        call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "*.txt"}));
    }
    let mut cache_dirs = Vec::new();
    let mut stack = vec![fx.dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "__pycache__" || name.ends_with(".pyc") {
                cache_dirs.push(path.display().to_string());
            } else if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            }
        }
    }
    assert!(
        cache_dirs.is_empty(),
        "bytecode cache written: {cache_dirs:?}"
    );
    assert!(
        !fx.root.join("__pycache__").exists() && !fx.root.join("sub").join("__pycache__").exists(),
        "the root must not receive a bytecode cache either"
    );
}

/// The optional parameters as a model actually combines them: a scoping
/// `path` with a pattern, explicit JSON nulls for omitted optionals, a file
/// as the grep target, a directory or a missing path as the read target,
/// and an offset past the end. Each answer follows the docstrings: glob
/// patterns match the root-relative path even when scoped, includes match
/// basenames, nulls mean defaults, and refusals never name an absolute
/// path or fall through to an internal error.
#[test]
#[serial]
fn scoped_paths_nulls_and_out_of_range_offsets_follow_the_documented_rules() {
    let Some(fx) = fixture("envoy-tools-scoped") else {
        return;
    };
    let root = Some(fx.root.as_path());

    let scoped_all = call(&fx, root, "fs_glob", json!({"pattern": "*", "path": "sub"}));
    assert_eq!(paths_of(&scoped_all), vec!["sub/b.rs"], "{scoped_all}");
    let scoped_full = call(
        &fx,
        root,
        "fs_glob",
        json!({"pattern": "sub/*.rs", "path": "sub"}),
    );
    assert_eq!(paths_of(&scoped_full), vec!["sub/b.rs"], "{scoped_full}");
    let scoped_base = call(
        &fx,
        root,
        "fs_glob",
        json!({"pattern": "b.rs", "path": "sub"}),
    );
    assert!(
        paths_of(&scoped_base).is_empty(),
        "the docstring promises root-relative matching, so a bare basename under a scope matches nothing: {scoped_base}"
    );

    let by_basename = call(
        &fx,
        root,
        "fs_grep",
        json!({"pattern": "needle", "include": "b.rs"}),
    );
    assert_eq!(
        by_basename["matches"].as_array().unwrap().len(),
        1,
        "{by_basename}"
    );
    let by_dir_glob = call(
        &fx,
        root,
        "fs_grep",
        json!({"pattern": "needle", "include": "sub/*.rs"}),
    );
    assert!(
        by_dir_glob["matches"].as_array().unwrap().is_empty(),
        "includes match basenames only: {by_dir_glob}"
    );

    let single_file = call(
        &fx,
        root,
        "fs_grep",
        json!({"pattern": "alpha", "path": "a.txt"}),
    );
    let hits = single_file["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{single_file}");
    assert_eq!(hits[0]["path"], json!("a.txt"));
    assert_eq!(hits[0]["line"], json!(1));
    let through_link = call(
        &fx,
        root,
        "fs_grep",
        json!({"pattern": "alpha", "path": "link_in"}),
    );
    let hits = through_link["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{through_link}");
    assert_eq!(hits[0]["line"], json!(1));

    let defaulted = read(&fx, "a.txt");
    let nulls = call(
        &fx,
        root,
        "fs_read",
        json!({"path": "a.txt", "offset": null, "limit": null}),
    );
    assert_eq!(nulls, defaulted, "explicit nulls mean the defaults");
    let grep_defaulted = call(&fx, root, "fs_grep", json!({"pattern": "needle"}));
    let grep_nulls = call(
        &fx,
        root,
        "fs_grep",
        json!({"pattern": "needle", "path": null, "include": null}),
    );
    assert_eq!(
        grep_nulls, grep_defaulted,
        "explicit nulls mean the defaults"
    );
    let glob_defaulted = call(&fx, root, "fs_glob", json!({"pattern": "*.rs"}));
    let glob_nulls = call(
        &fx,
        root,
        "fs_glob",
        json!({"pattern": "*.rs", "path": null}),
    );
    assert_eq!(
        glob_nulls, glob_defaulted,
        "explicit nulls mean the defaults"
    );

    let tail = call(
        &fx,
        root,
        "fs_read",
        json!({"path": "a.txt", "offset": 3, "limit": 5}),
    );
    assert_eq!(tail["content"], json!("3: gamma"), "{tail}");
    assert_eq!(
        tail["truncated"],
        json!(false),
        "a limit past the end is not a truncation: {tail}"
    );
    let past_end = call(&fx, root, "fs_read", json!({"path": "a.txt", "offset": 10}));
    assert!(
        !past_end["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("internal error")),
        "{past_end}"
    );
    if past_end.get("error").is_none() {
        assert_eq!(past_end["content"], json!(""), "{past_end}");
        assert_eq!(past_end["truncated"], json!(false), "{past_end}");
        assert_eq!(past_end["lines"], json!(0), "{past_end}");
        assert_eq!(past_end["total_lines_read"], json!(3), "{past_end}");
    }

    // The fs_glob docstring says `path` is a directory, but a file there is
    // accepted and listed on its own, the same way fs_grep takes a file.
    let file_scoped = call(
        &fx,
        root,
        "fs_glob",
        json!({"pattern": "*", "path": "sub/b.rs"}),
    );
    assert_eq!(paths_of(&file_scoped), vec!["sub/b.rs"], "{file_scoped}");

    let absolute_forms = [
        fx.root.display().to_string(),
        fx.parent.display().to_string(),
        fx.dir.display().to_string(),
    ];
    for (func, args) in [
        ("fs_read", json!({"path": "sub"})),
        ("fs_read", json!({"path": "nope/missing.txt"})),
        ("fs_read", json!({"path": "missing.txt"})),
        ("fs_grep", json!({"pattern": "alpha", "path": "nope"})),
        (
            "fs_grep",
            json!({"pattern": "alpha", "path": "nope/missing.txt"}),
        ),
        ("fs_glob", json!({"pattern": "*", "path": "nope"})),
    ] {
        let result = call(&fx, root, func, args.clone());
        let error = refused(&result);
        assert!(
            !error.starts_with("internal error"),
            "{func} {args}: a wrong path kind is a refusal, not an internal error: {result}"
        );
        for absolute in &absolute_forms {
            assert!(
                !error.contains(absolute.as_str()),
                "{func} {args}: refusal names an absolute path: {result}"
            );
        }
        assert!(
            result.get("content").is_none()
                && result.get("paths").is_none()
                && result.get("matches").is_none(),
            "{func} {args}: {result}"
        );
    }
}

/// `.git` is denied as a NAME, whatever kind of entry carries it: a
/// worktree or submodule checkout has a `.git` FILE holding
/// `gitdir: /absolute/path/to/main/.git/worktrees/...`, which would leak a
/// host path if read, grepped or listed. Sibling dot-git files that are not
/// the repository itself (`.gitmodules`) stay public.
#[test]
#[serial]
fn a_worktree_style_dot_git_file_is_denied_like_the_directory() {
    let Some(fx) = fixture("envoy-tools-git-file") else {
        return;
    };
    let pointer = format!("gitdir: {}/main/.git/worktrees/wt\n", fx.outside.display());
    write(&fx.root.join("wt").join(".git"), &pointer);
    write(&fx.root.join("wt").join("lib.rs"), "pub fn wt() {}\n");
    write(
        &fx.root.join(".gitmodules"),
        "[submodule \"dep\"]\n\tpath = dep\n\turl = https://example.invalid/dep.git\n",
    );
    write(
        &fx.root.join("dep").join(".git"),
        "gitdir: ../.git/modules/dep\n",
    );
    write(&fx.root.join("dep").join("dep.rs"), "pub fn dep() {}\n");
    // Keep the full listing under the glob cap so every name is observable.
    fs::remove_dir_all(fx.root.join("many")).unwrap();

    for path in ["wt/.git", "dep/.git", "./wt/.git", "wt/../wt/.git"] {
        let result = read(&fx, path);
        let error = refused(&result);
        // Every `..` form is refused up front as an escape; the rest reach
        // the deny check and are refused by name.
        assert!(
            error.starts_with("refused:") || error.contains("escapes the root"),
            "{path}: {result}"
        );
        assert!(
            !error.contains(&fx.outside.display().to_string()),
            "{path}: refusal names the pointer target: {result}"
        );
    }
    let modules = read(&fx, ".gitmodules");
    assert!(
        modules["content"]
            .as_str()
            .is_some_and(|c| c.contains("submodule")),
        ".gitmodules is public: {modules}"
    );

    for args in [
        json!({"pattern": "gitdir"}),
        json!({"pattern": "gitdir", "path": "wt"}),
        json!({"pattern": "gitdir", "path": "wt/.git"}),
        json!({"pattern": "worktrees|modules", "include": ".git"}),
    ] {
        let grep = call(&fx, Some(&fx.root), "fs_grep", args.clone());
        if let Some(error) = grep.get("error").and_then(Value::as_str) {
            assert!(error.starts_with("refused:"), "{args}: {grep}");
        } else {
            assert!(match_texts(&grep).is_empty(), "{args}: {grep}");
        }
        assert!(
            !grep.to_string().contains("worktrees"),
            "{args}: the pointer leaked: {grep}"
        );
    }

    for args in [
        json!({"pattern": "wt/*"}),
        json!({"pattern": "wt/.*"}),
        json!({"pattern": "*", "path": "wt"}),
        json!({"pattern": "*/.git"}),
        json!({"pattern": "**"}),
    ] {
        let listing = call(&fx, Some(&fx.root), "fs_glob", args.clone());
        let paths = paths_of(&listing);
        assert!(
            !paths.iter().any(|p| p.ends_with(".git")),
            "{args}: {listing}"
        );
    }
    let all = call(&fx, Some(&fx.root), "fs_glob", json!({"pattern": "**"}));
    let paths = paths_of(&all);
    for public in ["wt/lib.rs", "dep/dep.rs", ".gitmodules"] {
        assert!(paths.contains(&public), "{public} missing: {all}");
    }
}

/// `fs_grep(include=...)` is fnmatch, so a shell brace alternative like
/// `*.{rs,py}` is a literal and matches nothing. Whatever the tool decides,
/// it must not answer with a success-shaped empty result the model would
/// read as "no hits": either it refuses the pattern or the docstring the
/// model sees warns about braces.
#[test]
#[serial]
fn a_brace_include_pattern_is_refused_or_documented_not_silently_empty() {
    let Some(fx) = fixture("envoy-tools-brace-include") else {
        return;
    };
    let grep = call(
        &fx,
        Some(&fx.root),
        "fs_grep",
        json!({"pattern": "needle|alpha", "include": "*.{rs,txt}"}),
    );
    if grep.get("error").is_none() && match_texts(&grep).is_empty() {
        let tools_py = fs::read_to_string(fx.dir.join("tools.py")).unwrap();
        assert!(
            tools_py.contains("brace")
                || tools_py.contains("{a,b}")
                || tools_py.contains("fnmatch"),
            "brace include matched nothing and nothing warns the model: {grep}"
        );
    }
}
