//! Real-process checks of the command wizards and modals under the
//! full-screen UI: the questions `.install`, `.uninstall`, `.update`,
//! `.vault` and `.rag` raise become modals on a real tty and inquire
//! prompts in the plain REPL, document loaders that stall or die are
//! reined in, and however a case leaves the tty it ran on must come back
//! in its cooked, echoing state (the prompter-routed menus answered from
//! a clean config dir, and the runs with no human to ask, live in
//! `tui_binary_menus.rs`). Each case runs the binary on a pseudo-terminal
//! under a bash wrapper that reports the exit code and `stty -a`
//! afterwards, so the file is unix-only. The cases run one at a time:
//! each spawn forks a session leader and takes a controlling tty, and a
//! fork landing between another test's `openpty` and its `exec` has
//! failed the spawn with a garbled errno.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

// ---------------------------------------------------------------------------
// Bundle installs that reference secrets, and document loaders that are
// slow or die under the full-screen UI.
// ---------------------------------------------------------------------------

/// Writes `extra` top-level YAML after the fake-LLM config, so a case can
/// add `document_loaders` while keeping the fake model (whose served count
/// proves whether a document ever reached the model).
fn write_fake_llm_config_with(dir: &Path, fake: &FakeLlm, extra: &str) {
    write_fake_llm_config(dir, fake);
    let path = dir.join("config.yaml");
    let mut config = fs::read_to_string(&path).unwrap();
    config.push_str(extra);
    fs::write(&path, config).unwrap();
}

/// Pumps the pty until `marker` is on screen after `from` (whitespace
/// squeezed), answering each modal on the way with the key paired with
/// its squeezed question in `answers`, in whatever order they come up.
/// A modal is looked for only after the previous answer, and each answer
/// is followed by a resize: the next question may differ from the last
/// by a few cells, and a full repaint is what puts it on the pty whole.
/// Returns the text since `from`.
fn wait_for_marker_answering_modals(
    pty: &mut Pty,
    from: usize,
    marker: &str,
    answers: &[(&str, u8)],
    what: &str,
) -> String {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let mut pending: Vec<(&str, u8)> = answers.to_vec();
    let mut answered_at = from;
    let mut width = 140;
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
        if squeezed(&since).contains(&squeezed(marker)) {
            return since;
        }
        let recent = squeezed(&strip_ansi(&String::from_utf8_lossy(
            &pty.raw()[answered_at..],
        )));
        if let Some(index) = pending
            .iter()
            .position(|(question, _)| recent.contains(question))
        {
            let (_, key) = pending.remove(index);
            pty.read_for(HOTKEY_GAP);
            pty.send(&[key]).unwrap();
            pty.read_for(Duration::from_millis(500));
            answered_at = pty.raw().len();
            width = if width == 140 { 139 } else { 140 };
            pty.resize(60, width).unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: `{marker}` never appeared (modals still unanswered: {pending:?}):\n{since}"
        );
    }
}

/// Files under `dir` (outside the cache and the bundle's own source
/// repository) whose contents mention `needle`.
fn config_files_mentioning(dir: &Path, needle: &str) -> Vec<PathBuf> {
    walk(dir)
        .into_iter()
        .filter(|path| {
            let text = path.to_string_lossy();
            !text.contains("/cache/") && !text.contains("/bundle-src/")
        })
        .filter(|path| {
            fs::read(path)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
                .unwrap_or(false)
        })
        .collect()
}

/// The head of the notice an install prints once every missing secret
/// has been added or deferred; the deferred names follow it.
const DEFERRED_SECRETS_NOTICE: &str = "still required by your MCP servers";

/// `.install <local bundle>` whose mcp.json references two secrets the
/// vault does not hold: under the UI each `Add {{ X }} to the vault now?` is
/// a Confirm modal; declining both, the install completes, names both
/// secrets as deferred, leaves the merged mcp.json and the bundle record
/// behind (nothing half-applied), adds no secret to the vault, and the
/// UI keeps taking keys and leaves clean. Then `.install <bundle>`
/// updates it from a source that gained a third secret and dropped a
/// role the user had edited locally: the obsolete file's fate is a Select
/// modal (`keep` keeps it), the secrets are declined again, the update
/// completes, names the new secret, and keeps the edited file.
#[test]
#[serial]
fn dot_install_of_a_bundle_with_secrets_defers_every_secret_the_user_declines() {
    let dir = fresh_config_dir("install-bundle-secrets");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![]);
    write_fake_llm_config(&dir, &fake);
    let name = "bundle-k7s";
    let repo = write_bundle_repo(&dir, name, &["BUNDLE_SECRET_A", "BUNDLE_SECRET_B"]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&dir);
    let install_at = pty.raw().len();
    let sent = std::time::Instant::now();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    let since = wait_for_marker_answering_modals(
        &mut pty,
        install_at,
        DEFERRED_SECRETS_NOTICE,
        &[
            ("Add{{BUNDLE_SECRET_A}}tothevaultnow?", b'n'),
            ("Add{{BUNDLE_SECRET_B}}tothevaultnow?", b'n'),
        ],
        "`.install` of a bundle with secrets",
    );
    let took = sent.elapsed();
    pty.read_for(Duration::from_millis(800));
    let since_full = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    for secret in ["BUNDLE_SECRET_A", "BUNDLE_SECRET_B"] {
        assert!(
            squeezed(&since_full).contains(secret),
            "the install did not name {secret} as deferred (took {took:?}):\n{since}"
        );
    }
    assert!(
        squeezed(&since_full).matches("tothevaultnow?").count() >= 2,
        "the install did not ask about each secret in the modal:\n{since_full}"
    );
    assert!(
        !since_full.contains("exit="),
        "`.install` quit the UI:\n{since_full}"
    );

    // Nothing half-applied: the merged mcp.json holds the bundle's server
    // and the bundle is recorded; the vault gained no secret.
    let mcp_files = config_files_mentioning(&dir, "bundle-k7s-server");
    assert!(
        mcp_files
            .iter()
            .any(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
        "the merged mcp.json does not hold the bundle's server; files mentioning it: {mcp_files:?}\n{since_full}"
    );
    let recorded = new_files_named(&dir, &files_before, name);
    assert!(
        !recorded.is_empty(),
        "no file records the installed bundle `{name}`:\n{since_full}"
    );
    let installed_role = dir.join("roles").join(format!("{name}-role.md"));
    assert!(
        installed_role.exists(),
        "the bundle's role was not installed at {}:\n{since_full}",
        installed_role.display()
    );
    let secret_holders = config_files_mentioning(&dir, "BUNDLE_SECRET_A");
    assert!(
        secret_holders
            .iter()
            .all(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
        "a file other than mcp.json holds BUNDLE_SECRET_A (a vault write?): {secret_holders:?}"
    );

    // `.list bundles` names the bundle and the UI is live.
    let list_at = pty.raw().len();
    pty.send(b".list bundles\r").unwrap();
    pty.wait_for_after(name, list_at, PTY_DEADLINE)
        .expect("`.list bundles` did not name the installed bundle");
    pty.read_for(Duration::from_millis(500));

    // The user edits a bundle file locally; the source then drops that
    // file and gains a third secret.
    let obsolete = dir.join("roles").join(format!("{name}-obsolete.md"));
    assert!(
        obsolete.exists(),
        "the bundle's second role was not installed"
    );
    fs::write(
        &obsolete,
        "---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n",
    )
    .unwrap();
    fs::remove_file(repo.join("roles").join(format!("{name}-obsolete.md"))).unwrap();
    fs::write(
        repo.join("mcp.json"),
        bundle_mcp_json(&["BUNDLE_SECRET_A", "BUNDLE_SECRET_B", "BUNDLE_SECRET_C"]),
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "update"]);

    let update_at = pty.raw().len();
    pty.send(format!(".install {name}\r").as_bytes()).unwrap();
    // The repaints forced on the way re-emit the first install's notice,
    // so the update is awaited by its own deferred list: B and C are
    // adjacent rows only there.
    let since = wait_for_marker_answering_modals(
        &mut pty,
        update_at,
        "{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}",
        &[
            ("Obsoletefile", b'1'),
            ("Add{{BUNDLE_SECRET_A}}tothevaultnow?", b'n'),
            ("Add{{BUNDLE_SECRET_B}}tothevaultnow?", b'n'),
            ("Add{{BUNDLE_SECRET_C}}tothevaultnow?", b'n'),
        ],
        "`.install <bundle>` update with a new secret",
    );
    pty.read_for(Duration::from_millis(800));
    let since_full = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
    assert!(
        squeezed(&since_full).contains("Add{{BUNDLE_SECRET_C}}tothevaultnow?"),
        "the update did not ask about the new secret in the modal:\n{since_full}"
    );
    assert!(
        !since_full.contains("exit="),
        "the update quit the UI:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&obsolete).ok().as_deref(),
        Some("---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n"),
        "the locally edited obsolete file was not kept by the update:\n{since_full}"
    );
    let merged = config_files_mentioning(&dir, "BUNDLE_SECRET_C");
    assert!(
        merged
            .iter()
            .any(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
        "the update did not merge the new secret into mcp.json: {merged:?}\n{since_full}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("`.info` after the install and update never rendered");
    assert_eq!(fake.served(), 0, "a bundle install reached the model");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------
// The very first run on a terminal: the config wizard's vault step with
// the password file put somewhere other than the offered default.
// ---------------------------------------------------------------------

/// The first run ever (no config file) on a real terminal, answered up
/// to the vault step with the local provider: the password-file Input
/// offers a default path; typing a different one and a ten-plus
/// character password creates the file THERE (mode 0600, the password
/// as its bytes), the success line names the chosen path (`Password
/// file '<chosen>' created.`), nothing lands at the offered default, and
/// the wizard goes on to the API provider pick. Ctrl+C there leaves
/// without writing a config file and with the tty restored.
#[test]
#[serial]
fn the_first_run_wizard_creates_the_password_file_at_the_chosen_path_and_says_so() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fresh_config_dir("first-run-chosen-password-file");
    let _guard = TempDirGuard(dir.clone());
    assert!(!dir.join("config.yaml").exists());
    let chosen = dir.join("chosen-vault-pass");
    let password = "twelve-chars-long";

    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");
    pty.wait_for("create a new one?", PTY_DEADLINE)
        .expect("the first run did not ask to create a config file");
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.wait_for("secrets provider", PTY_DEADLINE)
        .expect("the wizard did not ask which secrets provider to use");
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.wait_for("create one now?", PTY_DEADLINE)
        .expect("the wizard did not offer to create the password file");
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    let path_at = pty.raw().len();
    pty.wait_for("path to the password file to create", PTY_DEADLINE)
        .expect("the wizard did not ask where to create the password file");
    // The default is painted a moment after the label, so it is waited
    // for rather than read off the first frame that shows the question.
    pty.wait_for_after("coyote_password", path_at, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("the path question shows no default path: {e:?}"));
    pty.read_for(Duration::from_millis(300));
    pty.send(format!("{}\r", chosen.display()).as_bytes())
        .unwrap();
    pty.wait_for("encrypt all vault secrets", PTY_DEADLINE)
        .expect("the wizard did not ask for the vault password");
    pty.read_for(Duration::from_millis(300));
    pty.send(format!("{password}\r").as_bytes()).unwrap();
    let created = pty
        .wait_for("created.", PTY_DEADLINE)
        .expect("the wizard never said the password file was created");
    let expected = format!("Password file '{}' created.", chosen.display());
    assert!(
        squeezed(&created).contains(&squeezed(&expected)),
        "the success line does not name the chosen path `{expected}`:\n{created}"
    );
    let default_line = created
        .lines()
        .find(|line| line.contains("created.") && line.contains("coyote_password"));
    assert!(
        default_line.is_none(),
        "the success line names the default path, not the chosen one: {default_line:?}\n{created}"
    );
    assert_eq!(
        fs::read_to_string(&chosen).ok().as_deref(),
        Some(password),
        "the chosen path does not hold the password typed:\n{created}"
    );
    assert_eq!(
        fs::metadata(&chosen).unwrap().permissions().mode() & 0o777,
        0o600,
        "the created password file is not private (0600):\n{created}"
    );
    let at_default: Vec<PathBuf> = walk(&dir)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("coyote_password"))
        .collect();
    assert!(
        at_default.is_empty(),
        "a file landed at the default password path {at_default:?} though another was chosen:\n{created}"
    );

    pty.wait_for("API Provider", PTY_DEADLINE)
        .expect("the wizard did not go on to the API provider pick");
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x03").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_ne!(
        exit_code(&transcript),
        0,
        "Ctrl+C on the provider pick exited 0:\n{transcript}"
    );
    assert!(
        !dir.join("config.yaml").exists(),
        "the interrupted wizard wrote config.yaml:\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

/// The uninstall twin: `.uninstall <bundle>`, `y`, then Esc on the
/// `was modified locally after install` Select modal keeps the edited
/// file (the unmodified one is removed), the report still prints, one
/// note says the question was cancelled, and the UI stays live.
#[test]
#[serial]
fn esc_on_the_uninstall_modified_file_select_modal_keeps_the_file_and_says_so() {
    let dir = fresh_config_dir("uninstall-esc-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-u8e";
    let repo = write_bundle_repo(&dir, name, &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    install_bundle_without_questions(&mut pty, &repo);
    let role = dir.join("roles").join(format!("{name}-role.md"));
    let edited = dir.join("roles").join(format!("{name}-obsolete.md"));
    fs::write(&edited, EDITED_OBSOLETE).unwrap();

    let at = pty.raw().len();
    pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
    wait_for_the_modal(
        &mut pty,
        at,
        ".uninstall",
        &[&format!("Uninstall bundle '{name}'?"), "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_the_modal(
        &mut pty,
        at,
        ".uninstall (modified file)",
        &["wasmodifiedlocallyafterinstall", "keep", "delete-all"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        "the question was cancelled",
        "the note after Esc on the modified-file modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
    let flat = squeezed(&since);
    assert_eq!(
        flat.matches("thequestionwascancelled").count(),
        1,
        "the cancelled note was printed more or less than once:\n{since}"
    );
    assert!(
        flat.contains("keptmodifiedfile") && flat.contains("partiallyuninstalled"),
        "the cancelled question did not resolve to keep with the report:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&edited).ok().as_deref(),
        Some(EDITED_OBSOLETE),
        "Esc on the modified-file modal did not keep the edited file:\n{since}"
    );
    assert!(
        !role.exists(),
        "the unmodified role survived the uninstall:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a question released the terminal:\n{since}"
    );
    assert_ui_alive_then_quit(&mut pty, "the cancelled modified-file question");
}

/// Whether `pid` still exists (signal 0 is checked, not delivered).
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 touches no memory; it only checks the pid.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Reads the pid a loader fixture wrote next to its input.
fn loader_pid(path: &Path) -> i32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

/// A loader that records its pid at `$1.pid` and then sleeps far past the
/// deadline; the file-branch twin would copy `$1` to `$2` afterwards, the
/// stdout twin would `cat` it.
fn slow_loader(dir: &Path, file_branch: bool) -> PathBuf {
    let script = dir.join(if file_branch {
        "slow-file.sh"
    } else {
        "slow-stdout.sh"
    });
    let tail = if file_branch {
        "cp \"$1\" \"$2\"\n"
    } else {
        "cat \"$1\"\n"
    };
    fs::write(
        &script,
        format!("#!/usr/bin/env bash\necho $$ > \"$1.pid\"\nsleep 30\n{tail}"),
    )
    .unwrap();
    script
}

/// A loader that writes its would-be output and is then killed by a
/// signal before it can exit cleanly.
fn dying_loader(dir: &Path, file_branch: bool) -> PathBuf {
    let script = dir.join(if file_branch {
        "dying-file.sh"
    } else {
        "dying-stdout.sh"
    });
    let body = if file_branch {
        "echo 'PARTIAL LOADER OUTPUT d4k' > \"$2\"\nkill -KILL $$\n"
    } else {
        "echo 'PARTIAL LOADER OUTPUT d4k'\nkill -KILL $$\n"
    };
    fs::write(&script, format!("#!/usr/bin/env bash\n{body}")).unwrap();
    script
}

/// `.file <doc>` through a loader that sleeps: Esc ends the turn within
/// about two seconds, the loader process is gone by then, nothing reached
/// the model, and the UI keeps taking keys and leaves clean.
fn esc_interrupts_a_slow_loader(scenario: &str, file_branch: bool) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("never reached m2x".to_string())]);
    let script = slow_loader(&dir, file_branch);
    let args = if file_branch { "$1 $2" } else { "$1" };
    write_fake_llm_config_with(
        &dir,
        &fake,
        &format!(
            "document_loaders:\n\x20 slow: 'bash {} {args}'\n",
            script.display()
        ),
    );
    let document = dir.join("doc.slow");
    fs::write(&document, "slow document body\n").unwrap();
    let pid_file = dir.join("doc.slow.pid");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(format!(".file {} -- summarize\r", document.display()).as_bytes())
        .unwrap();
    wait_for_file(&mut pty, &pid_file, "the slow loader's pid file");
    let pid = loader_pid(&pid_file);
    assert!(process_alive(pid), "the loader exited before Esc");
    pty.read_for(Duration::from_millis(500));
    let pressed_at = pty.raw().len();
    let pressed = std::time::Instant::now();
    pty.send(b"\x1b").unwrap();

    let mut gone_after = None;
    let mut ended_after = None;
    let deadline = pressed + Duration::from_secs(8);
    while (gone_after.is_none() || ended_after.is_none()) && std::time::Instant::now() < deadline {
        pty.read_for(Duration::from_millis(100));
        if gone_after.is_none() && !process_alive(pid) {
            gone_after = Some(pressed.elapsed());
        }
        if ended_after.is_none() {
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
            let lower = since.to_lowercase();
            if lower.contains("aborted") || lower.contains("interrupted") {
                ended_after = Some(pressed.elapsed());
            }
        }
    }
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    let gone_after = gone_after.unwrap_or_else(|| {
        panic!("{scenario}: the loader (pid {pid}) is still alive 8s after Esc:\n{since}")
    });
    let ended_after = ended_after.unwrap_or_else(|| {
        panic!("{scenario}: no aborted/interrupted cell followed Esc within 8s:\n{since}")
    });
    assert!(
        gone_after < Duration::from_millis(2500),
        "{scenario}: the loader outlived Esc by {gone_after:?}:\n{since}"
    );
    assert!(
        ended_after < Duration::from_millis(2500),
        "{scenario}: the turn ended {ended_after:?} after Esc:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "{scenario}: Esc quit the UI:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the interrupted loader");
    assert_eq!(
        fake.served(),
        0,
        "{scenario}: the interrupted document reached the model"
    );
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        !squeezed(&transcript).contains("neverreachedm2x"),
        "{scenario}: the model's reply landed after the interrupt:\n{transcript}"
    );
}

/// File-branch loader (`cmd $1 $2`).
#[test]
#[serial]
fn esc_interrupts_a_slow_file_loader_and_kills_it() {
    esc_interrupts_a_slow_loader("esc-slow-file-loader", true);
}

/// Stdout-branch loader (`cmd $1`).
#[test]
#[serial]
fn esc_interrupts_a_slow_stdout_loader_and_kills_it() {
    esc_interrupts_a_slow_loader("esc-slow-stdout-loader", false);
}

/// `.file <doc>` through a loader killed by a signal after writing its
/// output: the turn fails (an error cell, no model call with the partial
/// output), and the UI keeps taking keys and leaves clean.
fn a_signal_killed_loader_is_a_failure(scenario: &str, file_branch: bool) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("ingested anyway r8p".to_string())]);
    let script = dying_loader(&dir, file_branch);
    let args = if file_branch { "$1 $2" } else { "$1" };
    write_fake_llm_config_with(
        &dir,
        &fake,
        &format!(
            "document_loaders:\n\x20 dying: 'bash {} {args}'\n",
            script.display()
        ),
    );
    let document = dir.join("doc.dying");
    fs::write(&document, "dying document body\n").unwrap();
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let sent_at = pty.raw().len();
    pty.send(format!(".file {} -- summarize\r", document.display()).as_bytes())
        .unwrap();
    // The turn must end one way or the other: wait until the status row no
    // longer says Working, then look at what landed.
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        pty.read_for(Duration::from_millis(300));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..]));
        // The failure the command module raises for a child that did not
        // exit 0 (`exited with non-zero`), squeezed because the error cell
        // wraps; no looser word, so the scenario's own name in the echoed
        // command line cannot satisfy it.
        let squeezed_since = squeezed(&since);
        if squeezed_since.contains("ingestedanywayr8p")
            || squeezed_since.contains("exitedwithnon-zero")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{scenario}: neither a failure nor a reply followed the killed loader:\n{since}"
        );
    }
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..]));
    assert_eq!(
        fake.served(),
        0,
        "{scenario}: the output of a signal-killed loader was sent to the model:\n{since}"
    );
    assert!(
        !squeezed(&since).contains("ingestedanywayr8p"),
        "{scenario}: the model replied to a document whose loader was killed:\n{since}"
    );
    assert!(
        squeezed(&since).contains("exitedwithnon-zero"),
        "{scenario}: nothing told the user the loader failed:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "{scenario}: the failed loader quit the UI:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the failed loader");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// File-branch loader killed after writing `$2`.
#[test]
#[serial]
fn a_signal_killed_file_loader_is_reported_as_a_failure_not_ingested() {
    a_signal_killed_loader_is_a_failure("dying-file-loader", true);
}

/// Stdout-branch loader killed after writing its stdout.
#[test]
#[serial]
fn a_signal_killed_stdout_loader_is_reported_as_a_failure_not_ingested() {
    a_signal_killed_loader_is_a_failure("dying-stdout-loader", false);
}

// ---------------------------------------------------------------------------
// Esc on the incorporate question, `.rag` with no name and a seeded temp
// rag, a stdout loader that reads stdin outside the UI, conceal with a
// leading zero, and a tool whose background job keeps writing after its
// turn.
// ---------------------------------------------------------------------------

/// Esc on the incorporate question cancels it: no session opens (the
/// footer still says `no session` after a full repaint, the modal is gone,
/// nothing is written under the config dir), the UI keeps taking keys, and
/// `.session <name>` asked again and answered with `n` seats the session
/// empty.
#[test]
#[serial]
fn esc_on_the_incorporate_question_opens_no_session_and_the_next_n_seats_it_empty() {
    let dir = fresh_config_dir("session-after-turn-esc");
    let _guard = TempDirGuard(dir.clone());
    let name = "after-turn-esc3";
    let (mut pty, _fake) = open_session_after_a_turn(&dir, name);
    let files_before = walk(&dir);

    let pressed_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(800));
    // A different size from the one `answer_and_expect_session` grows to,
    // so both repaints are forced.
    pty.resize(60, 110).unwrap();
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    assert!(
        squeezed(&since).contains("nosession"),
        "the footer does not say `no session` after Esc on the incorporate question:\n{since}"
    );
    assert!(
        !squeezed(&since).contains("Esc=cancel"),
        "the modal is still up after Esc:\n{since}"
    );
    // The command echo `> .session <name>` sits in the transcript; the
    // footer is the last row, and the name must not be painted there.
    let on_footer: Vec<_> = painted_at(pty.raw(), name.as_bytes(), pressed_at)
        .into_iter()
        .filter(|(row, _)| *row == 60)
        .collect();
    assert!(
        on_footer.is_empty(),
        "the footer names `{name}` after Esc on the incorporate question:\n{since}"
    );
    let written = new_files_named(&dir, &files_before, name);
    assert!(
        written.is_empty(),
        "Esc on the incorporate question still wrote {written:?}:\n{since}"
    );

    // Asked again, the question comes back (the exchange is still there
    // and no session took it), and `n` seats the session empty.
    let asked_at = pty.raw().len();
    pty.send(format!(".session {name}\r").as_bytes()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
        if squeezed(&since).contains("incorporatesthelastquestionandanswer") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`.session` after Esc did not ask about the last exchange again within 10s:\n{since}"
        );
    }
    pty.read_for(HOTKEY_GAP);
    answer_and_expect_session(pty, b"n", name, true);
}

/// `.rag` with no name recreates the temporary rag by asking which
/// embedding model and documents to use, and removes the previous one
/// only once the wizard has answered; under the UI the embedding model is
/// asked in the Select modal while the seeded temp rag stays on disk
/// untouched. Esc cancels it with the old temp rag still in place and
/// byte-identical, the terminal is never released, and the UI keeps
/// taking keys and leaves clean.
#[test]
#[serial]
fn dot_rag_without_a_name_asks_in_the_select_modal_and_keeps_the_temp_rag_until_it_answers() {
    let dir = fresh_config_dir("modal-rag-temp");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let rags = dir.join("rags");
    fs::create_dir_all(&rags).unwrap();
    let temp_rag = rags.join("temp.yaml");
    let seeded = "embedding_model: dryrun:dry-embed-a\nchunk_size: 1000\nchunk_overlap: 50\ndocument_paths: []\nfiles: []\nmarker: temp-rag-seeded-t3g\n";
    fs::write(&temp_rag, seeded).unwrap();
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b".rag\r").unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        before,
        ".rag",
        &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
    );
    assert_eq!(
        fs::read_to_string(&temp_rag).ok().as_deref(),
        Some(seeded),
        "`.rag` removed or rewrote the temp rag {} before the modal was answered:\n{modal}",
        temp_rag.display()
    );
    esc_cancels_the_modal(&mut pty, ".rag");

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("`.info` after the cancelled `.rag` never rendered");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        fs::read_to_string(&temp_rag).ok().as_deref(),
        Some(seeded),
        "the cancelled `.rag` removed or rewrote the temp rag by the time the UI left:\n{transcript}"
    );
}

/// A stdout-branch loader (`cmd $1`) never inherits the terminal on its
/// stdin, outside the UI as much as under it: in the plain command mode
/// on a tty, `cat "$1" -` returns once the file is read, so the command
/// finishes within the deadline with no key typed (a loader left reading
/// the terminal would wait there until the deadline fails the test), the
/// document reaches the model exactly once, the reply is printed and the
/// command exits 0. No tighter wall-clock bound is asserted: a loaded
/// runner stretches it.
#[test]
#[serial]
fn a_stdout_loader_reading_stdin_returns_in_command_mode_on_a_tty() {
    let dir = fresh_config_dir("loader-stdin-cmd");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("loaded document reply s7d".to_string())]);
    write_fake_llm_config_with(
        &dir,
        &fake,
        "document_loaders:\n\x20 catstdin: 'cat \"$1\" -'\n",
    );
    let document = dir.join("doc.catstdin");
    fs::write(&document, "document body s7d-body\n").unwrap();
    let args = format!(" -f '{}' 'summarize'", document.display());
    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[("COYOTE_UI", "repl")],
        WAIT_WRAPPER,
        "/dev/tty",
        &args,
    );
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE)
        .unwrap_or_else(|e| {
            panic!(
                "the command never finished: the loader is waiting on the terminal's stdin: \
                 {e:?}\n{}",
                strip_ansi(&String::from_utf8_lossy(pty.raw()))
            )
        });
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_eq!(
        fake.served(),
        1,
        "the document never reached the model:\n{transcript}"
    );
    assert!(
        squeezed(&transcript).contains("loadeddocumentreplys7d"),
        "the reply was not printed:\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

/// Conceal written with a leading zero (`ESC[08m`) is still SGR 8: the
/// word is painted, no conceal parameter reaches the tty, the sequence is
/// never painted as text, a full repaint keeps the word, and nothing of
/// it reaches the normal screen after Ctrl+D.
#[test]
#[serial]
fn a_reply_concealed_with_a_leading_zero_shows_it_and_sends_no_conceal_to_the_tty() {
    let dir = fresh_config_dir("conceal-zero-visible");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(
        "before c9a \x1b[08mhidden-word-c9b\x1b[0m after c9c".to_string(),
    )]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let submitted_at = pty.raw().len();
    pty.send(b"conceal please\r").unwrap();
    let transcript = wait_for_squeezed(&mut pty, "after c9c", "the reply's tail");
    assert_eq!(fake.served(), 1, "{transcript}");
    pty.read_for(Duration::from_millis(600));

    let raw = pty.raw();
    let visible = strip_ansi(&String::from_utf8_lossy(&raw[submitted_at..]));
    assert!(
        squeezed(&visible).contains("hidden-word-c9b"),
        "the word concealed with ESC[08m was not painted:\n{visible}"
    );
    assert!(
        !visible.contains("[08m") && !visible.contains("[8m") && !visible.contains("[0m"),
        "the conceal sequence was painted as text:\n{visible}"
    );
    let concealing: Vec<_> = sgr_params(&raw[submitted_at..])
        .into_iter()
        .filter(|p| is_conceal_sgr(p))
        .collect();
    assert!(
        concealing.is_empty(),
        "a conceal SGR reached the tty: {concealing:?}\n{visible}"
    );

    let repaint_at = raw.len();
    pty.resize(30, 90).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let repainted = loop {
        pty.read_for(Duration::from_millis(200));
        let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[repaint_at..]));
        if squeezed(&repainted).contains("afterc9c") {
            break repainted;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reply was not repainted after the resize:\n{repainted}"
        );
    };
    assert!(
        squeezed(&repainted).contains("hidden-word-c9b"),
        "the concealed word vanished on repaint:\n{repainted}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let left_at = find(pty.raw(), LEAVE_ALT, 0).expect("the UI never left the alternate screen");
    let after_leave = &pty.raw()[left_at..];
    let visible_after = strip_ansi(&String::from_utf8_lossy(after_leave));
    assert!(
        !visible_after.contains("[08m") && !visible_after.contains("[8m"),
        "the conceal sequence reached the normal screen as text:\n{visible_after}"
    );
    let concealing_after: Vec<_> = sgr_params(after_leave)
        .into_iter()
        .filter(|p| is_conceal_sgr(p))
        .collect();
    assert!(
        concealing_after.is_empty(),
        "a conceal SGR reached the normal screen: {concealing_after:?}\n{visible_after}"
    );
}

/// A tool that leaves a background job writing to its stdout after the
/// tool itself (and its turn) finished.
const LINGERING_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Finish at once, leaving a job that prints later.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   ( sleep 4; echo 'late-line-one-k7a'; sleep 0.3; echo 'late-line-two-k7b'; sleep 0.3; echo 'late-line-three-k7c' ) &\n\
    \x20   echo 'fixture lingering started k7s' >> \"$LLM_OUTPUT\"\n\
    }\n";

/// A tool whose background job keeps writing after the turn ended does
/// not disturb the UI: the turn completes, the UI stays live and leaves
/// clean. The job's writes arrive after the tool's output pump was
/// detached (the drain grace is two seconds), so the binary drops them
/// rather than attributing them to a finished tool; should any of them
/// be shown, they share one trailing cell (adjacent rows, no blank cell
/// gap between the lines) rather than a cell per pipe read, as the
/// `out_of_turn_tool_output_shares_one_bounded_cell` unit test pins
/// in-process.
#[test]
#[serial]
fn a_tool_leaving_a_lingering_writer_behind_keeps_the_ui_live_and_folds_its_output() {
    let dir = fresh_config_dir("tool-late-output");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_lingering".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done after lingering k7d".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_lingering.sh", LINGERING_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let submitted_at = pty.raw().len();
    pty.send(b"call the lingering tool\r").unwrap();
    wait_for_squeezed(
        &mut pty,
        "fixture done after lingering k7d",
        "the reply after the tool",
    );
    assert_eq!(fake.served(), 2);
    // Outlive the job's writes, pumping the pty.
    pty.read_for(Duration::from_millis(6000));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]));
    let one = painted_at(pty.raw(), b"late-line-one-k7a", submitted_at);
    let two = painted_at(pty.raw(), b"late-line-two-k7b", submitted_at);
    let three = painted_at(pty.raw(), b"late-line-three-k7c", submitted_at);
    if let (Some(&(row_one, col_one)), Some(&(row_two, col_two))) = (one.last(), two.last()) {
        assert_eq!(
            (row_two, col_two),
            (row_one + 1, col_one),
            "the late lines did not share one cell:\n{since}"
        );
    }
    if let (Some(&(row_two, col_two)), Some(&(row_three, col_three))) = (two.last(), three.last()) {
        assert_eq!(
            (row_three, col_three),
            (row_two + 1, col_two),
            "the late lines did not share one cell:\n{since}"
        );
    }
    assert!(
        !since.contains("exit="),
        "the lingering job's output quit the UI:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the lingering tool");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------------
// A tool question carrying invisible format characters, a reply in palette
// grey, a rebuild whose document fails to load, and an MCP conflict on
// `.install` under the UI and headless.
// ---------------------------------------------------------------------------

/// The UTF-8 of each format character the question below carries, so the
/// raw terminal stream can be searched for them.
const FORMAT_CHARACTERS: [(&str, &[u8]); 5] = [
    ("U+202E right-to-left override", "\u{202E}".as_bytes()),
    ("U+202C pop directional formatting", "\u{202C}".as_bytes()),
    ("U+200B zero-width space", "\u{200B}".as_bytes()),
    ("U+200F right-to-left mark", "\u{200F}".as_bytes()),
    ("U+FEFF byte-order mark", "\u{FEFF}".as_bytes()),
];

/// A confirm tool whose question wraps the thing being approved in a
/// right-to-left override (which would display `txt.safe` reversed), pads
/// it with a zero-width space and ends in a right-to-left mark and a
/// byte-order mark.
fn format_character_confirm_tool() -> String {
    RECORDING_CONFIRM_TOOL.replace(
        "Really do it? q9d-q",
        "Delete \u{202E}txt.safe\u{202C} \u{200B}k7m\u{200F}\u{FEFF}? q9d-q",
    )
}

/// A tool-authored question carrying bidi overrides and zero-width
/// characters is drawn in its stored order without them: the modal reads
/// `Delete txt.safe k7m?`, none of the characters reaches the terminal
/// (where a real emulator would reorder or hide the text around them),
/// `y` still answers the tool as a yes, and nothing of them is left on
/// the normal screen after the UI leaves.
#[test]
#[serial]
fn a_tools_confirm_question_with_format_characters_is_drawn_without_them() {
    let dir = fresh_config_dir("tool-confirm-format-chars");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done f8c".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_confirm_rec.sh",
        &format_character_confirm_tool(),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let submitted_at = pty.raw().len();
    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for("q9d-q", PTY_DEADLINE)
        .expect("the tool's confirm question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]));
    assert!(
        squeezed(&since).contains("Deletetxt.safek7m?q9d-q"),
        "the question was not drawn in its stored order with the format characters dropped:\n{since}"
    );
    for (name, bytes) in FORMAT_CHARACTERS {
        assert!(
            find(pty.raw(), bytes, submitted_at).is_none(),
            "{name} reached the terminal while the modal was up:\n{since}"
        );
    }

    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done f8c", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` on the scrubbed question did not reach the tool as a yes:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    for (name, bytes) in FORMAT_CHARACTERS {
        assert!(
            find(pty.raw(), bytes, submitted_at).is_none(),
            "{name} reached the terminal by the time the UI left:\n{transcript}"
        );
    }
}

/// A reply coloured with the palette index 8 (`ESC[38;5;8m`, the grey
/// most prompts use for de-emphasis) is painted as colour: the word is
/// visible, the sequence is not drawn as text, the palette foreground is
/// what reaches the terminal, and nothing conceals. The `8` inside an
/// extended colour is a colour argument, not the conceal attribute.
#[test]
#[serial]
fn a_reply_in_palette_grey_is_painted_as_colour_not_stripped_as_conceal() {
    let dir = fresh_config_dir("palette-grey");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(
        "before g8a \x1b[38;5;8mgrey-word-g8b\x1b[0m after g8c".to_string(),
    )]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let submitted_at = pty.raw().len();
    pty.send(b"grey please\r").unwrap();
    let transcript = wait_for_squeezed(&mut pty, "after g8c", "the reply's tail");
    assert_eq!(fake.served(), 1, "{transcript}");
    pty.read_for(Duration::from_millis(600));

    let raw = pty.raw();
    let visible = strip_ansi(&String::from_utf8_lossy(&raw[submitted_at..]));
    assert!(
        squeezed(&visible).contains("grey-word-g8b"),
        "the grey word was not painted:\n{visible}"
    );
    assert!(
        !visible.contains("[38;5;8m") && !visible.contains("[0m"),
        "the colour sequence was painted as text:\n{visible}"
    );
    let params = sgr_params(&raw[submitted_at..]);
    assert!(
        params.iter().any(|p| p.windows(3).any(|w| w == [38, 5, 8])),
        "the palette-grey foreground never reached the terminal (stripped as conceal?); SGRs seen: {params:?}\n{visible}"
    );
    let concealing: Vec<_> = params.iter().filter(|p| is_conceal_sgr(p)).collect();
    assert!(
        concealing.is_empty(),
        "a conceal SGR reached the tty: {concealing:?}\n{visible}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The dry-run config with two embedding models and a document loader
/// for the `broken` extension that always fails. Returns the loader.
fn write_dry_run_config_with_a_failing_loader(dir: &Path) -> PathBuf {
    let script = dir.join("broken-loader.sh");
    fs::write(
        &script,
        "#!/usr/bin/env bash\necho 'loader refused r8l' >&2\nexit 1\n",
    )
    .unwrap();
    write_config(
        dir,
        &format!(
            "\x20     - name: dry-embed-a\n\
             \x20       type: embedding\n\
             \x20       max_input_tokens: 2048\n\
             \x20       default_chunk_size: 1000\n\
             \x20       max_batch_size: 100\n\
             save_session: false\n\
             document_loaders:\n\
             \x20 broken: 'bash {} $1'\n",
            script.display()
        ),
    );
    script
}

/// A named yaml rag over one document whose loader fails, with no files
/// indexed yet, so `.rag <name>` loads it and `.rebuild rag` has to load
/// the document.
fn seed_rag_with_a_failing_document(dir: &Path, name: &str) -> (PathBuf, String) {
    let document = dir.join("doc.broken");
    fs::write(&document, "body\n").unwrap();
    let rags = dir.join("rags");
    fs::create_dir_all(&rags).unwrap();
    let rag_path = rags.join(format!("{name}.yaml"));
    let seeded = format!(
        "driver: yaml\n\
         embedding_model: dryrun:dry-embed-a\n\
         chunk_size: 1000\n\
         chunk_overlap: 50\n\
         top_k: 5\n\
         document_paths:\n\
         - '{}'\n\
         files: {{}}\n\
         marker: rag-survives-r8r\n",
        document.display()
    );
    fs::write(&rag_path, &seeded).unwrap();
    (rag_path, seeded)
}

/// `.rebuild rag` over a document whose loader fails: the sync's
/// `Some documents failed to load. Continue?` is the Confirm modal under
/// the UI (not a raw-tty `[y/N]` prompt, not an abort naming the plain
/// REPL); `n` aborts with `Aborted: some documents failed to load` after
/// the load and warning rows for the document. The rag file is left
/// exactly as it was, the terminal is never released, and the UI keeps
/// taking keys and leaves clean.
#[test]
#[serial]
fn dot_rebuild_rag_with_a_failing_document_asks_continue_in_the_confirm_modal_and_n_aborts() {
    let dir = fresh_config_dir("rebuild-rag-failed-doc");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_a_failing_loader(&dir);
    let name = "kb-w8";
    let (rag_path, seeded) = seed_rag_with_a_failing_document(&dir, name);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 120).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(format!(".rag {name}\r").as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(1500));
    let before = pty.raw().len();
    pty.send(b".rebuild rag\r").unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        before,
        ".rebuild rag",
        &["Somedocumentsfailedtoload.Continue?", "y=yes"],
    );
    assert!(
        !modal.contains("No RAG"),
        "`.rag {name}` did not seat the rag, so the rebuild had nothing to work on:\n{modal}"
    );
    let flat_modal = squeezed(&modal);
    assert!(
        !flat_modal.contains("[y/N]") && !flat_modal.contains("(y/N)"),
        "`Continue?` was asked on the raw tty, not in the modal:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    wait_for_squeezed(
        &mut pty,
        "Aborted: some documents failed to load",
        "the abort after `n` on the modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let squeezed_since = squeezed(&since);
    // The load row names the document's full path, which a Command cell
    // cuts at the pane's edge; the row's head and the scenario dir are
    // enough to place it.
    assert!(
        squeezed_since.contains("Load/") && squeezed_since.contains("rebuild-rag-failed-doc"),
        "the load row for the document was not printed before the question:\n{since}"
    );
    assert!(
        // The warning row leads with the warning sign (U+26A0).
        squeezed_since.contains("loaderrefusedr8l") || squeezed_since.contains("\u{26a0}"),
        "the warning row for the failed document was not printed:\n{since}"
    );
    assert!(
        !squeezed_since.contains(&squeezed("Re-run in the plain REPL")),
        "the abort after `n` still names the plain REPL as the way to choose:\n{since}"
    );
    assert!(
        !squeezed_since.contains(&squeezed("Re-run in a terminal")),
        "the abort says `Re-run in a terminal`, but the UI is on a terminal:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "the aborted rebuild quit the UI:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&rag_path).unwrap(),
        seeded,
        "the aborted rebuild rewrote the rag file:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the aborted rebuild released the terminal:\n{since}"
    );

    let transcript = assert_ui_alive_then_quit(&mut pty, "the aborted rebuild");
    assert_eq!(
        fs::read_to_string(&rag_path).unwrap(),
        seeded,
        "the rag file changed by the time the UI left:\n{transcript}"
    );
}

/// `y` on the same modal lets the rebuild go on past the failed
/// document; with no other document the corpus is empty, so the rebuild
/// reports `No RAG files` instead of the abort, the rag file is left as
/// it was, and the UI keeps taking keys and leaves clean.
#[test]
#[serial]
fn dot_rebuild_rag_with_a_failing_document_and_y_goes_on_to_report_the_empty_corpus() {
    let dir = fresh_config_dir("rebuild-rag-failed-doc-y");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_a_failing_loader(&dir);
    let name = "kb-w8";
    let (rag_path, seeded) = seed_rag_with_a_failing_document(&dir, name);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 120).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(format!(".rag {name}\r").as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(1500));
    let before = pty.raw().len();
    pty.send(b".rebuild rag\r").unwrap();
    wait_for_the_modal(
        &mut pty,
        before,
        ".rebuild rag",
        &["Somedocumentsfailedtoload.Continue?", "y=yes"],
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_squeezed(
        &mut pty,
        "No RAG files",
        "the empty-corpus report after `y` on the modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert!(
        !squeezed(&since).contains(&squeezed("Aborted: some documents failed to load")),
        "`y` on the modal still aborted the rebuild:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "the rebuild over an empty corpus quit the UI:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&rag_path).unwrap(),
        seeded,
        "the rebuild over an empty corpus rewrote the rag file:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the rebuild over an empty corpus released the terminal:\n{since}"
    );

    assert_ui_alive_then_quit(&mut pty, "the rebuild over an empty corpus");
}

const MCP_CONFLICT_REFUSAL: &str =
    "Refusing to merge the already-installed MCP server 'bundle-k7s-server' non-interactively";
const TERMINAL_HINT: &str = "Re-run in a terminal";

/// `.install <local bundle>` whose `mcp.json` defines a server the local
/// `mcp.json` already holds differently: under the UI the menu that
/// resolves the conflict is the Select modal with the four choices,
/// `1` keeps the local definition, the install completes without a
/// refusal, the local server definition is untouched, the UI keeps
/// taking keys and leaves clean.
#[test]
#[serial]
fn dot_install_with_a_conflicting_mcp_server_asks_in_the_select_modal_and_keep_local_keeps_it() {
    let dir = fresh_config_dir("install-mcp-conflict-tui");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![]);
    write_fake_llm_config(&dir, &fake);
    let local_before = write_conflicting_local_mcp_server(&dir);
    let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    let sent = std::time::Instant::now();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        install_at,
        ".install",
        &[
            "ConflictonMCPserver'bundle-k7s-server'",
            "keeplocal",
            "takeremote",
            "bundle-k7s-server-remote",
            "abortmerge",
        ],
    );
    let took = sent.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "the menu took {took:?} to come up:\n{modal}"
    );
    assert!(
        !squeezed(&modal).contains(&squeezed(MCP_CONFLICT_REFUSAL)),
        "the merge was refused instead of asked in the modal:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"1").unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        if squeezed(&since).contains("keptlocal:bundle-k7s-server") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`keep local` did not let the merge report the kept server:\n{since}"
        );
    }
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let squeezed_since = squeezed(&since);
    assert!(
        !squeezed_since.contains(&squeezed(MCP_CONFLICT_REFUSAL)),
        "the merge was refused after the menu was answered:\n{since}"
    );
    assert!(
        !squeezed_since.contains(&squeezed(TERMINAL_HINT)),
        "the install under the UI points at another UI:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "the install quit the UI:\n{since}"
    );
    let installed_role = dir.join("roles").join("bundle-k7s-role.md");
    assert!(
        installed_role.exists(),
        "the bundle's role was not installed at {}:\n{since}",
        installed_role.display()
    );
    // The merge writes mcp.json back formatted; the server definition is
    // what `keep local` keeps.
    let local_before: serde_json::Value = serde_json::from_str(&local_before).unwrap();
    let merged = |dir: &Path| -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap()
    };
    assert_eq!(
        merged(&dir),
        local_before,
        "`keep local` changed the local mcp.json:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the conflict menu released the terminal:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("`.info` after the install never rendered");
    assert_eq!(fake.served(), 0, "a bundle install reached the model");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        merged(&dir),
        local_before,
        "the local mcp.json changed by the time the UI left:\n{transcript}"
    );
}

/// The same conflict with stdout redirected: `coyote --install <bundle>`
/// with a tty on stdin but stdout a file (the shape of `coyote --install
/// x > log`), no UI involved. The menu cannot be shown without a terminal
/// on stdout, so the merge is refused in the shared no-tty shape, naming
/// the server and ending with the CLI remedy word for word (not the plain
/// REPL's, which is not what stands in the way), the command fails, and
/// the local server definition is untouched.
#[test]
#[serial]
fn an_install_with_stdout_redirected_and_a_conflicting_mcp_server_is_refused_naming_a_terminal() {
    let dir = fresh_config_dir("install-mcp-conflict-stdout-file");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![]);
    write_fake_llm_config(&dir, &fake);
    let local_before = write_conflicting_local_mcp_server(&dir);
    let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
    let captured = dir.join("stdout.log");
    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[("COYOTE_UI", "repl")],
        &wait_wrapper_with_stdout_to(&captured),
        "/dev/tty",
        &format!(" --install '{}'", repo.display()),
    );

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    let file = String::from_utf8_lossy(&fs::read(&captured).unwrap_or_default()).to_string();
    let everything = format!("{transcript}\n{file}");
    let (action, remedy) = refusal_shape(&everything).unwrap_or_else(|| {
        panic!(
            "the install with an MCP conflict and stdout redirected was not refused in the shared shape:\n--- terminal ---\n{transcript}\n--- stdout file ---\n{file}"
        )
    });
    assert!(
        action.starts_with("merge the already-installed MCP server 'bundle-k7s-server'"),
        "the refusal does not name the MCP merge and the server: {action:?}\n{everything}"
    );
    assert_eq!(
        remedy, CLI_INSTALL_REMEDY,
        "the refusal does not end with the documented remedy:\n{everything}"
    );
    assert_ne!(
        exit_code(&transcript),
        0,
        "a refused install exited 0:\n{everything}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("mcp.json")).unwrap(),
        local_before,
        "the refused merge changed the local mcp.json:\n{everything}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "an install with stdout redirected entered the alternate screen:\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------------
// A file-writing loader that reads stdin under the plain REPL, escapes
// and format characters in a typed line and in an error notice, and late
// output arriving once the exit has begun.
// ---------------------------------------------------------------------------

/// A `$2` loader (the file-writing branch) that copies `$1` to `$2` and
/// then reads its stdin to the end, the way a filter that is handed a
/// closed stdin returns at once and one handed the terminal waits for a
/// key the user never types.
fn file_loader_reading_stdin(dir: &Path) -> PathBuf {
    let script = dir.join("file-loader-stdin.sh");
    fs::write(&script, "#!/usr/bin/env bash\ncat \"$1\" > \"$2\"\ncat -\n").unwrap();
    script
}

/// `.file <doc>` in the plain REPL on a tty, through a `$2` loader whose
/// process reads stdin after writing its output: the loader is spawned
/// with stdin closed on this branch too, so the reply arrives within the
/// deadline with no key typed (a loader left reading the terminal would
/// wait there until the deadline fails the test), the document reaches
/// the model exactly once, and `.exit` then leaves the tty cooked. No
/// tighter wall-clock bound is asserted: a loaded runner stretches it.
#[test]
#[serial]
fn dot_file_through_a_file_loader_reading_stdin_returns_at_once_in_the_plain_repl() {
    let dir = fresh_config_dir("file-loader-stdin-repl");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("loaded document reply r9f".to_string())]);
    let script = file_loader_reading_stdin(&dir);
    write_fake_llm_config_with(
        &dir,
        &fake,
        &format!(
            "document_loaders:\n\x20 catstdinfile: 'bash {} $1 $2'\n",
            script.display()
        ),
    );
    let document = dir.join("doc.catstdinfile");
    fs::write(&document, "document body r9f-body\n").unwrap();
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    // Type only once the line editor owns the terminal: input ahead of
    // the prompt is cooked-mode typeahead the editor discards.
    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let started = std::time::Instant::now();
    pty.send(format!(".file '{}' -- summarize\r", document.display()).as_bytes())
        .unwrap();
    let transcript = wait_for_squeezed(
        &mut pty,
        "loaded document reply r9f",
        "the reply after the file loader (is the loader waiting on the terminal's stdin?)",
    );
    let took = started.elapsed();
    assert_eq!(
        fake.served(),
        1,
        "the document never reached the model (the reply came back after {took:?}):\n{transcript}"
    );
    // Let the reply finish printing and the prompt come back before
    // typing the next command.
    pty.wait_for_after(
        "fake-model)>",
        pty.raw().len().saturating_sub(64),
        PTY_DEADLINE,
    )
    .expect("the prompt did not come back after the reply");
    pty.read_for(Duration::from_millis(300));
    let exit_started = std::time::Instant::now();
    pty.send(b".exit\r").unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE * 2)
        .unwrap_or_else(|e| {
            panic!(
                "the REPL did not exit (after {:?}): {e:?}",
                exit_started.elapsed()
            )
        });
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A line carrying a colour SGR, a zero-width space and an OSC 8
/// hyperlink, as pasted text would.
const ESCAPED_WORDS: &[u8] =
    b"see \x1b[31mred\x1b[0m \xe2\x80\x8bzw \x1b]8;;http://x.invalid\x1b\\link\x1b]8;;\x1b\\ end-q3u";

/// The same words joined into one path component (no spaces, which
/// `.file` would read as separate paths).
const ESCAPED_PATH: &[u8] =
    b"see-\x1b[31mred\x1b[0m-\xe2\x80\x8bzw-\x1b]8;;http://x.invalid\x1b\\link\x1b]8;;\x1b\\-end-q3u";

/// Nothing of the raw sequences or the format character may be painted
/// as text after `from`, and the words must be painted in order with
/// nothing between them (`plain` is the squeezed form expected).
fn assert_escaped_words_drawn_plain(pty: &Pty, from: usize, plain: &str, what: &str) {
    let since_raw = &pty.raw()[from..];
    let since = strip_ansi(&String::from_utf8_lossy(since_raw));
    for leaked in ["[31m", "[0m", "]8;;", "x.invalid"] {
        assert!(
            !since.contains(leaked),
            "{what}: `{leaked}` was painted as text:\n{since}"
        );
    }
    assert_eq!(
        count(since_raw, "\u{200B}".as_bytes()),
        0,
        "{what}: the zero-width space reached the terminal:\n{since}"
    );
    assert!(
        squeezed(&since).contains(plain),
        "{what}: the words were not drawn plain and in order:\n{since}"
    );
}

/// A pasted User line carrying a colour SGR, a zero-width space and an
/// OSC 8 hyperlink is drawn as its plain words once submitted: no `[31m`
/// or `]8;;` painted as text, the zero-width space never reaches the
/// terminal, and the words sit in order. The dry-run echo of the same
/// line is drawn plain as well, and the UI leaves clean.
#[test]
#[serial]
fn a_pasted_user_line_with_escapes_and_a_zero_width_space_is_drawn_as_plain_words() {
    let dir = fresh_config_dir("scrub-user-line");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend_from_slice(ESCAPED_WORDS);
    paste.extend_from_slice(b"\x1b[201~");
    pty.send(&paste).unwrap();
    pty.wait_for("end-q3u", PTY_DEADLINE)
        .expect("the pasted text never reached the composer");
    pty.read_for(Duration::from_millis(500));
    let submitted_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    wait_for_squeezed(&mut pty, "Working", "the turn");
    // The dry-run reply echoes the line; let it land after the working
    // row so both the User cell and the reply are on screen.
    pty.read_for(Duration::from_millis(2500));
    assert_escaped_words_drawn_plain(&pty, submitted_at, "seeredzwlinkend-q3u", "the user line");
    assert!(
        count(&pty.raw()[submitted_at..], b"end-q3u") >= 2,
        "the line was not drawn as both the User cell and the dry-run echo:\n{}",
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]))
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A command line and the error notice that echoes it (`.file` on a path
/// that does not exist names the path) are drawn as plain words when the
/// path carries a colour SGR, a zero-width space and an OSC 8 hyperlink:
/// nothing of the sequences painted as text in the `> .file ...` header
/// or in the error, the format character never sent, the UI keeps taking
/// keys and leaves clean. The header shows the typed line; the error
/// names the path the `.file` argument splitter hands the loader, which
/// drops a lone backslash (REPL semantics), so the hyperlink's `ESC \`
/// terminators reach it as bare escapes: each cuts its string short and
/// takes the character it introduces, as on a terminal.
#[test]
#[serial]
fn an_error_notice_echoing_escapes_and_a_zero_width_space_is_drawn_as_plain_words() {
    let dir = fresh_config_dir("scrub-error-notice");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let mut paste = b"\x1b[200~.file /nonexistent-w9/".to_vec();
    paste.extend_from_slice(ESCAPED_PATH);
    paste.extend_from_slice(b"\x1b[201~");
    pty.send(&paste).unwrap();
    pty.wait_for("end-q3u", PTY_DEADLINE)
        .expect("the pasted text never reached the composer");
    pty.read_for(Duration::from_millis(500));
    let submitted_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    pty.wait_for_after("Not found", submitted_at, PTY_DEADLINE)
        .expect("no error notice for the missing path");
    pty.read_for(Duration::from_millis(800));
    let since_raw = &pty.raw()[submitted_at..];
    let since = strip_ansi(&String::from_utf8_lossy(since_raw));
    // The error names the path after `Not found '`; the command header
    // is everything painted before the error. Each is judged on its own
    // so a failure names the cell.
    let error_at = since.find("Not found '").unwrap();
    let (header, error) = since.split_at(error_at);
    let header_squeezed = squeezed(header);
    let error_squeezed = squeezed(error);
    let mut failures = Vec::new();
    for (what, text, plain) in [
        (
            "the command header",
            &header_squeezed,
            "/nonexistent-w9/see-red-zw-link-end-q3u",
        ),
        (
            "the error notice",
            &error_squeezed,
            "/nonexistent-w9/see-red-zw-inknd-q3u",
        ),
    ] {
        for leaked in ["[31m", "[0m", "]8;;", "x.invalid"] {
            if text.contains(leaked) {
                failures.push(format!("{what}: `{leaked}` was painted as text"));
            }
        }
        if text.contains('\u{200B}') {
            failures.push(format!("{what}: the zero-width space was painted"));
        }
        if !text.contains(plain) {
            failures.push(format!(
                "{what}: the path was not drawn plain and whole (got {text:?})"
            ));
        }
    }
    assert_eq!(
        count(since_raw, "\u{200B}".as_bytes()),
        0,
        "the zero-width space reached the terminal:\n{since}"
    );
    let error_raw_at = find(since_raw, b"Not found", 0).unwrap();
    let error_raw = String::from_utf8_lossy(
        &since_raw[error_raw_at..(error_raw_at + 240).min(since_raw.len())],
    );
    assert!(
        failures.is_empty(),
        "{}\n--- header ---\n{header}\n--- error ---\n{error}\n--- error, raw bytes ---\n{:?}",
        failures.join("\n"),
        error_raw
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the error notice");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The REPL's confirmation line once a session was written by name.
const SAVED_LINE: &[u8] = b"Saved the session to";

/// `.exit session` on a dirty temp session, answered `y` and named: the
/// exit path's own output (the REPL's `Saved the session to '<path>'`
/// confirmation) lands in the transcript inside the alternate screen, on
/// a row below the `.exit session` command, the footer goes back to "no
/// session", the UI keeps taking keys, and nothing of the line leaks to
/// the normal screen after Ctrl+D.
#[test]
#[serial]
fn dot_exit_session_saving_on_y_prints_where_it_saved_in_the_transcript() {
    let dir = fresh_config_dir("exit-session-saved-line");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    open_and_dirty_a_session(&mut pty);
    let command_at = pty.raw().len();
    pty.send(b".exit session\r").unwrap();
    pty.wait_for_after("Save session?", command_at, PTY_DEADLINE)
        .unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    pty.wait_for("ession name", PTY_DEADLINE).unwrap();
    let named_at = pty.raw().len();
    answer_field_modal(&mut pty, "saved-w9-session");
    // The painter skips cells that were already blank, so the words of a
    // line can reach the stream without the spaces between them.
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let since = loop {
        let text = strip_ansi(&String::from_utf8_lossy(&pty.raw()[named_at..]));
        if squeezed(&text).contains("Savedthesessionto") {
            break text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the save confirmation never reached the transcript:\n{text}"
        );
        pty.read_for(Duration::from_millis(200));
    };
    pty.wait_for_after("no session", named_at, PTY_DEADLINE)
        .expect("the footer did not go back to `no session` after the session exit");
    assert!(
        find(pty.raw(), LEAVE_ALT, 0).is_none(),
        "the session exit left the alternate screen:\n{since}"
    );
    // The order is read off one full repaint: the stream before it holds
    // cell diffs from several frames (the transcript scrolls as the footer
    // flips), and a needle's nearest cursor move may belong to a frame
    // other than the one that painted it.
    let repaint_at = pty.raw().len();
    pty.resize(24, 81).unwrap();
    pty.read_for(Duration::from_millis(600));
    let command_rows = painted_at(pty.raw(), b".exit session", repaint_at);
    let saved_rows = painted_at(pty.raw(), b"Saved", repaint_at);
    let (Some(&(command_row, _)), Some(&(saved_row, _))) =
        (command_rows.first(), saved_rows.last())
    else {
        panic!("the command or its confirmation was not positioned on screen:\n{since}");
    };
    assert!(
        saved_row > command_row,
        "the confirmation was drawn above its command ({command_row} / {saved_row}):\n{since}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", named_at, PTY_DEADLINE)
        .expect("keys were not handled after the session exit");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let left_at = rfind(pty.raw(), LEAVE_ALT).expect("no leave sequence");
    assert!(
        find(pty.raw(), SAVED_LINE, left_at).is_none(),
        "the confirmation reached the normal screen after the UI left:\n{transcript}"
    );
    assert!(
        walk(&dir)
            .into_iter()
            .any(|p| p.file_name().is_some_and(|n| n == "saved-w9-session.yaml")),
        "no saved-w9-session.yaml was written under {dir:?}:\n{transcript}"
    );
}

/// `.exit` on a dirty temp session, answered `y` and named: the exit
/// work's own output (the same `Saved the session to '<path>'` line) is
/// not lost with the alternate screen; it is carried to the normal screen
/// after the UI left, exactly once, before the shell gets the terminal
/// back (above the wrapper's report), with the tty cooked and the file
/// written.
#[test]
#[serial]
fn dot_exit_saving_the_session_on_y_carries_the_saved_line_to_the_normal_screen() {
    let dir = fresh_config_dir("exit-saved-line");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    open_and_dirty_a_session(&mut pty);
    let exit_at = pty.raw().len();
    pty.send(b".exit\r").unwrap();
    pty.wait_for_after("Save session?", exit_at, PTY_DEADLINE)
        .expect(".exit on a dirty session did not ask to save it");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    pty.wait_for("ession name", PTY_DEADLINE).unwrap();
    let named_at = pty.raw().len();
    answer_field_modal(&mut pty, "saved-w9-exit");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let left_at = rfind(pty.raw(), LEAVE_ALT).expect("no leave sequence");
    let saved_at = find(pty.raw(), SAVED_LINE, named_at)
        .unwrap_or_else(|| panic!("the save confirmation was lost with the screen:\n{transcript}"));
    assert!(
        saved_at > left_at,
        "the confirmation was drawn inside the alternate screen only, not carried out:\n{transcript}"
    );
    let report_at = find(pty.raw(), b"exit=", left_at).expect("no exit= line");
    assert!(
        saved_at < report_at,
        "the confirmation came after the shell took the terminal back:\n{transcript}"
    );
    assert_eq!(
        count(&pty.raw()[named_at..], SAVED_LINE),
        1,
        "the confirmation was printed more than once:\n{transcript}"
    );
    let after_leave = strip_ansi(&String::from_utf8_lossy(&pty.raw()[left_at..]));
    assert!(
        after_leave.contains("saved-w9-exit.yaml"),
        "the confirmation does not name the written file:\n{after_leave}"
    );
    assert!(
        walk(&dir)
            .into_iter()
            .any(|p| p.file_name().is_some_and(|n| n == "saved-w9-exit.yaml")),
        "no saved-w9-exit.yaml was written under {dir:?}:\n{transcript}"
    );
}

// ---------------------------------------------------------------------------
// The remaining rerouted menus, each driven as a consumer would from a
// clean config dir.
// ---------------------------------------------------------------------------

/// `.vault update <name>` under the UI: the question is the Password
/// modal (`Enter the secret value:`), the replacement typed into it is
/// never painted, the vault reports the update, and `.vault get` prints
/// the new value and no longer the old one. The secret is seeded through
/// the same modal by `.vault add`.
#[test]
#[serial]
fn dot_vault_update_asks_in_the_password_modal_and_the_new_value_replaces_the_old() {
    let dir = fresh_config_dir("vault-update-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    secure_vault_password_file(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let add_at = pty.raw().len();
    pty.send(b".vault add vault-secret-u5v\r").unwrap();
    wait_for_the_modal(&mut pty, add_at, ".vault add", &["Enterthesecretvalue:"]);
    answer_field_modal(&mut pty, "old-value-u5v");
    wait_for_squeezed(
        &mut pty,
        "Secret 'vault-secret-u5v' added to the vault",
        "the seeded secret's confirmation",
    );
    pty.read_for(Duration::from_millis(300));

    let update_at = pty.raw().len();
    pty.send(b".vault update vault-secret-u5v\r").unwrap();
    wait_for_the_modal(
        &mut pty,
        update_at,
        ".vault update",
        &["Enterthesecretvalue:"],
    );
    answer_field_modal(&mut pty, "new-value-u5v");
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let updated = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
        if squeezed(&since).contains("Secret'vault-secret-u5v'updatedinthevault") {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the answered Password modal did not update the secret:\n{since}"
        );
    };
    assert!(
        !updated.contains("new-value-u5v"),
        "the replacement typed into the Password modal was painted on the screen:\n{updated}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the Password modal released the terminal:\n{updated}"
    );

    let get_at = pty.raw().len();
    pty.send(b".vault get vault-secret-u5v\r").unwrap();
    pty.wait_for_after("new-value-u5v", get_at, PTY_DEADLINE)
        .expect("`.vault get` did not print the value the update modal collected");
    pty.read_for(Duration::from_millis(300));
    let got = strip_ansi(&String::from_utf8_lossy(&pty.raw()[get_at..]));
    assert!(
        !got.contains("old-value-u5v"),
        "`.vault get` still prints the value from before the update:\n{got}"
    );

    assert_ui_alive_then_quit(&mut pty, "the vault update");
}

/// `.rag attach <name>` under the UI: the first question is the driver
/// Select modal, a closed select (only `qdrant`, no `Other (custom)`
/// row); Enter on it moves the wizard on to the host Input modal (the
/// answer flowed back); Esc there leaves no rag or kit behind and the
/// terminal was never released.
#[test]
#[serial]
fn dot_rag_attach_asks_for_the_driver_in_a_closed_select_modal_then_the_host_input() {
    let dir = fresh_config_dir("rag-attach-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".rag attach attach-rag-u5v\r").unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        before,
        ".rag attach",
        &["Selectdriver:", "qdrant"],
    );
    assert!(
        !squeezed(&modal).contains("Other(custom)"),
        "the driver select is closed but the modal offers a custom row:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    let frame = repainted_frame(&mut pty, 50, 120);
    assert!(
        squeezed(&frame).contains("Host(e.g.qdrant.company.com:6333):")
            && squeezed(&frame).contains("Esc=cancel"),
        "Enter on the driver select did not move the attach wizard on to the host Input modal:\n{frame}"
    );
    assert!(
        !squeezed(&frame).contains("Selectdriver:"),
        "the driver select is still up after Enter:\n{frame}"
    );

    esc_cancels_the_modal(&mut pty, ".rag attach attach-rag-u5v");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["u5v", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled attach still wrote {written:?}:\n{since}"
        );
    }
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "an attach question released the terminal:\n{since}"
    );

    assert_ui_alive_then_quit(&mut pty, "the cancelled attach");
}

/// `.install <category>` under the UI: the overwrite warning and its
/// `Continue?` question are the Confirm modal (the warning body is the
/// modal's help line, not a raw-tty `[y/N]` prompt); `n` aborts with
/// `No files were changed.` and the category directory is untouched;
/// asked again, `y` reinstalls and reports it.
#[test]
#[serial]
fn dot_install_of_a_builtin_category_asks_continue_in_the_confirm_modal() {
    let dir = fresh_config_dir("install-builtins-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let macros_dir = dir.join("macros");
    let snapshot = |dir: &Path| -> Vec<(PathBuf, u64, SystemTime)> {
        let mut rows: Vec<_> = walk(dir)
            .into_iter()
            .map(|path| {
                let meta = fs::metadata(&path).unwrap();
                (path, meta.len(), meta.modified().unwrap())
            })
            .collect();
        rows.sort();
        rows
    };
    let macros_before = snapshot(&macros_dir);

    let before = pty.raw().len();
    pty.send(b".install macros\r").unwrap();
    wait_for_the_modal(&mut pty, before, ".install macros", &["Continue?", "y=yes"]);
    // The warning body is long enough to overlap the banner it is painted
    // over, so the diff repaint skips the cells that already match; the
    // help line is read off a forced full repaint instead.
    let modal = repainted_frame(&mut pty, 58, 140);
    assert!(
        squeezed(&modal).contains("Reinstallingbundledmacrosoverwrites"),
        "the overwrite warning is not the modal's help line:\n{modal}"
    );
    assert!(
        !squeezed(&modal).contains("[y/N]"),
        "`Continue?` was asked on the raw tty, not in the modal:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    wait_for_squeezed(
        &mut pty,
        "Aborted. No files were changed.",
        "the abort after `n` on the modal",
    );
    pty.read_for(Duration::from_millis(500));
    assert_eq!(
        snapshot(&macros_dir),
        macros_before,
        "`n` on the overwrite modal still touched the macros dir"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the question released the terminal"
    );

    let again = pty.raw().len();
    pty.send(b".install macros\r").unwrap();
    wait_for_the_modal(&mut pty, again, ".install macros", &["Continue?", "y=yes"]);
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_squeezed(
        &mut pty,
        "Reinstalled bundled macros",
        "the reinstall report after `y` on the modal",
    );
    pty.read_for(Duration::from_millis(500));
    assert!(
        macros_dir.is_dir() && !walk(&macros_dir).is_empty(),
        "`y` on the overwrite modal reported a reinstall but left no macros under {}",
        macros_dir.display()
    );

    assert_ui_alive_then_quit(&mut pty, "the reinstall");
}

/// A local bundle with no secrets (`write_bundle_repo` with none), so its
/// install asks nothing and only the uninstall menus are exercised.
fn install_bundle_without_questions(pty: &mut Pty, repo: &Path) {
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        assert!(
            !squeezed(&since).contains("Esc=cancel"),
            "`.install` of a bundle without secrets or conflicts asked a question:\n{since}"
        );
        if squeezed(&since).contains("Installed:2new") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`.install {}` did not report the installed files:\n{since}",
            repo.display()
        );
    }
    pty.read_for(Duration::from_millis(500));
}

/// `.uninstall <bundle>` under the UI: the question is the Confirm modal
/// whose help line names the bundle's footprint (`owns N file(s) and M
/// mcp.json server(s)`); `n` aborts with `nothing was changed` and every
/// owned file stays. Asked again, `y` proceeds; a file edited locally
/// after the install raises the `was modified locally after install`
/// Select modal (keep / delete / keep-all / delete-all, closed: no
/// custom row), `1` keeps it, the unmodified file is deleted, and the
/// bundle is reported partially uninstalled with its record kept.
#[test]
#[serial]
fn dot_uninstall_asks_in_the_confirm_modal_and_a_modified_file_in_the_select_modal() {
    let dir = fresh_config_dir("uninstall-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-u5v";
    let repo = write_bundle_repo(&dir, name, &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    install_bundle_without_questions(&mut pty, &repo);
    let role = dir.join("roles").join(format!("{name}-role.md"));
    let edited = dir.join("roles").join(format!("{name}-obsolete.md"));
    assert!(
        role.exists() && edited.exists(),
        "the bundle's roles were not installed under {}",
        dir.display()
    );
    let edited_text = "---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n";
    fs::write(&edited, edited_text).unwrap();

    // `n` on the Confirm modal: nothing changes.
    let question = squeezed(&format!("Uninstall bundle '{name}'?"));
    let first = pty.raw().len();
    pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
    let modal = wait_for_the_modal(&mut pty, first, ".uninstall", &[question.as_str(), "y=yes"]);
    assert!(
        squeezed(&modal).contains("owns2file(s)and1mcp.jsonserver(s)"),
        "the Confirm modal does not show the bundle's footprint as its help line:\n{modal}"
    );
    assert!(
        !squeezed(&modal).contains("(y/N)"),
        "`Uninstall bundle?` was asked on the raw tty, not in the modal:\n{modal}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    wait_for_squeezed(
        &mut pty,
        &format!("Uninstall of '{name}' aborted; nothing was changed."),
        "the abort after `n` on the uninstall modal",
    );
    pty.read_for(Duration::from_millis(300));
    assert!(
        role.exists() && edited.exists(),
        "`n` on the uninstall modal still removed a bundle file"
    );
    let records = walk(&dir)
        .into_iter()
        .filter(|p| {
            let text = p.to_string_lossy();
            !text.contains("/bundle-src/")
                && !text.contains("/roles/")
                && fs::read_to_string(p).is_ok_and(|body| body.contains(name))
        })
        .count();
    assert!(records > 0, "the bundle record vanished after `n`");

    // `y`, then `1` (keep) on the modified-file Select modal.
    let second = pty.raw().len();
    pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
    wait_for_the_modal(
        &mut pty,
        second,
        ".uninstall",
        &[question.as_str(), "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    let select = wait_for_the_modal(
        &mut pty,
        second,
        ".uninstall (modified file)",
        &[
            "wasmodifiedlocallyafterinstall",
            "keep",
            "delete",
            "keep-all",
            "delete-all",
        ],
    );
    assert!(
        !squeezed(&select).contains("Other(custom)"),
        "the modified-file select is closed but the modal offers a custom row:\n{select}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"1").unwrap();
    wait_for_squeezed(
        &mut pty,
        &format!("Bundle '{name}' partially uninstalled; its record keeps the remaining items"),
        "the uninstall report after keeping the modified file",
    );
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[second..]));
    assert!(
        squeezed(&since).contains("keptmodifiedfile"),
        "`1` on the Select modal was not taken as `keep`:\n{since}"
    );
    assert!(
        !role.exists(),
        "the unmodified role was not deleted by the uninstall:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&edited).ok().as_deref(),
        Some(edited_text),
        "the modified role was not kept as answered:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "an uninstall question released the terminal:\n{since}"
    );

    // The kept file is still owned, so the record stays and `.list
    // bundles` keeps naming the bundle. The name is looked for after the
    // table's `Bundles:` header so the match is a table row, not the
    // command's echo or a repaint of an earlier line naming the bundle.
    let list_at = pty.raw().len();
    pty.send(b".list bundles\r").unwrap();
    let header_at = pty
        .wait_for_raw(b"Bundles:", list_at, PTY_DEADLINE)
        .expect("`.list bundles` did not print its `Bundles:` header");
    pty.wait_for_after(name, header_at, PTY_DEADLINE)
        .unwrap_or_else(|e| {
            panic!(
                "`.list bundles` no longer names the partially uninstalled bundle whose record was kept: {e:?}"
            )
        });

    assert_ui_alive_then_quit(&mut pty, "the uninstall");
}

/// With no terminal, the no-tty silent paths keep their polarity and the
/// refusals keep theirs: `--install <local bundle>` with nothing to ask
/// proceeds and installs the files; `--uninstall <bundle>` without
/// `--yes` is refused (exit non-zero, the refusal names the
/// non-interactive run and `--yes`) with every owned file left in place
/// and the record kept; `--uninstall --yes` then removes them.
#[test]
#[serial]
fn a_tty_less_uninstall_without_yes_is_refused_and_keeps_the_bundle() {
    let dir = fresh_config_dir("ttyless-uninstall");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-u5v";
    let repo = write_bundle_repo(&dir, name, &[]);
    let repo_arg = repo.to_str().unwrap();

    let (ok, out) = run_ttyless(&dir, &["--install", repo_arg], "tty-less install");
    let role = dir.join("roles").join(format!("{name}-role.md"));
    assert!(
        ok && role.exists(),
        "a tty-less `--install` of a bundle with nothing to ask did not proceed silently:\n{out}"
    );

    let (ok, out) = run_ttyless(&dir, &["--uninstall", name], "tty-less uninstall");
    assert!(
        !ok,
        "a tty-less `--uninstall` without `--yes` exited 0:\n{out}"
    );
    let lower = out.to_lowercase();
    assert!(
        lower.contains("non-interactive")
            || lower.contains("not a terminal")
            || lower.contains("not a tty"),
        "the refusal does not say the run is non-interactive:\n{out}"
    );
    assert!(
        out.contains("--yes"),
        "the refusal does not name `--yes` as the remedy:\n{out}"
    );
    assert!(
        role.exists(),
        "the refused tty-less uninstall still removed a bundle file:\n{out}"
    );

    let (ok, out) = run_ttyless(
        &dir,
        &["--uninstall", name, "--yes"],
        "tty-less uninstall --yes",
    );
    assert!(
        ok && !role.exists(),
        "`--uninstall --yes` with no terminal did not remove the bundle's files:\n{out}"
    );
}

// ---------------------------------------------------------------------
// Wizards driven from a clean config dir: the rag wizard's Input modal
// against its plain-REPL twin, the closed Select's key handling, and
// typeahead on the `.update` confirm.
// ---------------------------------------------------------------------

/// Walks the rag wizard from the embedding Select modal to the
/// `Set chunk size:` Input modal with Enter (the highlighted choice or the
/// default) and returns the frame showing the chunk-size question. Every
/// frame is forced by a resize, so `rows` alternates between two heights
/// and the caller keeps using it for the frames that follow.
fn walk_to_the_chunk_size_modal(pty: &mut Pty, command: &str, rows: &mut u16) -> String {
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    let mut frame = next_wizard_frame(pty, rows);
    let mut steps = 0;
    while !squeezed(&frame).contains("Setchunksize:") {
        assert!(
            !squeezed(&frame).contains("Adddocuments:"),
            "`{command}` reached `Add documents:` without asking the chunk size:\n{frame}"
        );
        assert!(
            steps < 4,
            "`{command}` did not reach `Set chunk size:` within 4 answers:\n{frame}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        frame = next_wizard_frame(pty, rows);
        steps += 1;
    }
    frame
}

/// A repaint forced by flipping the height between 50 and 48 rows.
fn next_wizard_frame(pty: &mut Pty, rows: &mut u16) -> String {
    *rows = if *rows == 50 { 48 } else { 50 };
    repainted_frame(pty, *rows, 120)
}

/// Typing a value into the `Set chunk size:` Input modal must mean the
/// same as typing it at the plain REPL's `Set chunk size: (1000)`: the
/// answer is what was typed, not the default with the typed digits
/// appended. The wizard derives the chunk-overlay default from the
/// accepted chunk size (1/20th of it), so the next modal shows which value
/// was taken: `25` for 500, `50025` for 1000500.
#[test]
#[serial]
fn dot_rag_chunk_size_typed_into_the_input_modal_replaces_the_default_like_the_plain_repl() {
    let dir = fresh_config_dir("rag-input-default-typed");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".rag new-rag-q8d\r").unwrap();
    wait_for_the_modal(
        &mut pty,
        before,
        ".rag new-rag-q8d",
        &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
    );
    let mut rows = 48;
    let frame = walk_to_the_chunk_size_modal(&mut pty, ".rag new-rag-q8d", &mut rows);
    assert!(
        squeezed(&frame).contains("Setchunksize:1000"),
        "the `Set chunk size:` Input modal does not show its default 1000:\n{frame}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"500\r").unwrap();
    let frame = next_wizard_frame(&mut pty, &mut rows);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Setchunkoverlay:") && flat.contains("Esc=cancel"),
        "the wizard did not move on to `Set chunk overlay:` after the chunk size was typed:\n{frame}"
    );
    assert!(
        !flat.contains("Setchunkoverlay:50025"),
        "typing 500 into the Input modal appended to the default: the wizard took chunk size \
         1000500 (overlay default 50025), where the plain REPL takes 500:\n{frame}"
    );
    assert!(
        flat.contains("Setchunkoverlay:25"),
        "the chunk-overlay default is not 25, so the chunk size taken was not the typed 500:\n{frame}"
    );

    esc_cancels_the_modal(&mut pty, ".rag new-rag-q8d");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["q8d", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled wizard still wrote {written:?}:\n{since}"
        );
    }
    assert_ui_alive_then_quit(&mut pty, "the cancelled wizard");
}

/// The plain-REPL twin of the case above, the contract the modal is held
/// to: Enter on the Select echoes the chosen row in full (the model id
/// with its description columns, as the wizard renders it), inquire shows
/// the default as `(1000)`, `500` typed over it is the answer, and the
/// next question's default is `(25)`.
#[test]
#[serial]
fn dot_rag_chunk_size_typed_in_the_plain_repl_replaces_the_default() {
    let dir = fresh_config_dir("rag-input-default-typed-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".rag new-rag-r9e\r").unwrap();
    pty.wait_for_after("Select embedding model:", before, PTY_DEADLINE)
        .expect("`.rag <new>` in the plain REPL did not ask for the embedding model");
    pty.read_for(Duration::from_millis(500));
    pty.send(b"\r").unwrap();
    pty.wait_for_after(
        "Select embedding model: dryrun:dry-embed-a (max-tokens:-;max-batch:100;price:-)",
        before,
        PTY_DEADLINE,
    )
    .expect("Enter on the plain REPL's Select did not echo the chosen model's full row");
    let mut answered = 0;
    loop {
        let text = pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        if since.contains("Set chunk size: (1000)") {
            break;
        }
        assert!(
            answered < 4,
            "the plain REPL wizard did not reach `Set chunk size: (1000)` within 4 answers:\n{text}"
        );
        pty.send(b"\r").unwrap();
        answered += 1;
    }

    let typed_at = pty.raw().len();
    pty.send(b"500\r").unwrap();
    pty.wait_for_after("Set chunk overlay:", typed_at, PTY_DEADLINE)
        .expect("the plain REPL wizard did not move on to `Set chunk overlay:`");
    // The default is painted a moment after the label, so it is waited for
    // rather than read off the first frame that shows the question.
    let since = pty
        .wait_for_after("Set chunk overlay: (25)", typed_at, PTY_DEADLINE)
        .unwrap_or_else(|e| {
            panic!(
                "typing 500 at `Set chunk size: (1000)` did not make the chunk size 500 \
                 (the overlay default is 1/20th of it): {e:?}"
            )
        });
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{since}"
    );

    let esc_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after("dry-model)>", esc_at, PTY_DEADLINE)
        .expect("Esc on the inquire text prompt did not bring the prompt back");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["r9e", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled `.rag` still wrote {written:?}:\n{since}"
        );
    }
    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------
// Cancelling the obsolete-file question of a bundle update.
// ---------------------------------------------------------------------

const EDITED_OBSOLETE: &str = "---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n";
const KEPT_OBSOLETE_NOTE: &str = "Kept the obsolete files: the question was cancelled.";

/// Installs the secret-free bundle `name` from `repo`, edits its second
/// role locally, then drops that role from the source and commits, so
/// the next `.install <name>` finds an obsolete file to ask about.
/// Returns the edited file's path.
fn make_the_second_role_obsolete(dir: &Path, repo: &Path, name: &str) -> PathBuf {
    let obsolete = dir.join("roles").join(format!("{name}-obsolete.md"));
    assert!(
        obsolete.exists(),
        "the bundle's second role was not installed at {}",
        obsolete.display()
    );
    fs::write(&obsolete, EDITED_OBSOLETE).unwrap();
    fs::remove_file(repo.join("roles").join(format!("{name}-obsolete.md"))).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "drop a role"]);
    obsolete
}

/// `.install <bundle>` updating a bundle whose source dropped a role the
/// user edited: the obsolete file's fate is a Select modal; Esc on it
/// keeps the file, the update still prints its summary and one note,
/// `Kept the obsolete files: the question was cancelled.`, the terminal
/// is never released, and the UI keeps taking keys.
#[test]
#[serial]
fn esc_on_the_obsolete_file_select_modal_keeps_the_file_and_says_the_question_was_cancelled() {
    let dir = fresh_config_dir("obsolete-esc-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-o8e";
    let repo = write_bundle_repo(&dir, name, &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    install_bundle_without_questions(&mut pty, &repo);
    let obsolete = make_the_second_role_obsolete(&dir, &repo, name);

    let update_at = pty.raw().len();
    pty.send(format!(".install {name}\r").as_bytes()).unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        update_at,
        ".install <bundle> (obsolete file)",
        &["Obsolete file", &format!("{name}-obsolete.md"), "keep"],
    );
    assert!(
        !squeezed(&modal).contains("Other(custom)"),
        "the obsolete-file select is closed but the modal offers a custom row:\n{modal}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        KEPT_OBSOLETE_NOTE,
        "the note after Esc on the obsolete-file modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
    let flat = squeezed(&since);
    assert_eq!(
        flat.matches(&squeezed(KEPT_OBSOLETE_NOTE)).count(),
        1,
        "the kept-files note was printed more or less than once:\n{since}"
    );
    assert!(
        flat.contains("Installed:") || flat.contains("Updated") || flat.contains("unchanged"),
        "the cancelled question swallowed the update's summary:\n{since}"
    );
    let frame = repainted_frame(&mut pty, 58, 140);
    assert!(
        !squeezed(&frame).contains("Esc=cancel"),
        "the obsolete-file modal is still up after Esc:\n{frame}"
    );
    assert_eq!(
        fs::read_to_string(&obsolete).ok().as_deref(),
        Some(EDITED_OBSOLETE),
        "Esc on the obsolete-file modal did not keep the edited file:\n{since}"
    );
    assert!(
        dir.join("roles").join(format!("{name}-role.md")).exists(),
        "the update dropped the bundle's current role:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the obsolete-file question released the terminal:\n{since}"
    );

    assert_ui_alive_then_quit(&mut pty, "the cancelled obsolete-file question");
}

/// The same update in the plain REPL: the obsolete file's fate is an
/// inquire Select; Esc cancels it, the file is kept, the summary and
/// the one note line follow, and the REPL goes on taking commands.
#[test]
#[serial]
fn esc_on_the_obsolete_file_select_in_the_plain_repl_keeps_the_file_and_says_so() {
    let dir = fresh_config_dir("obsolete-esc-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-o8r";
    let repo = write_bundle_repo(&dir, name, &[]);
    let (ok, out) = run_ttyless(
        &dir,
        &["--install", repo.to_str().unwrap()],
        "tty-less install of the bundle to update",
    );
    assert!(ok, "{out}");
    let obsolete = make_the_second_role_obsolete(&dir, &repo, name);

    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");
    pty.wait_for(")> ", PTY_DEADLINE).unwrap();
    let update_at = pty.raw().len();
    pty.send(format!(".install {name}\r").as_bytes()).unwrap();
    pty.wait_for_after("Obsolete file", update_at, PTY_DEADLINE)
        .expect("the REPL update did not ask about the obsolete file");
    pty.read_for(Duration::from_millis(500));
    let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
    assert!(
        asked.contains("keep") && !asked.contains("Other (custom)"),
        "the REPL question is not the closed keep/delete select:\n{asked}"
    );
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after(KEPT_OBSOLETE_NOTE, update_at, PTY_DEADLINE)
        .expect("the REPL did not print the kept-files note after Esc");
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
    assert_eq!(
        since.matches(KEPT_OBSOLETE_NOTE).count(),
        1,
        "the kept-files note was printed more or less than once:\n{since}"
    );
    assert!(
        since.contains("Installed:") || since.contains("Updated") || since.contains("unchanged"),
        "the cancelled question swallowed the update's summary:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&obsolete).ok().as_deref(),
        Some(EDITED_OBSOLETE),
        "Esc on the REPL select did not keep the edited file:\n{since}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("`.info` after the cancelled question never rendered");
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A closed Select modal takes only its rows: a digit past the last row
/// is ignored and the question stays up. A non-integer typed into the
/// `Set chunk size:` Input modal is re-asked in the modal with the
/// reason (`Must be an integer.`), the way the plain REPL re-asks; Esc
/// then leaves nothing on disk.
#[test]
#[serial]
fn dot_rag_ignores_a_digit_past_the_closed_select_and_reasks_a_non_integer_chunk_size_in_the_modal()
{
    let dir = fresh_config_dir("rag-closed-select-and-reask");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".rag new-rag-t3m\r").unwrap();
    wait_for_the_modal(
        &mut pty,
        before,
        ".rag new-rag-t3m",
        &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"9").unwrap();
    let mut rows = 48;
    let frame = next_wizard_frame(&mut pty, &mut rows);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Selectembeddingmodel:") && flat.contains("Esc=cancel"),
        "`9` on a two-row closed Select closed the modal instead of being ignored:\n{frame}"
    );
    assert!(
        !flat.contains("Setchunksize:"),
        "`9` on a two-row closed Select answered it and the wizard moved on:\n{frame}"
    );

    walk_to_the_chunk_size_modal(&mut pty, ".rag new-rag-t3m", &mut rows);
    pty.read_for(HOTKEY_GAP);
    pty.send(b"abc\r").unwrap();
    let frame = next_wizard_frame(&mut pty, &mut rows);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Mustbeaninteger.")
            && flat.contains("Setchunksize:")
            && flat.contains("Esc=cancel"),
        "a non-integer chunk size was not re-asked in the modal with its reason:\n{frame}"
    );
    assert!(
        !flat.contains("Setchunkoverlay:"),
        "a non-integer chunk size was accepted and the wizard moved on:\n{frame}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a wizard question released the terminal:\n{frame}"
    );

    esc_cancels_the_modal(&mut pty, ".rag new-rag-t3m");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["t3m", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled wizard still wrote {written:?}:\n{since}"
        );
    }
    assert_ui_alive_then_quit(&mut pty, "the cancelled wizard");
}

/// `.update` followed at once by `y` in the same burst of keys (a user
/// typing ahead of the confirm): the `y` must not accept `Update anyway?`,
/// so the modal is still up well past the idle gate, the update has not
/// started, and `n` then cancels it with the binary untouched. Where the
/// `y` lands depends on timing (dropped by the modal's idle gate, or typed
/// into the composer before the modal opened); either way it is not an answer.
#[test]
#[serial]
fn dot_update_y_typed_ahead_with_the_command_does_not_accept_the_confirm_modal() {
    let dir = fresh_config_dir("update-typeahead-y");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let binary = cargo_installed_binary(&dir);
    let binary_before = fs::metadata(&binary).unwrap();
    let mut pty = spawn_binary_under(&binary, &dir, &[], WAIT_WRAPPER, "/dev/tty", "");

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b".update\ry").unwrap();
    wait_for_the_modal(&mut pty, before, ".update", &["Updateanyway?", "y=yes"]);

    pty.read_for(Duration::from_millis(2500));
    let frame = repainted_frame(&mut pty, 58, 140);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Updateanyway?") && flat.contains("Esc=cancel"),
        "the `y` typed ahead with `.update` accepted the confirm modal:\n{frame}"
    );
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert!(
        !since.contains("Checking target-arch") && !since.contains("updating anyway"),
        "the update started on the typed-ahead `y`:\n{since}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    wait_for_squeezed(
        &mut pty,
        "Update cancelled.",
        "the cancellation after `n` on the modal",
    );
    let binary_after = fs::metadata(&binary).unwrap();
    assert!(
        binary_after.len() == binary_before.len()
            && binary_after.modified().unwrap() == binary_before.modified().unwrap(),
        "the cancelled update touched the binary at {}",
        binary.display()
    );
    // A `y` that reached the composer before the modal opened would ride
    // along with the next command; one Backspace clears it and is a no-op
    // on an empty composer.
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x7f").unwrap();
    assert_ui_alive_then_quit(&mut pty, "the cancelled update");
}

// ---------------------------------------------------------------------
// The Input modal's empty-field default, the Password modal's empty
// re-ask, and `--update` with only stdout piped.
// ---------------------------------------------------------------------

/// `Set chunk size:` under the UI shows its default `1000` as a dim
/// placeholder; Enter on the untouched field answers the default (the
/// next question's default is 1/20th of the chunk size: `50`), and a
/// field typed into and then backspaced back to empty answers the
/// default too, exactly as the plain REPL's `(1000)` does. Esc on the
/// overlay question leaves nothing on disk both times.
#[test]
#[serial]
fn dot_rag_enter_on_the_empty_chunk_size_modal_takes_the_dim_placeholder_default() {
    let dir = fresh_config_dir("rag-input-empty-default");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let mut rows = 48;
    for (name, keys, how) in [
        ("new-rag-e5d", &b"\r"[..], "Enter on the untouched field"),
        ("new-rag-e5e", &b"7\x7f\r"[..], "`7`, Backspace, Enter"),
    ] {
        let command = format!(".rag {name}");
        let before = pty.raw().len();
        pty.send(format!("{command}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            &mut pty,
            before,
            &command,
            &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
        );
        let frame = walk_to_the_chunk_size_modal(&mut pty, &command, &mut rows);
        assert!(
            squeezed(&frame).contains("Setchunksize:1000"),
            "`{command}`: the `Set chunk size:` Input modal does not show its default 1000:\n{frame}"
        );
        let painted_at = pty.raw().len();
        let frame = next_wizard_frame(&mut pty, &mut rows);
        assert!(
            painted_dim(pty.raw(), painted_at, b"1000"),
            "`{command}`: the default 1000 is not painted dim as a placeholder:\n{frame}\n--- raw ---\n{}",
            String::from_utf8_lossy(&pty.raw()[painted_at..]).escape_debug()
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(keys).unwrap();
        let frame = next_wizard_frame(&mut pty, &mut rows);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Setchunkoverlay:") && flat.contains("Esc=cancel"),
            "`{command}`: {how} did not move the wizard on to `Set chunk overlay:`:\n{frame}"
        );
        assert!(
            flat.contains("Setchunkoverlay:50"),
            "`{command}`: {how} did not take chunk size 1000 (overlay default 50):\n{frame}"
        );
        assert!(
            !flat.contains("Setchunkoverlay:500") && !flat.contains("Setchunkoverlay:0"),
            "`{command}`: {how} took a chunk size other than the default 1000:\n{frame}"
        );

        esc_cancels_the_modal(&mut pty, &command);
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in [name, "/rags/"] {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "`{command}`: the cancelled wizard still wrote {written:?}:\n{since}"
            );
        }
    }
    assert_ui_alive_then_quit(&mut pty, "the cancelled wizards");
}

/// `.vault add <name>` under the UI with Enter on an empty Password
/// modal: nothing is stored, the question is re-asked in the modal with
/// its reason spelled out as a sentence (the plain REPL re-asks with
/// `A response is required.`), the terminal is never released, and Esc
/// then leaves no vault file behind.
#[test]
#[serial]
fn dot_vault_add_with_an_empty_password_modal_is_reasked_and_esc_stores_nothing() {
    let dir = fresh_config_dir("vault-add-empty-reask");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    secure_vault_password_file(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".vault add vault-secret-e6f\r").unwrap();
    wait_for_the_modal(&mut pty, before, ".vault add", &["Enterthesecretvalue:"]);

    pty.read_for(HOTKEY_GAP);
    let entered_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    let frame = repainted_frame(&mut pty, 50, 120);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Enterthesecretvalue:") && flat.contains("Esc=cancel"),
        "Enter on the empty Password modal closed it instead of re-asking:\n{frame}"
    );
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[entered_at..]));
    assert!(
        !squeezed(&since).contains("addedtothevault"),
        "an empty secret value was stored:\n{since}"
    );
    assert!(
        flat.contains("Aresponseisrequired."),
        "the re-asked Password modal does not give the plain REPL's reason `A response is \
         required.` as a sentence:\n{frame}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the re-asked Password modal released the terminal:\n{frame}"
    );

    esc_cancels_the_modal(&mut pty, ".vault add");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["e6f", "vault.yml"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled `.vault add` still wrote {written:?}:\n{since}"
        );
    }
    assert_ui_alive_then_quit(&mut pty, "the cancelled vault add");
}

// ---------------------------------------------------------------------------
// Seen from the real binary: a tool's list question keeps its arrow help
// and its custom row, the asset-overwrite warning is an Enter-answered
// confirm, a blank required agent variable is re-asked with its reason,
// and a tty-less `.rag` with no name refuses before it touches the temp
// rag.
// ---------------------------------------------------------------------------

/// A tool that asks the human to pick one of three entries through the
/// prompt helpers' `list` and records the index (or typed text) it got
/// back, and next to it the helper's exit status: 0 for an index, 2 for
/// typed text.
const RECORDING_LIST_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user to pick one entry and record the index.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out v rc=0\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   v=\"$(list \"Pick one entry q7l-q\" alpha-q7l beta-q7l gamma-q7l)\" || rc=$?\n\
    \x20   printf '%s' \"$rc\" > \"$out-rc\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture picked: ${v:-cancelled} q7l-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// The scripted model: one call of the list tool, then a closing line.
fn list_tool_turn(dir: &Path) -> (FakeLlm, PathBuf) {
    let out = dir.join("list-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_list_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done q7l".to_string()),
    ]);
    write_fake_llm_config(dir, &fake);
    write_tool(dir, "fixture_list_rec.sh", RECORDING_LIST_TOOL);
    (fake, out)
}

/// The `list` exit status the recording tool saw for the answer at `out`.
fn list_exit_status(out: &Path) -> String {
    fs::read_to_string(format!("{}-rc", out.display())).unwrap()
}

/// A tool's `list` question in the plain REPL is the inquire select it
/// always was: the three entries and the `Other (custom)` row are listed
/// under the question with the `to move, enter to select` help, a typed
/// letter does not filter the list (Down then Enter still lands on the
/// second entry, not on the only one starting with that letter), the
/// index flows back to the tool and the turn completes.
#[test]
#[serial]
fn a_tools_list_question_in_the_plain_repl_is_an_open_select_keeping_the_arrow_help() {
    let dir = fresh_config_dir("tool-list-repl");
    let _guard = TempDirGuard(dir.clone());
    let (fake, out) = list_tool_turn(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
    pty.send(b"call the list tool\r").unwrap();
    pty.wait_for_after("q7l-q", asked_at, PTY_DEADLINE)
        .expect("the tool's list question never reached the terminal");
    pty.wait_for_after("gamma-q7l", asked_at, PTY_DEADLINE)
        .expect("the last entry of the list never reached the terminal");
    pty.read_for(Duration::from_millis(500));
    let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
    for entry in ["alpha-q7l", "beta-q7l", "gamma-q7l"] {
        assert!(
            asked.contains(entry),
            "the list question does not offer `{entry}`:\n{asked}"
        );
    }
    assert!(
        asked.contains("Other (custom)"),
        "a tool's list question does not offer the free-text row:\n{asked}"
    );
    assert!(
        asked.contains("to move, enter to select"),
        "the tool's list question lost its arrow help:\n{asked}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{asked}"
    );

    // `g` would narrow a filtering select to `gamma-q7l`; here it is
    // ignored, so Down then Enter picks the second entry.
    pty.send(b"g").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "Down then Enter on the list did not reach the tool as the second entry (a typed \
         letter must not filter the list):\n{transcript}"
    );
    assert_eq!(
        list_exit_status(&out),
        "0",
        "the chosen entry did not reach the tool with the index status:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The same tool's `list` question under the UI is the Select modal with
/// the tool's entries and the `Other (custom)` row, the terminal is never
/// released, Down then Enter hands the tool the second entry's index, and
/// on a second call text typed on the custom row flows back to the tool
/// verbatim.
#[test]
#[serial]
fn a_tools_list_question_under_the_ui_takes_typed_text_from_the_custom_row() {
    let dir = fresh_config_dir("tool-list-modal");
    let _guard = TempDirGuard(dir.clone());
    let picked = dir.join("list-answer-picked");
    let out = dir.join("list-answer");
    let call = |out: &Path| Reply::ToolCall {
        name: "fixture_list_rec".to_string(),
        arguments: format!("{{\"out\": \"{}\"}}", out.display()),
    };
    let fake = FakeLlm::start(vec![
        call(&picked),
        Reply::Text("fixture picked first q7l".to_string()),
        call(&out),
        Reply::Text("fixture done q7l".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_list_rec.sh", RECORDING_LIST_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    let ask = |pty: &mut Pty, line: &[u8]| {
        let asked_at = pty.raw().len();
        pty.send(line).unwrap();
        wait_for_the_modal(
            pty,
            asked_at,
            "the list tool",
            &[
                "q7l-q",
                "alpha-q7l",
                "beta-q7l",
                "gamma-q7l",
                "Other (custom)",
                "Up/Down=move",
            ],
        );
        pty.read_for(HOTKEY_GAP);
    };

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    ask(&mut pty, b"call the list tool\r");
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &picked, "the tool's recorded index");
    let transcript = wait_for_squeezed(
        &mut pty,
        "fixture picked first q7l",
        "the reply after the first call",
    );
    assert_eq!(
        fs::read_to_string(&picked).unwrap(),
        "1",
        "Down then Enter in the modal did not reach the tool as the second entry:\n{transcript}"
    );
    assert_eq!(
        list_exit_status(&picked),
        "0",
        "the chosen entry did not reach the tool with the index status:\n{transcript}"
    );

    pty.read_for(Duration::from_millis(300));
    ask(&mut pty, b"call the list tool again\r");
    for _ in 0..3 {
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
    }
    pty.send(b"\r").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"typed-by-hand-x7q").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "typed-by-hand-x7q",
        "the text typed on the custom row did not reach the tool verbatim:\n{transcript}"
    );
    assert_eq!(
        list_exit_status(&out),
        "2",
        "typed text did not reach the tool with the custom-answer status:\n{transcript}"
    );
    assert_eq!(fake.served(), 4, "{transcript}");
    assert!(
        fake.bodies()[3].contains("fixture picked: typed-by-hand-x7q q7l-r"),
        "the tool's output handed to the model does not carry the typed text:\n{:?}",
        fake.bodies()
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.install <category>` in the plain REPL: the overwrite warning is
/// followed by the inquire `Continue? (y/N)` confirm, which Enter
/// answers - a bare `y` on its own does nothing until Enter follows
/// (the single-key `[y/N]` read is gone), and a bare Enter takes the
/// `No` default: `Aborted. No files were changed.` and the category
/// directory is untouched.
#[test]
#[serial]
fn dot_install_of_a_builtin_category_in_the_plain_repl_confirms_on_enter_not_on_a_bare_key() {
    let dir = fresh_config_dir("install-builtins-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let macros_dir = dir.join("macros");
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    // The built-in macros are seeded on startup; the question must not
    // touch them from here on.
    let files_before = walk(&macros_dir);
    let before = pty.raw().len();
    pty.send(b".install macros\r").unwrap();
    pty.wait_for_after("Continue?", before, PTY_DEADLINE)
        .expect("`.install macros` in the plain REPL did not ask `Continue?`");
    pty.read_for(Duration::from_millis(500));
    let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    // The warning body is a plain `println!` ahead of the question; the
    // terminal may wrap it, so it is looked for with whitespace removed.
    let flat_asked = squeezed(&asked);
    let warning_at = flat_asked
        .find("WARNING:Reinstallingbundledmacrosoverwrites")
        .unwrap_or_else(|| panic!("the overwrite warning was not printed:\n{asked}"));
    assert!(
        warning_at < flat_asked.find("Continue?").unwrap(),
        "the overwrite warning came after the `Continue?` question:\n{asked}"
    );
    assert!(
        asked.contains("(y/N)"),
        "the `Continue?` question is not the inquire confirm with its `(y/N)` legend:\n{asked}"
    );

    // A bare `y` is typed, not acted on.
    pty.send(b"y").unwrap();
    let after_y = pty.read_for(Duration::from_millis(1500));
    let flat = squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..])));
    assert!(
        !flat.contains("Aborted") && !flat.contains("Installed") && !flat.contains("Reinstalled"),
        "a bare `y` without Enter already settled the `Continue?` question:\n{after_y}"
    );
    assert_eq!(
        walk(&macros_dir),
        files_before,
        "a bare `y` without Enter already changed the macros directory"
    );

    // Backspace takes the `y` back; Enter then answers with the default.
    pty.send(b"\x7f").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    let settled = wait_for_squeezed(
        &mut pty,
        "No files were changed",
        "the plain REPL's answer to Enter on `Continue?`",
    );
    assert!(
        squeezed(&settled).contains("Continue?No"),
        "Enter on the confirm did not take the `No` default:\n{settled}"
    );
    assert_eq!(
        walk(&macros_dir),
        files_before,
        "the declined reinstall changed the macros directory"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{settled}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------
// Esc on a vault-secret modal defers the rest, and a delete-all answer
// about owned files never reaches the mcp.json servers, while keep-all
// does.
// ---------------------------------------------------------------------

/// Rewrites the merged `mcp.json` so the bundle's server entry differs
/// from the one the install recorded (its `k7s` argument gains a
/// suffix); the uninstall then sees a locally modified server.
fn edit_the_bundle_server_in_mcp_json(dir: &Path) -> String {
    let path = dir.join("mcp.json");
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"k7s\""),
        "the merged mcp.json does not carry the bundle server's `k7s` argument:\n{text}"
    );
    let edited = text.replace("\"k7s\"", "\"k7s-edited-locally\"");
    fs::write(&path, &edited).unwrap();
    edited
}

/// `.install <local bundle>` whose mcp.json references three secrets:
/// Esc on the first `Add {{ X }} to the vault now?` modal defers THAT
/// secret and every later one (no modal for B or C), the notice lists
/// all three, the bundle is installed and recorded with its server
/// merged, nothing reached the vault and the UI stays live. Asked again
/// by `.install <bundle>` (an update from the same source), `n` on A and
/// Esc on B leaves C unasked and all three listed once more.
#[test]
#[serial]
fn esc_on_a_vault_secret_modal_defers_that_and_every_later_secret_and_keeps_the_install() {
    let dir = fresh_config_dir("install-esc-defers-secrets");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-e9s";
    let secrets = ["BUNDLE_SECRET_A", "BUNDLE_SECRET_B", "BUNDLE_SECRET_C"];
    let repo = write_bundle_repo(&dir, name, &secrets);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&dir);
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    wait_for_the_modal(
        &mut pty,
        install_at,
        ".install (first secret)",
        &["Add{{BUNDLE_SECRET_A}}tothevaultnow?", "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        DEFERRED_SECRETS_NOTICE,
        "the deferred-secrets notice after Esc on the first secret modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let flat = squeezed(&since);
    for later in ["BUNDLE_SECRET_B", "BUNDLE_SECRET_C"] {
        assert!(
            !flat.contains(&format!("Add{{{{{later}}}}}tothevaultnow?")),
            "Esc on the first secret modal still asked about {later}:\n{since}"
        );
    }
    assert!(
        flat.contains("{{BUNDLE_SECRET_A}}{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}"),
        "the notice does not list all three deferred secrets in order:\n{since}"
    );
    assert!(
        !flat.contains("thequestionwascancelled") && !flat.contains("nobodycouldbeasked"),
        "a deferred secret printed a kept note (the summary alone reports deferred secrets):\n{since}"
    );
    assert!(!since.contains("exit="), "`.install` quit the UI:\n{since}");
    for role in ["role", "obsolete"] {
        let installed = dir.join("roles").join(format!("{name}-{role}.md"));
        assert!(
            installed.exists(),
            "the bundle's {role} file was not installed at {}:\n{since}",
            installed.display()
        );
    }
    let mcp_files = config_files_mentioning(&dir, "bundle-k7s-server");
    assert!(
        mcp_files
            .iter()
            .any(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
        "the merged mcp.json does not hold the bundle's server: {mcp_files:?}\n{since}"
    );
    assert!(
        !new_files_named(&dir, &files_before, name).is_empty(),
        "no file records the installed bundle `{name}`:\n{since}"
    );
    let secret_holders = config_files_mentioning(&dir, "BUNDLE_SECRET_A");
    assert!(
        secret_holders
            .iter()
            .all(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
        "a file other than mcp.json holds BUNDLE_SECRET_A (a vault write?): {secret_holders:?}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a secret question released the terminal:\n{since}"
    );

    // The update from the same source asks about every secret again:
    // `n` defers A alone, Esc on B defers B and C together.
    let update_at = pty.raw().len();
    pty.send(format!(".install {name}\r").as_bytes()).unwrap();
    wait_for_the_modal(
        &mut pty,
        update_at,
        ".install <bundle> (first secret again)",
        &["Add{{BUNDLE_SECRET_A}}tothevaultnow?", "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    // The second secret's modal differs from the first by one letter, so
    // the painter sends only that cell; a resize forces the whole frame.
    pty.read_for(Duration::from_millis(500));
    let answered_at = pty.raw().len();
    pty.resize(60, 139).unwrap();
    wait_for_the_modal(
        &mut pty,
        answered_at,
        ".install <bundle> (second secret)",
        &["Add{{BUNDLE_SECRET_B}}tothevaultnow?", "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        "{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}",
        "the deferred list of the update after `n` then Esc",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
    let flat = squeezed(&since);
    assert!(
        !flat.contains("Add{{BUNDLE_SECRET_C}}tothevaultnow?"),
        "Esc on the second secret modal still asked about the third secret:\n{since}"
    );
    assert!(
        flat.contains("{{BUNDLE_SECRET_A}}{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}"),
        "the update's notice does not list all three deferred secrets:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a secret question of the update released the terminal:\n{since}"
    );
    assert_ui_alive_then_quit(&mut pty, "the deferred secrets");
}

/// `.uninstall <bundle>` of a bundle whose role AND mcp.json server were
/// both edited after install: `delete-all` on the file modal deletes the
/// owned files but does NOT carry into the mcp.json loop, so the server
/// gets its own `MCP server '<key>' was modified locally after install`
/// modal. Esc there keeps the server, prints exactly one `Kept the
/// locally modified items: the question was cancelled.` note, the
/// summary counts `deleted=2 kept=0` files and `removed=0 kept=1`
/// servers, the bundle is partially uninstalled and the UI stays live.
#[test]
#[serial]
fn delete_all_on_the_uninstall_file_modal_does_not_carry_into_the_mcp_server_modal() {
    let dir = fresh_config_dir("uninstall-delete-all-mcp");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-d9m";
    let repo = write_bundle_repo(&dir, name, &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    install_bundle_without_questions(&mut pty, &repo);
    let role = dir.join("roles").join(format!("{name}-role.md"));
    let edited = dir.join("roles").join(format!("{name}-obsolete.md"));
    fs::write(&edited, EDITED_OBSOLETE).unwrap();
    let edited_mcp = edit_the_bundle_server_in_mcp_json(&dir);

    let at = pty.raw().len();
    pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
    wait_for_the_modal(
        &mut pty,
        at,
        ".uninstall",
        &[&format!("Uninstall bundle '{name}'?"), "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_the_modal(
        &mut pty,
        at,
        ".uninstall (modified file)",
        &[
            &format!("Fileroles/{name}-obsolete.mdwasmodifiedlocallyafterinstall"),
            "delete-all",
        ],
    );
    pty.read_for(HOTKEY_GAP);
    let files_answered_at = pty.raw().len();
    pty.send(b"4").unwrap();
    wait_for_the_modal(
        &mut pty,
        files_answered_at,
        ".uninstall (modified server)",
        &[
            "MCPserver'bundle-k7s-server'wasmodifiedlocallyafterinstall",
            "keep",
            "delete-all",
        ],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        "the question was cancelled",
        "the note after Esc on the modified-server modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
    let flat = squeezed(&since);
    assert_eq!(
        flat.matches("Keptthelocallymodifieditems:thequestionwascancelled.")
            .count(),
        1,
        "the cancelled note was printed more or less than once:\n{since}"
    );
    assert!(
        !flat.contains("nobodycouldbeasked"),
        "the Esc note reads as the headless one:\n{since}"
    );
    assert!(
        flat.contains("keptserver'bundle-k7s-server'")
            && flat.contains("mcpservers:removed=0kept=1")
            && flat.contains("files:deleted=2kept=0"),
        "delete-all on the files carried into the servers, or the summary disagrees:\n{since}"
    );
    assert!(
        flat.contains("partiallyuninstalled"),
        "the bundle with a kept server is not reported partially uninstalled:\n{since}"
    );
    assert!(
        !role.exists() && !edited.exists(),
        "delete-all on the file modal left an owned file behind:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("mcp.json")).ok().as_deref(),
        Some(edited_mcp.as_str()),
        "the kept server was rewritten or removed:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "an uninstall question released the terminal:\n{since}"
    );
    assert_ui_alive_then_quit(&mut pty, "the cancelled server question");
}

/// The keep-all twin: with both roles and the server edited, `keep-all`
/// on the first file modal settles the whole run: no second file modal,
/// no server modal, no kept note (an answer was given), two `kept
/// modified file` lines, the server kept and the summary saying so.
#[test]
#[serial]
fn keep_all_on_the_uninstall_file_modal_carries_into_the_mcp_server_loop_without_asking() {
    let dir = fresh_config_dir("uninstall-keep-all-mcp");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-k9m";
    let repo = write_bundle_repo(&dir, name, &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    install_bundle_without_questions(&mut pty, &repo);
    let role = dir.join("roles").join(format!("{name}-role.md"));
    let obsolete = dir.join("roles").join(format!("{name}-obsolete.md"));
    let edited_role = format!("{}# edited\n", fs::read_to_string(&role).unwrap());
    fs::write(&role, &edited_role).unwrap();
    fs::write(&obsolete, EDITED_OBSOLETE).unwrap();
    let edited_mcp = edit_the_bundle_server_in_mcp_json(&dir);

    let at = pty.raw().len();
    pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
    wait_for_the_modal(
        &mut pty,
        at,
        ".uninstall",
        &[&format!("Uninstall bundle '{name}'?"), "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_the_modal(
        &mut pty,
        at,
        ".uninstall (first modified file)",
        &["wasmodifiedlocallyafterinstall", "keep-all"],
    );
    pty.read_for(HOTKEY_GAP);
    let answered_at = pty.raw().len();
    pty.send(b"3").unwrap();
    wait_for_squeezed(
        &mut pty,
        "mcp servers: removed=0 kept=1",
        "the uninstall summary after keep-all",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[answered_at..]));
    let flat = squeezed(&since);
    assert!(
        !flat.contains("MCPserver'bundle-k7s-server'wasmodifiedlocallyafterinstall")
            && !flat.contains("Esc=cancel"),
        "keep-all did not carry into the mcp.json loop (another modal came up):\n{since}"
    );
    assert!(
        !flat.contains("Keptthelocallymodifieditems"),
        "a kept note was printed though keep-all was answered:\n{since}"
    );
    assert_eq!(
        flat.matches("keptmodifiedfile").count(),
        2,
        "the ledger does not carry one `kept modified file` line per kept role:\n{since}"
    );
    assert!(
        flat.contains("keptserver'bundle-k7s-server'") && flat.contains("files:deleted=0kept=2"),
        "the summary does not show every item kept:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&role).ok().as_deref(),
        Some(edited_role.as_str()),
        "keep-all lost the first role's edit:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(&obsolete).ok().as_deref(),
        Some(EDITED_OBSOLETE),
        "keep-all lost the second role's edit:\n{since}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("mcp.json")).ok().as_deref(),
        Some(edited_mcp.as_str()),
        "keep-all rewrote or removed the kept server:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "an uninstall question released the terminal:\n{since}"
    );
    assert_ui_alive_then_quit(&mut pty, "keep-all on the uninstall");
}

// ---------------------------------------------------------------------
// Cancelling INSIDE the vault wizard a bundle secret opens, and Esc on the
// MCP conflict select of an install.
// ---------------------------------------------------------------------

/// The note the merge prints when the MCP conflict select was closed
/// without an answer.
const KEPT_MCP_SERVER_CANCELLED_NOTE: &str =
    "Kept the local MCP server 'bundle-k7s-server': the question was cancelled.";

/// Typed into the vault password modal and left unsubmitted before Esc;
/// no file may end up holding it.
const CANCELLED_VAULT_PASSWORD: &str = "cancelled-pw-v4q";

/// Waits for the modal `markers` describe after `from`, with a resize
/// first so the whole frame is on the pty (a modal differing from the
/// last by a few cells paints only those cells).
fn next_modal_after_a_repaint(
    pty: &mut Pty,
    from: usize,
    command: &str,
    markers: &[&str],
    rows: &mut u16,
) -> String {
    *rows = if *rows == 60 { 59 } else { 60 };
    pty.resize(*rows, 140).unwrap();
    wait_for_the_modal(pty, from, command, markers)
}

/// The bundle install a vault-wizard cancel is checked against.
struct VaultScene<'a> {
    dir: &'a Path,
    name: &'a str,
    password_file: &'a Path,
    files_before: &'a [PathBuf],
}

/// One install pass of `command` on a bundle with secrets A and B: `y`
/// on `Add {{ A }} to the vault now?` opens the vault wizard, `walk`
/// drives it up to the widget to cancel, Esc there. Then: the deferred
/// notice lists A and B in order, B was never asked about, no password
/// file exists at `password_file` (or anywhere new under `dir`), the
/// bundle's files are installed, the UI is still up and the terminal was
/// never released. Returns the text since the command.
fn esc_inside_the_vault_wizard_defers_the_rest(
    pty: &mut Pty,
    scene: &VaultScene<'_>,
    command: &str,
    walk: &dyn Fn(&mut Pty, usize, &mut u16),
    rows: &mut u16,
    what: &str,
) -> String {
    let VaultScene {
        dir,
        name,
        password_file,
        files_before,
    } = *scene;
    let at = pty.raw().len();
    pty.send(format!("{command}\r").as_bytes()).unwrap();
    wait_for_the_modal(
        pty,
        at,
        &format!("{command} ({what}: first secret)"),
        &["Add{{BUNDLE_SECRET_A}}tothevaultnow?", "y=yes"],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    pty.read_for(Duration::from_millis(500));
    let answered_at = pty.raw().len();
    walk(pty, answered_at, rows);
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        pty,
        DEFERRED_SECRETS_NOTICE,
        &format!("{what}: the deferred-secrets notice after Esc inside the vault wizard"),
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
    let flat = squeezed(&since);
    assert!(
        !flat.contains("Add{{BUNDLE_SECRET_B}}tothevaultnow?"),
        "{what}: Esc inside the vault wizard still asked about the later secret:\n{since}"
    );
    assert!(
        flat.contains("{{BUNDLE_SECRET_A}}{{BUNDLE_SECRET_B}}"),
        "{what}: the notice does not list both secrets as deferred in order:\n{since}"
    );
    assert!(
        !flat.contains("created.") && !flat.contains("updated."),
        "{what}: the cancelled wizard reported a password file written:\n{since}"
    );
    assert!(
        !flat.contains("Refusingto") && !flat.contains("Error:"),
        "{what}: the install failed instead of deferring the secrets:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "{what}: the install quit the UI:\n{since}"
    );
    assert!(
        !password_file.exists(),
        "{what}: a password file was written at {} though the wizard was cancelled:\n{since}",
        password_file.display()
    );
    let written: Vec<PathBuf> = walk_files(dir)
        .into_iter()
        .filter(|path| !files_before.contains(path))
        .filter(|path| {
            fs::read(path).is_ok_and(|bytes| {
                bytes
                    .windows(CANCELLED_VAULT_PASSWORD.len())
                    .any(|window| window == CANCELLED_VAULT_PASSWORD.as_bytes())
            })
        })
        .collect();
    assert!(
        written.is_empty(),
        "{what}: a file holds the password typed before the cancel: {written:?}\n{since}"
    );
    for role in ["role", "obsolete"] {
        let installed = dir.join("roles").join(format!("{name}-{role}.md"));
        assert!(
            installed.exists(),
            "{what}: the bundle's {role} file was not installed at {}:\n{since}",
            installed.display()
        );
    }
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "{what}: a vault wizard question released the terminal:\n{since}"
    );
    since
}

/// [`walk`] under its harness name, for the closure above that shadows it.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    walk(dir)
}

/// `.install <local bundle>` with two secrets after the vault's password
/// file went missing while the UI was up (the boot needs it, the install
/// runs later): `y` on `Add {{ A }} to the vault now?` opens the
/// password-file wizard as modals. Esc on each of its three widgets
/// in turn (the `create one now?` confirm on the install, the path
/// Input on the first update, the password modal with a password typed
/// but not submitted on the second) defers A and every later secret, the
/// install or update still succeeds and lists both secrets as deferred,
/// no password file is written, and no new file anywhere under the
/// config dir holds the typed password.
#[test]
#[serial]
fn esc_inside_the_vault_wizard_of_a_bundle_secret_defers_the_rest_and_writes_no_password_file() {
    let dir = fresh_config_dir("install-esc-inside-vault-wizard");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let password_file = dir.join("vault-pass");
    let name = "bundle-w2v";
    let repo = write_bundle_repo(&dir, name, &["BUNDLE_SECRET_A", "BUNDLE_SECRET_B"]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    // The boot checked the password file; once it is gone the first `yes`
    // on a secret reaches the wizard rather than the vault.
    fs::remove_file(&password_file).unwrap();
    let files_before = walk(&dir);
    let install = format!(".install {}", repo.display());
    // The size the repaints alternate from, carried across the passes so
    // no resize is a no-op.
    let mut rows = 60u16;
    let scene = VaultScene {
        dir: &dir,
        name,
        password_file: &password_file,
        files_before: &files_before,
    };

    // First pass: Esc on the `create one now?` confirm.
    esc_inside_the_vault_wizard_defers_the_rest(
        &mut pty,
        &scene,
        &install,
        &|pty, from, rows| {
            next_modal_after_a_repaint(
                pty,
                from,
                ".install (password-file confirm)",
                &["Nopasswordfileconfigured", "createonenow?", "y=yes"],
                rows,
            );
        },
        &mut rows,
        "Esc on the create confirm",
    );

    // Second pass (an update from the same source): Enter on the confirm,
    // Esc on the path Input.
    let update = format!(".install {name}");
    esc_inside_the_vault_wizard_defers_the_rest(
        &mut pty,
        &scene,
        &update,
        &|pty, from, rows| {
            next_modal_after_a_repaint(
                pty,
                from,
                ".install <bundle> (password-file confirm)",
                &["createonenow?", "y=yes"],
                rows,
            );
            pty.read_for(HOTKEY_GAP);
            pty.send(b"\r").unwrap();
            pty.read_for(Duration::from_millis(500));
            let at = pty.raw().len();
            next_modal_after_a_repaint(
                pty,
                at,
                ".install <bundle> (password-file path)",
                &["Enterthepathtothepasswordfiletocreate:", "Enter=answer"],
                rows,
            );
        },
        &mut rows,
        "Esc on the path Input",
    );

    // Third pass: Enter on the confirm, Enter on the path (the default),
    // Esc on the password modal.
    esc_inside_the_vault_wizard_defers_the_rest(
        &mut pty,
        &scene,
        &update,
        &|pty, from, rows| {
            next_modal_after_a_repaint(
                pty,
                from,
                ".install <bundle> (password-file confirm, third pass)",
                &["createonenow?", "y=yes"],
                rows,
            );
            pty.read_for(HOTKEY_GAP);
            pty.send(b"\r").unwrap();
            pty.read_for(Duration::from_millis(500));
            let at = pty.raw().len();
            next_modal_after_a_repaint(
                pty,
                at,
                ".install <bundle> (password-file path, third pass)",
                &["Enterthepathtothepasswordfiletocreate:", "Enter=answer"],
                rows,
            );
            answer_field_modal(pty, "");
            pty.read_for(Duration::from_millis(500));
            let at = pty.raw().len();
            next_modal_after_a_repaint(
                pty,
                at,
                ".install <bundle> (vault password)",
                &["Enterapasswordtoencryptallvaultsecrets:", "Enter=answer"],
                rows,
            );
            pty.read_for(HOTKEY_GAP);
            pty.send(CANCELLED_VAULT_PASSWORD.as_bytes()).unwrap();
            pty.read_for(Duration::from_millis(400));
        },
        &mut rows,
        "Esc on the password modal",
    );

    assert_ui_alive_then_quit(&mut pty, "the cancelled vault wizards");
}

/// `.install <local bundle>` whose mcp.json defines a server the local
/// mcp.json already holds differently: Esc on the `Conflict on MCP
/// server` Select modal keeps the local definition, the install still
/// completes (role installed, merge report says `kept local`), exactly
/// one note says `Kept the local MCP server '<name>': the question was
/// cancelled.`, nothing is refused, the terminal is never released and
/// the UI stays live.
#[test]
#[serial]
fn esc_on_the_mcp_conflict_select_modal_keeps_the_local_server_and_says_the_question_was_cancelled()
{
    let dir = fresh_config_dir("install-mcp-conflict-esc");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let local_before = write_conflicting_local_mcp_server(&dir);
    let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        install_at,
        ".install (MCP conflict)",
        &[
            "ConflictonMCPserver'bundle-k7s-server'",
            "keeplocal",
            "takeremote",
            "abortmerge",
        ],
    );
    assert!(
        !squeezed(&modal).contains("Other(custom)"),
        "the MCP conflict select is closed but the modal offers a custom row:\n{modal}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        KEPT_MCP_SERVER_CANCELLED_NOTE,
        "the note after Esc on the MCP conflict modal",
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let flat = squeezed(&since);
    assert_eq!(
        flat.matches(&squeezed(KEPT_MCP_SERVER_CANCELLED_NOTE))
            .count(),
        1,
        "the kept-server note was printed more or less than once:\n{since}"
    );
    assert!(
        flat.contains("keptlocal:bundle-k7s-server"),
        "the merge report does not say the server was kept local:\n{since}"
    );
    assert!(
        !flat.contains(&squeezed(MCP_CONFLICT_REFUSAL)) && !flat.contains("Abortedmerge"),
        "Esc on the conflict modal refused or aborted the merge instead of keeping local:\n{since}"
    );
    assert!(
        !since.contains("exit="),
        "the install quit the UI:\n{since}"
    );
    let frame = repainted_frame(&mut pty, 58, 140);
    assert!(
        !squeezed(&frame).contains("Esc=cancel"),
        "the MCP conflict modal is still up after Esc:\n{frame}"
    );
    assert!(
        dir.join("roles").join("bundle-k7s-role.md").exists(),
        "the bundle's role was not installed after Esc on the conflict modal:\n{since}"
    );
    let local_before: serde_json::Value = serde_json::from_str(&local_before).unwrap();
    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
    assert_eq!(
        merged, local_before,
        "Esc on the conflict modal changed the local mcp.json:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the conflict modal released the terminal:\n{since}"
    );
    assert_ui_alive_then_quit(&mut pty, "Esc on the MCP conflict modal");
}

// ---------------------------------------------------------------------
// A tool's open select answered with typed text in the plain REPL and
// from a Python tool under the UI, two MCP conflicts where the first is
// cancelled, and a tool Input question whose default is the empty
// string.
// ---------------------------------------------------------------------

/// Text a user might type on the custom row: inner spaces, both quote
/// kinds, a dollar sign and a backslash, none of which may be expanded,
/// escaped or dropped on the way to the tool.
const TYPED_CUSTOM_ANSWER: &str = "my \"own\" pick $HOME 'x11' back\\slash";

/// `list` in the plain REPL: the `Other (custom)` row opens the
/// `Custom response:` text prompt, and the text typed there reaches the
/// bash tool verbatim (the helper prints it as is and exits 2) and the
/// model sees it in the tool's output.
#[test]
#[serial]
fn a_tools_list_question_in_the_plain_repl_opens_custom_response_and_passes_the_typed_text_verbatim()
 {
    let dir = fresh_config_dir("tool-list-repl-custom");
    let _guard = TempDirGuard(dir.clone());
    let (fake, out) = list_tool_turn(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
    pty.send(b"call the list tool\r").unwrap();
    pty.wait_for_after("Other (custom)", asked_at, PTY_DEADLINE)
        .expect("the tool's list question never offered the custom row");
    pty.read_for(Duration::from_millis(500));
    for _ in 0..3 {
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(300));
    }
    let custom_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    pty.wait_for_after("Custom response:", custom_at, PTY_DEADLINE)
        .expect("Enter on `Other (custom)` did not open the `Custom response:` prompt");
    pty.read_for(Duration::from_millis(400));
    pty.send(TYPED_CUSTOM_ANSWER.as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        TYPED_CUSTOM_ANSWER,
        "the text typed at `Custom response:` did not reach the tool verbatim:\n{transcript}"
    );
    assert_eq!(
        list_exit_status(&out),
        "2",
        "typed text did not reach the tool with the custom-answer status:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");
    // The tool's output reaches the model as the `output` field of the
    // JSON object in the tool message's content.
    let tool_output = format!("fixture picked: {TYPED_CUSTOM_ANSWER} q7l-r");
    let body: serde_json::Value = serde_json::from_str(&fake.bodies()[1]).unwrap();
    let handed: Vec<String> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| serde_json::from_str::<serde_json::Value>(content).ok())
        .filter_map(|content| content["output"].as_str().map(str::to_string))
        .collect();
    assert!(
        handed.iter().any(|output| output.trim_end() == tool_output),
        "the tool's output handed to the model does not carry the typed text verbatim: \
         {handed:?}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A Python tool that loads the prompt helpers by the path the shim
/// exports, asks one `select`, and records the Python type and value of
/// the answer.
const RECORDING_PY_SELECT_TOOL: &str = r#"import importlib.util
import os


def run(out: str) -> str:
    """Ask the user to pick one entry and record the answer type.

    Args:
        out (str): Where to write the answer.

    Returns:
        str: The answer the user gave.
    """
    spec = importlib.util.spec_from_file_location("prompt_utils", os.environ["LLM_PROMPT_UTILS_PY"])
    pu = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(pu)
    v = pu.select("Pick one entry p5y-q", ["alpha-p5y", "beta-p5y"])
    with open(out, "w") as f:
        f.write(type(v).__name__ + ":" + str(v))
    return "fixture picked: " + str(v) + " p5y-r"
"#;

/// The Python helper's `select` under the UI: the Select modal offers the
/// custom row, and text typed there comes back from `select` as a `str`
/// holding exactly that text (an index is an `int`).
#[test]
#[serial]
fn a_python_tools_select_under_the_ui_returns_the_typed_custom_text_as_a_str() {
    let dir = fresh_config_dir("tool-py-select-custom");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("py-select-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_py_select".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done p5y".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_py_select.py", RECORDING_PY_SELECT_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let asked_at = pty.raw().len();
    pty.send(b"call the python select tool\r").unwrap();
    wait_for_the_modal(
        &mut pty,
        asked_at,
        "the python select tool",
        &["p5y-q", "alpha-p5y", "beta-p5y", "Other (custom)"],
    );
    pty.read_for(HOTKEY_GAP);
    for _ in 0..2 {
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
    }
    pty.send(b"\r").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"  typed by hand p5y ").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the python tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done p5y", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "str:  typed by hand p5y ",
        "the Python `select` did not return the typed text as a str:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A bundle repo whose mcp.json ships two servers, and a local mcp.json
/// that holds both under different definitions: two conflicts in a row.
fn two_mcp_conflicts(dir: &Path) -> (PathBuf, String) {
    let repo = write_bundle_repo(dir, "bundle-k7s", &[]);
    fs::write(
        repo.join("mcp.json"),
        "{\"mcpServers\": {\
         \"bundle-k7s-server\": {\"type\": \"stdio\", \"command\": \"echo\", \"args\": [\"k7s\"], \"env\": {}}, \
         \"bundle-k7s-second\": {\"type\": \"stdio\", \"command\": \"echo\", \"args\": [\"k7s-2\"], \"env\": {}}}}\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "two servers"]);
    let local = "{\"mcpServers\": {\
                 \"bundle-k7s-server\": {\"type\": \"stdio\", \"command\": \"printf\", \"args\": [\"local-1-k7s\"], \"env\": {}}, \
                 \"bundle-k7s-second\": {\"type\": \"stdio\", \"command\": \"printf\", \"args\": [\"local-2-k7s\"], \"env\": {}}}}\n";
    fs::write(dir.join("mcp.json"), local).unwrap();
    (repo, local.to_string())
}

/// Checks what follows Esc on the first of two MCP conflicts: the second
/// conflict is never asked, exactly one `question was cancelled` note,
/// both servers kept local, the role installed, nothing refused, and
/// the local mcp.json unchanged.
fn assert_both_kept_local_after_one_esc(dir: &Path, local_before: &str, since: &str) {
    let flat = squeezed(since);
    assert!(
        !flat.contains("ConflictonMCPserver'bundle-k7s-second'"),
        "the second conflict was asked after the first was cancelled:\n{since}"
    );
    assert_eq!(
        flat.matches("thequestionwascancelled").count(),
        1,
        "expected exactly one `the question was cancelled` note:\n{since}"
    );
    assert!(
        !flat.contains("Refusingto") && !flat.contains("Abortedmerge") && !flat.contains("Error:"),
        "Esc on the first conflict refused, aborted or failed the install:\n{since}"
    );
    assert!(
        dir.join("roles").join("bundle-k7s-role.md").exists(),
        "the bundle's role was not installed:\n{since}"
    );
    let before: serde_json::Value = serde_json::from_str(local_before).unwrap();
    let merged: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
    assert_eq!(
        merged, before,
        "Esc on the first conflict changed a local server definition:\n{since}"
    );
}

/// Under the UI: Esc on the first of two MCP conflict modals keeps both
/// servers local with one note and never raises the second modal.
#[test]
#[serial]
fn esc_on_the_first_of_two_mcp_conflict_modals_keeps_both_local_with_one_note() {
    let dir = fresh_config_dir("install-two-mcp-conflicts-esc");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let (repo, local_before) = two_mcp_conflicts(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        install_at,
        ".install (first MCP conflict)",
        &["ConflictonMCPserver", "keeplocal"],
    );
    let first_is_server = squeezed(&modal).contains("ConflictonMCPserver'bundle-k7s-server'");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        "the question was cancelled.",
        "the note after Esc on the first MCP conflict modal",
    );
    pty.read_for(Duration::from_millis(2500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let since = if first_is_server {
        since
    } else {
        since
            .replace("bundle-k7s-server", "SWAP")
            .replace("bundle-k7s-second", "bundle-k7s-server")
            .replace("SWAP", "bundle-k7s-second")
    };
    assert_both_kept_local_after_one_esc(&dir, &local_before, &since);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the conflict modal released the terminal:\n{since}"
    );
    assert_ui_alive_then_quit(&mut pty, "Esc on the first of two MCP conflicts");
}

/// In the plain REPL: Esc on the first of two inquire conflict selects
/// keeps both servers local with one note and never asks the second.
#[test]
#[serial]
fn esc_on_the_first_of_two_mcp_conflicts_in_the_plain_repl_keeps_both_local_with_one_note() {
    let dir = fresh_config_dir("install-two-mcp-conflicts-esc-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let (repo, local_before) = two_mcp_conflicts(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    pty.wait_for_after("Conflict on MCP server", install_at, PTY_DEADLINE)
        .expect("`.install` did not ask about the first MCP conflict");
    pty.read_for(Duration::from_millis(600));
    let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let first_is_server = asked.contains("Conflict on MCP server 'bundle-k7s-server'");
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        "the question was cancelled.",
        "the note after Esc on the first MCP conflict",
    );
    pty.read_for(Duration::from_millis(2500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let since = if first_is_server {
        since
    } else {
        since
            .replace("bundle-k7s-server", "SWAP")
            .replace("bundle-k7s-second", "bundle-k7s-server")
            .replace("SWAP", "bundle-k7s-second")
    };
    assert_both_kept_local_after_one_esc(&dir, &local_before, &since);
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A tool that asks one Input question over the prompt channel with
/// `"default": ""` on the wire and records the answer it gets back.
const EMPTY_DEFAULT_INPUT_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user for a label and record the answer.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out id line reply v\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   id=\"e0d-$$\"\n\
    \x20   line=\"$(jq -cn --arg id \"$id\" '{coyote_prompt:{id:$id,action:\"input\",question:\"Label e0d-q\",options:[],default:\"\"}}')\"\n\
    \x20   printf '%s\\n' \"$line\" >&\"$_COYOTE_PROMPT_OUT\"\n\
    \x20   while IFS= read -r reply <\"$_COYOTE_PROMPT_REPLY_PATH\"; do\n\
    \x20       if printf '%s' \"$reply\" | jq -e --arg id \"$id\" '.id == $id' >/dev/null 2>&1; then break; fi\n\
    \x20   done\n\
    \x20   v=\"$(printf '%s' \"$reply\" | jq -r 'if (.answer | type) == \"string\" then \"[\" + .answer + \"]\" else \"cancelled\" end')\"\n\
    \x20   printf '%s' \"$v\" > \"$out\"\n\
    \x20   echo \"fixture label: $v e0d-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

fn empty_default_turn(dir: &Path) -> (FakeLlm, PathBuf) {
    let out = dir.join("label-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_label_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done e0d".to_string()),
    ]);
    write_fake_llm_config(dir, &fake);
    write_tool(dir, "fixture_label_rec.sh", EMPTY_DEFAULT_INPUT_TOOL);
    (fake, out)
}

/// `"default": ""` in the plain REPL is no default: the inquire prompt
/// shows no `(...)` default hint after the question, and typed text is
/// the answer.
#[test]
#[serial]
fn a_tools_input_question_with_an_empty_default_shows_no_default_hint_in_the_plain_repl() {
    let dir = fresh_config_dir("tool-input-empty-default-repl");
    let _guard = TempDirGuard(dir.clone());
    let (fake, out) = empty_default_turn(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
    pty.send(b"call the label tool\r").unwrap();
    pty.wait_for_after("Label e0d-q", asked_at, PTY_DEADLINE)
        .expect("the tool's Input question never reached the terminal");
    pty.read_for(Duration::from_millis(600));
    let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
    let line = asked
        .lines()
        .rev()
        .find(|line| line.contains("Label e0d-q"))
        .unwrap()
        .to_string();
    let after = line.split("Label e0d-q").nth(1).unwrap_or("");
    assert!(
        !after.contains('(') && !after.contains(')'),
        "`\"default\": \"\"` painted a default hint after the question: {line:?}\n{asked}"
    );
    pty.send(b"typed-e0d").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done e0d", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "[typed-e0d]",
        "{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `"default": ""` under the UI is no default: no dim placeholder text is
/// painted in the Input modal, and Enter on the untouched field answers
/// the empty string (nothing substituted for it).
#[test]
#[serial]
fn a_tools_input_question_with_an_empty_default_paints_no_placeholder_under_the_ui() {
    let dir = fresh_config_dir("tool-input-empty-default-tui");
    let _guard = TempDirGuard(dir.clone());
    let (fake, out) = empty_default_turn(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let asked_at = pty.raw().len();
    pty.send(b"call the label tool\r").unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        asked_at,
        "the label tool",
        &["Labele0d-q", "Enter=answer"],
    );
    assert!(
        !squeezed(&modal).contains("\"\"") && !squeezed(&modal).contains("default:"),
        "the empty default is rendered as visible text in the modal:\n{modal}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done e0d", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "[]",
        "Enter on the untouched modal with `\"default\": \"\"` did not answer the empty string:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A tool that asks one `checkbox` question and records the indices it
/// got back.
const RECORDING_CHECKBOX_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user to tick entries and record the indices.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out v\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   v=\"$(checkbox \"Tick entries c3b-q\" alpha-c3b beta-c3b)\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture ticked: ${v:-cancelled} c3b-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// The custom row is for selects only: a tool's `checkbox` question,
/// in the plain REPL and under the UI, lists the entries with no
/// `Other (custom)` row.
#[test]
#[serial]
fn a_tools_checkbox_question_stays_closed_with_no_custom_row_in_either_interface() {
    for ui in ["repl", "tui"] {
        let dir = fresh_config_dir(&format!("tool-checkbox-closed-{ui}"));
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("checkbox-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_checkbox_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done c3b".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_checkbox_rec.sh", RECORDING_CHECKBOX_TOOL);
        let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", ui)], WAIT_WRAPPER, "/dev/tty");
        if ui == "repl" {
            pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
        } else {
            pty.wait_for("no session", PTY_DEADLINE).unwrap();
        }
        pty.read_for(Duration::from_millis(300));
        let asked_at = pty.raw().len();
        pty.send(b"call the checkbox tool\r").unwrap();
        if ui == "repl" {
            pty.wait_for_after("beta-c3b", asked_at, PTY_DEADLINE)
                .expect("the checkbox question never reached the terminal");
        } else {
            wait_for_the_modal(
                &mut pty,
                asked_at,
                "the checkbox tool",
                &["c3b-q", "alpha-c3b", "beta-c3b"],
            );
        }
        pty.read_for(Duration::from_millis(800));
        let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
        assert!(
            !squeezed(&asked).contains("Other(custom)"),
            "[{ui}] a tool's checkbox question offers the custom row:\n{asked}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        wait_for_squeezed(&mut pty, "fixture done c3b", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "cancelled",
            "[{ui}] Esc on the checkbox question did not reach the tool as a cancel"
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(if ui == "repl" {
            b".exit\r" as &[u8]
        } else {
            b"\x04"
        })
        .unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "[{ui}] {transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(fake.served(), 2, "[{ui}] {transcript}");
    }
}
