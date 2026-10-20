//! Cold-start, real-binary contract for the three built-in mixin kits: a run
//! that engages an agent (`--info -a probe`) installs the bundled assets into
//! a fresh config dir, which lands the kits at
//! `functions/kits/built-in-tools/`, `agents/sisyphus/kits/sisyphus-ddg/` and
//! `agents/probe/kits/agent-probe/` byte-for-byte, removes the v2
//! `sbx-mixin.yaml` predecessors that carry a shipped name, and leaves every
//! other `sbx-mixin.yaml` alone. A second run changes nothing.
//!
//! Every run uses a fresh fake HOME, config dir and cache dir so host state
//! cannot leak in; the dry-run model means no provider is ever contacted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

const KIT_FILES: [&str; 3] = [
    "functions/kits/built-in-tools/built-in-tools.yaml",
    "agents/sisyphus/kits/sisyphus-ddg/sisyphus-ddg.yaml",
    "agents/probe/kits/agent-probe/agent-probe.yaml",
];

const V2_FILE: &str = "sbx-mixin.yaml";

/// A v2 mixin in the shape the shipped files used (single-quoted version).
fn v2_mixin(name: &str) -> String {
    format!(
        "schemaVersion: '2'\nkind: mixin\nname: {name}\npermissions:\n  network:\n    allow:\n      - example.invalid\n"
    )
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_root(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-sbx-mixin-{label}-{}-{unique}",
        process::id()
    ));
    for sub in ["home", "config", "cache"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir
}

fn repo_asset(relative: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(relative);
    fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn write_dry_run_config(root: &Path) {
    let vault_pass = root.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        root.join("config/config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             memory: false\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn run_coyote(root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(root)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_SANDBOX_KIT")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn config_path(root: &Path, relative: &str) -> PathBuf {
    root.join("config").join(relative)
}

fn seed(root: &Path, relative: &str, text: &str) -> PathBuf {
    let path = config_path(root, relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

fn assert_engaged(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label}: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every regular file under any `kits/` directory of the config dir plus
/// every `sbx-mixin.yaml`, relative to the config dir, with its mtime.
fn kit_and_v2_listing(root: &Path) -> BTreeMap<String, SystemTime> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, SystemTime>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
                continue;
            }
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if relative.contains("/kits/") || relative.ends_with(V2_FILE) {
                out.insert(relative, fs::metadata(&path).unwrap().modified().unwrap());
            }
        }
    }
    let base = root.join("config");
    let mut out = BTreeMap::new();
    walk(&base, &base, &mut out);
    out
}

#[test]
fn engaging_an_agent_installs_the_kits_and_reconciles_only_shipped_v2_mixins() {
    let root = fresh_root("install");
    let _guard = TempDirGuard(root.clone());
    write_dry_run_config(&root);

    // Shipped names at the two paths the installer reconciles: removed.
    let shipped_probe = seed(
        &root,
        "agents/probe/sbx-mixin.yaml",
        &v2_mixin("agent-probe"),
    );
    let shipped_tools = seed(
        &root,
        "functions/sbx-mixin.yaml",
        &v2_mixin("built-in-tools").replace("'2'", "\"2\""),
    );
    // Everything else at or near those paths: left byte-for-byte.
    let user_named = seed(&root, "agents/coder/sbx-mixin.yaml", &v2_mixin("mine"));
    let non_bundled_agent = seed(
        &root,
        "agents/my-own-agent/sbx-mixin.yaml",
        &v2_mixin("agent-probe"),
    );
    let suffixed = seed(
        &root,
        "agents/coder/extra.sbx-mixin.yaml",
        &v2_mixin("agent-probe"),
    );
    let workload = seed(
        &root,
        "agents/sisyphus/sbx-mixin.yaml",
        &v2_mixin("sisyphus-ddg").replace("kind: mixin", "kind: workload"),
    );
    let v3_named_like_v2 = seed(
        &root,
        "agents/architect/sbx-mixin.yaml",
        &v2_mixin("agent-probe").replace("'2'", "\"3\""),
    );
    let directory = config_path(&root, "agents/oracle/sbx-mixin.yaml");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("inner"), v2_mixin("agent-probe")).unwrap();

    let survivors = [
        &user_named,
        &non_bundled_agent,
        &suffixed,
        &workload,
        &v3_named_like_v2,
    ];
    let survivor_text: Vec<String> = survivors
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();

    assert_engaged("first run", &run_coyote(&root, &["--info", "-a", "probe"]));

    for relative in KIT_FILES {
        let installed = config_path(&root, relative);
        assert_eq!(
            fs::read(&installed).unwrap_or_else(|e| panic!("{relative}: {e}")),
            repo_asset(relative),
            "{relative}: installed kit differs from the shipped asset"
        );
        let dir = installed.parent().unwrap();
        let names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [installed
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()],
            "{relative}: the kit dir holds exactly the handle yaml"
        );
    }
    assert!(!shipped_probe.exists(), "shipped probe v2 mixin survived");
    assert!(!shipped_tools.exists(), "shipped tools v2 mixin survived");
    for (path, text) in survivors.iter().zip(&survivor_text) {
        assert_eq!(
            &fs::read_to_string(path).unwrap(),
            text,
            "{} must survive untouched",
            path.display()
        );
    }
    assert!(directory.is_dir() && directory.join("inner").is_file());

    // The second run is a no-op on this surface: same files, same mtimes.
    let before = kit_and_v2_listing(&root);
    assert_engaged("second run", &run_coyote(&root, &["--info", "-a", "probe"]));
    assert_eq!(kit_and_v2_listing(&root), before);
    assert!(directory.is_dir() && directory.join("inner").is_file());
}

/// Each installer reconciles with its own shipped-name list, so a shipped name
/// from the OTHER installer's list is user content at that path and stays,
/// as does a user name at a path the old release did ship. The installers
/// also treat the kit files like every other bundled asset: a deleted kit
/// comes back on the next run, a user-edited one is left as edited.
#[test]
fn installers_keep_their_name_lists_apart_and_restore_only_missing_kits() {
    let root = fresh_root("lists");
    let _guard = TempDirGuard(root.clone());
    write_dry_run_config(&root);

    let cross_list_tools = seed(&root, "functions/sbx-mixin.yaml", &v2_mixin("agent-probe"));
    let cross_list_agent = seed(
        &root,
        "agents/probe/sbx-mixin.yaml",
        &v2_mixin("built-in-tools"),
    );
    let user_at_shipped_path = seed(&root, "agents/sisyphus/sbx-mixin.yaml", &v2_mixin("mine"));
    let survivors = [&cross_list_tools, &cross_list_agent, &user_at_shipped_path];
    let survivor_text: Vec<String> = survivors
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();

    assert_engaged("first run", &run_coyote(&root, &["--info", "-a", "probe"]));

    for relative in KIT_FILES {
        assert_eq!(
            fs::read(config_path(&root, relative)).unwrap(),
            repo_asset(relative),
            "{relative}: first run installs the shipped kit"
        );
    }
    for (path, text) in survivors.iter().zip(&survivor_text) {
        assert_eq!(
            &fs::read_to_string(path).unwrap(),
            text,
            "{} must survive the first run untouched",
            path.display()
        );
    }

    // Remove one kit at the directory level and one at the file level; edit
    // the third in place.
    let probe_kits = config_path(&root, "agents/probe/kits");
    fs::remove_dir_all(&probe_kits).unwrap();
    let tools_kit = config_path(&root, KIT_FILES[0]);
    fs::remove_file(&tools_kit).unwrap();
    let ddg_kit = config_path(&root, KIT_FILES[1]);
    let edited = format!(
        "{}# edited by the operator\n",
        fs::read_to_string(&ddg_kit).unwrap()
    );
    fs::write(&ddg_kit, &edited).unwrap();

    assert_engaged("second run", &run_coyote(&root, &["--info", "-a", "probe"]));

    assert_eq!(
        fs::read(config_path(&root, KIT_FILES[2])).unwrap(),
        repo_asset(KIT_FILES[2]),
        "a deleted kit directory is recreated from the shipped asset"
    );
    let names: Vec<String> = fs::read_dir(&probe_kits)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["agent-probe"],
        "the recreated kits dir holds only the kit"
    );
    assert_eq!(
        fs::read(&tools_kit).unwrap(),
        repo_asset(KIT_FILES[0]),
        "a deleted kit file is recreated from the shipped asset"
    );
    assert_eq!(
        fs::read_to_string(&ddg_kit).unwrap(),
        edited,
        "a user-edited kit is left as edited on a normal start"
    );
    for (path, text) in survivors.iter().zip(&survivor_text) {
        assert_eq!(
            &fs::read_to_string(path).unwrap(),
            text,
            "{} must survive the second run untouched",
            path.display()
        );
    }
}

/// The common first-run shape has no `agents/` or `functions/` directory at
/// all, and the v2 reconcile is part of every start rather than of the first
/// kit write: a shipped-named `sbx-mixin.yaml` that reappears at any of the
/// three reconciled paths after the kits already exist is removed by the
/// next run, which leaves the kits themselves untouched.
#[test]
fn usage_probe_a_bare_config_dir_gets_the_kits_and_every_start_reconciles_shipped_v2_files() {
    let root = fresh_root("bare");
    let _guard = TempDirGuard(root.clone());
    write_dry_run_config(&root);
    let config = root.join("config");
    assert!(
        !config.join("agents").exists() && !config.join("functions").exists(),
        "the fixture must start without the asset directories"
    );

    assert_engaged("cold start", &run_coyote(&root, &["--info", "-a", "probe"]));

    let after_cold = kit_and_v2_listing(&root);
    let kit_rows: BTreeSet<&str> = after_cold
        .keys()
        .filter(|relative| relative.contains("/kits/"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        kit_rows,
        KIT_FILES.into_iter().collect::<BTreeSet<_>>(),
        "a bare config dir ends the cold start with exactly the three kits"
    );
    assert!(
        after_cold
            .keys()
            .all(|relative| !relative.ends_with(V2_FILE)),
        "a cold start writes no v2 mixin anywhere: {after_cold:?}"
    );
    for relative in KIT_FILES {
        assert_eq!(
            fs::read(config_path(&root, relative)).unwrap(),
            repo_asset(relative),
            "{relative}: installed kit differs from the shipped asset"
        );
    }

    // The kits exist now; an older release's files reappearing at the three
    // reconciled paths (one per shipped name, one per installer list) must
    // still go on the next start.
    let reappeared = [
        seed(
            &root,
            "functions/sbx-mixin.yaml",
            &v2_mixin("built-in-tools"),
        ),
        seed(
            &root,
            "agents/sisyphus/sbx-mixin.yaml",
            &v2_mixin("sisyphus-ddg"),
        ),
        seed(
            &root,
            "agents/probe/sbx-mixin.yaml",
            &v2_mixin("agent-probe").replace("'2'", "\"2\""),
        ),
    ];

    assert_engaged(
        "second start",
        &run_coyote(&root, &["--info", "-a", "probe"]),
    );

    for path in &reappeared {
        assert!(
            !path.exists(),
            "{} must be reconciled on a start that writes no kit",
            path.display()
        );
    }
    assert_eq!(
        kit_and_v2_listing(&root),
        after_cold,
        "reconciling the v2 files leaves the kits' bytes and mtimes alone"
    );
}
