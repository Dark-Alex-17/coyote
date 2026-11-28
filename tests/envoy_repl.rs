//! Black-box coverage of the built-in envoy through the interactive REPL.
//!
//! The one-shot `--agent envoy` CLI path is covered in `inspection_flags.rs`.
//! This file drives the real `coyote` binary through a pty so the REPL path
//! is exercised the way a human uses it: `.agent envoy` at a live prompt
//! materializes the embedded built-in into a private per-process temp dir,
//! `.info tools` shows what the envoy may call, and `.exit` removes the dir
//! again. Every wait is an `expect` bounded by `EXPECT_TIMEOUT`; nothing here
//! asserts on timing.
//!
//! **Unix only**, for the same reasons as `mesh_autostart_repl.rs`: the
//! harness answers the raw cursor position handshake itself and expectrl's
//! pty backend is the unix one. The file is compiled out on Windows.

#![cfg(unix)]

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use expectrl::process::unix::WaitStatus;
use expectrl::{Eof, Expect};

use support::bin::{DryRunConfig, coyote_command};
use support::pty::{Probe, cursor_position_reply, spawn_repl, strip_escapes, wait_for};

/// Prompt tails the REPL paints, as regexes: reedline colours the pieces
/// separately, so SGR sequences may sit between the model name, the agent
/// name and the `>` markers.
const TOP_PROMPT: &str = r"dry-model\)(\x1b\[[0-9;]*m)*>";
const ENVOY_PROMPT: &str = r"envoy>(\x1b\[[0-9;]*m)*>";

impl Probe {
    fn new(label: &str) -> Self {
        let probe = Self::fresh("coyote-envoy-repl", label);
        DryRunConfig::default()
            .vault_password_file(&probe.vault_pass())
            .write(&probe.config_dir);
        probe
    }

    fn command(&self) -> Command {
        let mut cmd = coyote_command(&self.dirs());
        cmd.env("XDG_CACHE_HOME", &self.temp_root)
            .env("TERM", "xterm-256color")
            .env_remove("IS_SANDBOX")
            .env_remove("ENVOY_DATA_DIR")
            .env_remove("ENVOY_CONFIG_FILE")
            .current_dir(&self.home_dir);
        cmd
    }

    fn envoy_temp_dirs(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.temp_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().contains("-envoy-"))
                    .unwrap_or(false)
            })
            .collect()
    }
}

/// The harness answers the REPL's sentinel query with the column it moved to,
/// the last move winning, and falls back to the fixed reply when nothing
/// moved the cursor (reedline anchoring its prompt, or plain painting).
#[test]
fn cursor_queries_are_answered_with_the_last_column_moved_to() {
    assert_eq!(cursor_position_reply(b"prompt text"), "\x1b[2;1R");
    assert_eq!(cursor_position_reply(b"\x1b[0m\x1b[2J\x1b[H"), "\x1b[2;1R");
    assert_eq!(cursor_position_reply(b"\x1b7\x1b[6G"), "\x1b[2;6R");
    assert_eq!(
        cursor_position_reply(b"\x1b[3G text \x1b[12G"),
        "\x1b[2;12R"
    );
    assert_eq!(cursor_position_reply(b"\x1b[G"), "\x1b[2;1R");
}

/// A human switches into the built-in at a live prompt, inspects its tool
/// set and leaves. Expected: the switch is accepted (the reservation refuses
/// `agent__spawn`, not a human at the prompt), the embedded agent is materialized
/// into exactly one owner-only dir under the process temp dir and never under
/// `<config_dir>/agents/`, the tool catalog holds exactly the bundled
/// read-only file tools and the user escalation tools (no execute, write,
/// spawn, memory, job, skill or mesh tool), and `.exit` removes the temp dir.
#[test]
fn repl_agent_envoy_materializes_privately_shows_only_read_only_and_escalation_tools_and_exits_clean()
 {
    let probe = Probe::new("switch");
    let mut session = spawn_repl(probe.command());

    wait_for(&mut session, TOP_PROMPT);
    assert!(
        probe.envoy_temp_dirs().is_empty(),
        "the envoy was materialized before anyone asked for it"
    );

    session
        .send(".agent envoy\r")
        .expect("switch into the envoy");
    let switch_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !switch_output.contains("reserved") && !switch_output.contains("Error"),
        ".agent envoy was refused for a human: {switch_output:?}"
    );

    let envoy_dirs = probe.envoy_temp_dirs();
    assert_eq!(
        envoy_dirs.len(),
        1,
        ".agent envoy: expected exactly one envoy temp dir, found {envoy_dirs:?}"
    );
    let envoy_dir = &envoy_dirs[0];
    let dir_name = envoy_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let own_pid = session.get_process().pid().as_raw();
    assert!(
        dir_name.contains(&format!("-{own_pid}-envoy-")),
        ".agent envoy: the temp dir {dir_name} is not tagged with the REPL's pid {own_pid}"
    );
    let mode = fs::metadata(envoy_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        ".agent envoy: the envoy dir is group/world accessible (mode {mode:o})"
    );
    assert!(
        envoy_dir.join("config.yaml").is_file(),
        ".agent envoy: the materialized config.yaml is missing from {envoy_dir:?}"
    );
    assert!(
        !probe.config_dir.join("agents").join("envoy").exists(),
        ".agent envoy: an envoy dir was written under <config_dir>/agents/"
    );

    session.send(".info tools\r").expect("list the tools");
    wait_for(
        &mut session,
        r"Tools enabled for the next request: [0-9]+\r?\n",
    );
    let listing = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    // Between the header and the next prompt: one indented tool name per
    // line, then the model-name prefix of the prompt that ends the capture.
    let tools: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains("dry-model") && !line.starts_with('.'))
        .map(str::to_owned)
        .collect();
    assert!(
        !tools.is_empty(),
        ".info tools: expected at least the user escalation tools, got nothing\n{listing:?}"
    );
    let read_only = ["fs_read", "fs_grep", "fs_glob"];
    for tool in &tools {
        for forbidden in [
            "execute_command",
            "fs_write",
            "fs_patch",
            "fs_mkdir",
            "fs_rm",
            "agent__",
            "memory__",
            "mesh__",
            "job__",
            "skill__",
            "todo__",
            "rag__",
        ] {
            assert!(
                !tool.starts_with(forbidden),
                ".info tools: the envoy can call `{tool}`, which matches the forbidden family `{forbidden}`\nall tools: {tools:?}"
            );
        }
        assert!(
            tool.starts_with("user__") || read_only.contains(&tool.as_str()),
            ".info tools: the envoy catalog holds `{tool}`, which is neither a user escalation tool nor a bundled read-only tool\nall tools: {tools:?}"
        );
    }
    for tool in read_only {
        assert!(
            tools.iter().any(|t| t == tool),
            ".info tools: the bundled read-only tool `{tool}` is missing\nall tools: {tools:?}"
        );
    }

    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let leftovers = probe.envoy_temp_dirs();
    let agents_envoy_exists = probe.config_dir.join("agents").join("envoy").exists();
    probe.cleanup();

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert!(
        leftovers.is_empty(),
        ".exit: the envoy temp dir survived the REPL exit: {leftovers:?}"
    );
    assert!(
        !agents_envoy_exists,
        "an envoy dir was written under <config_dir>/agents/ during the session"
    );
}

/// The other human entry point: starting the REPL already inside the envoy
/// with `coyote -a envoy`. Expected: the first prompt is the envoy prompt
/// (no reserved-name refusal), the built-in is materialized under the temp
/// dir by then, and `.exit` from that prompt removes it.
#[test]
fn repl_started_with_agent_envoy_lands_in_the_envoy_and_exits_clean() {
    let probe = Probe::new("startup");
    let mut command = probe.command();
    command.args(["--agent", "envoy"]);
    let mut session = spawn_repl(command);

    let banner = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !banner.contains("reserved") && !banner.contains("Error"),
        "-a envoy: the REPL refused the built-in for a human: {banner:?}"
    );
    let envoy_dirs = probe.envoy_temp_dirs();
    assert_eq!(
        envoy_dirs.len(),
        1,
        "-a envoy: expected exactly one envoy temp dir at the first prompt, found {envoy_dirs:?}"
    );
    let mode = fs::metadata(&envoy_dirs[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "-a envoy: the envoy dir is group/world accessible (mode {mode:o})"
    );

    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let leftovers = probe.envoy_temp_dirs();
    let agents_envoy_exists = probe.config_dir.join("agents").join("envoy").exists();
    probe.cleanup();

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert!(
        leftovers.is_empty(),
        ".exit: the envoy temp dir survived the REPL exit: {leftovers:?}"
    );
    assert!(
        !agents_envoy_exists,
        "-a envoy: an envoy dir was written under <config_dir>/agents/"
    );
}

/// `.agent <name> [session-name]` lets a human open an agent straight into a
/// named session. The envoy keeps no sessions, so `.agent envoy notes` is
/// expected to be refused with the no-sessions message at the top prompt, the
/// REPL stays at the top prompt and usable, nothing is written under
/// `<config_dir>/agents/` or `<config_dir>/sessions/`, a plain `.agent envoy`
/// afterwards still succeeds, and `.exit` leaves no envoy temp dir behind.
#[test]
fn repl_agent_envoy_with_a_session_name_is_refused_and_the_repl_stays_usable() {
    let probe = Probe::new("named-session");
    let mut session = spawn_repl(probe.command());

    wait_for(&mut session, TOP_PROMPT);

    session
        .send(".agent envoy notes\r")
        .expect("ask for the envoy with a named session");
    // Reedline repaints the prompt line while echoing the command, so the
    // top prompt alone cannot mark the end of the command; anchor on the
    // refusal text first, then on the prompt that follows it.
    let before_refusal = strip_escapes(&wait_for(&mut session, "does not keep sessions"));
    let mut refusal = before_refusal;
    refusal.push_str("does not keep sessions");
    refusal.push_str(&strip_escapes(&wait_for(&mut session, TOP_PROMPT)));
    assert!(
        !refusal.contains("reserved"),
        ".agent envoy notes: refused as reserved instead of as session-less: {refusal:?}"
    );
    assert!(
        !probe.config_dir.join("agents").join("envoy").exists(),
        ".agent envoy notes: an envoy dir was written under <config_dir>/agents/"
    );
    assert!(
        !probe.config_dir.join("sessions").exists(),
        ".agent envoy notes: a sessions/ dir was created for the envoy"
    );

    // The refusal must not have left the REPL half switched: a plain
    // `.agent envoy` from the same prompt still lands in the envoy.
    session
        .send(".agent envoy\r")
        .expect("switch into the envoy without a session");
    let switch_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !switch_output.contains("does not keep sessions")
            && !switch_output.contains("reserved")
            && !switch_output.contains("Error"),
        ".agent envoy after a refused named session was itself refused: {switch_output:?}"
    );
    let envoy_dirs = probe.envoy_temp_dirs();
    assert_eq!(
        envoy_dirs.len(),
        1,
        ".agent envoy: expected exactly one envoy temp dir after the refusal and the switch, found {envoy_dirs:?}"
    );

    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let leftovers = probe.envoy_temp_dirs();
    let agents_envoy_exists = probe.config_dir.join("agents").join("envoy").exists();
    let sessions_exists = probe.config_dir.join("sessions").exists();
    probe.cleanup();

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert!(
        leftovers.is_empty(),
        ".exit: the envoy temp dir survived the REPL exit: {leftovers:?}"
    );
    assert!(
        !agents_envoy_exists,
        "an envoy dir was written under <config_dir>/agents/ during the session"
    );
    assert!(
        !sessions_exists,
        "a sessions/ dir was created for the envoy during the session"
    );
}

/// `.session <name>` from inside the envoy is the other way to ask it for a
/// named session. Expected: the refusal carries the no-sessions message, the
/// REPL stays at the envoy prompt and usable, no `sessions/` dir appears under
/// the envoy temp dir or `<config_dir>/`, and `.exit` removes the temp dir.
#[test]
fn repl_session_inside_the_envoy_is_refused_and_the_repl_stays_usable() {
    let probe = Probe::new("session-inside");
    let mut session = spawn_repl(probe.command());

    wait_for(&mut session, TOP_PROMPT);
    session
        .send(".agent envoy\r")
        .expect("switch into the envoy");
    let switch_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !switch_output.contains("reserved") && !switch_output.contains("Error"),
        ".agent envoy was refused for a human: {switch_output:?}"
    );
    let envoy_dirs = probe.envoy_temp_dirs();
    assert_eq!(
        envoy_dirs.len(),
        1,
        ".agent envoy: expected exactly one envoy temp dir, found {envoy_dirs:?}"
    );
    let envoy_dir = envoy_dirs[0].clone();

    session
        .send(".session notes\r")
        .expect("ask the envoy for a named session");
    let mut refusal = strip_escapes(&wait_for(&mut session, "does not keep sessions"));
    refusal.push_str("does not keep sessions");
    refusal.push_str(&strip_escapes(&wait_for(&mut session, ENVOY_PROMPT)));
    assert!(
        !refusal.contains("reserved"),
        ".session notes: refused as reserved instead of as session-less: {refusal:?}"
    );
    assert!(
        !envoy_dir.join("sessions").exists(),
        ".session notes: a sessions/ dir was created under the envoy temp dir"
    );
    assert!(
        !probe.config_dir.join("sessions").exists(),
        ".session notes: a sessions/ dir was created under <config_dir>/"
    );

    session.send(".info tools\r").expect("list the tools");
    wait_for(
        &mut session,
        r"Tools enabled for the next request: [0-9]+\r?\n",
    );
    let listing = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        listing.contains("user__"),
        ".info tools after the refusal: the envoy prompt is not usable: {listing:?}"
    );

    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let leftovers = probe.envoy_temp_dirs();
    let sessions_exists = probe.config_dir.join("sessions").exists();
    probe.cleanup();

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert!(
        leftovers.is_empty(),
        ".exit: the envoy temp dir survived the REPL exit: {leftovers:?}"
    );
    assert!(
        !sessions_exists,
        "a sessions/ dir was created under <config_dir>/ during the session"
    );
}

/// A human who steps into the envoy and back out again. Expected: `.exit
/// agent` returns to the top prompt and the top-level tool set comes back
/// (the envoy's tool gate is keyed on the active agent, so a family the
/// top level owns and the envoy does not, `skill__*`, must reappear), a
/// second `.agent envoy` succeeds and lands on exactly one envoy temp dir
/// (the per-process dir is fixed at registration time and re-entry must not
/// mint another), repeating `.agent envoy` while already inside is accepted,
/// and `.exit` still removes the dir.
#[test]
fn repl_leaving_the_envoy_restores_the_top_level_tools_and_reentry_is_idempotent() {
    let probe = Probe::new("round-trip");
    let mut session = spawn_repl(probe.command());

    wait_for(&mut session, TOP_PROMPT);
    session
        .send(".info tools\r")
        .expect("list the top-level tools");
    wait_for(
        &mut session,
        r"Tools enabled for the next request: [0-9]+\r?\n",
    );
    let top_listing_before = strip_escapes(&wait_for(&mut session, TOP_PROMPT));
    assert!(
        top_listing_before.contains("skill__list"),
        "top level: expected the skill__ family before entering the envoy, so its return can be observed: {top_listing_before:?}"
    );

    session
        .send(".agent envoy\r")
        .expect("switch into the envoy");
    let switch_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !switch_output.contains("reserved") && !switch_output.contains("Error"),
        ".agent envoy was refused for a human: {switch_output:?}"
    );
    assert_eq!(
        probe.envoy_temp_dirs().len(),
        1,
        ".agent envoy: expected exactly one envoy temp dir"
    );

    session
        .send(".exit agent\r")
        .expect("leave the envoy for the top prompt");
    let leave_output = strip_escapes(&wait_for(&mut session, TOP_PROMPT));
    assert!(
        !leave_output.contains("Error"),
        ".exit agent from the envoy failed: {leave_output:?}"
    );

    session
        .send(".info tools\r")
        .expect("list the top-level tools again");
    wait_for(
        &mut session,
        r"Tools enabled for the next request: [0-9]+\r?\n",
    );
    let top_listing_after = strip_escapes(&wait_for(&mut session, TOP_PROMPT));
    assert!(
        top_listing_after.contains("skill__list"),
        ".exit agent: the envoy tool gate stuck to the top level (skill__ family missing): {top_listing_after:?}"
    );

    session
        .send(".agent envoy\r")
        .expect("switch back into the envoy");
    let reentry_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !reentry_output.contains("reserved") && !reentry_output.contains("Error"),
        "second .agent envoy was refused: {reentry_output:?}"
    );
    let reentry_dirs = probe.envoy_temp_dirs();
    assert_eq!(
        reentry_dirs.len(),
        1,
        "re-entry: expected exactly one envoy temp dir, found {reentry_dirs:?}"
    );

    session
        .send(".agent envoy\r")
        .expect("repeat .agent envoy while already inside");
    let repeat_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !repeat_output.contains("reserved") && !repeat_output.contains("Error"),
        ".agent envoy while already in the envoy was refused: {repeat_output:?}"
    );
    let repeat_dirs = probe.envoy_temp_dirs();
    assert_eq!(
        repeat_dirs.len(),
        1,
        "repeat: expected exactly one envoy temp dir, found {repeat_dirs:?}"
    );

    session.send(".info tools\r").expect("list the envoy tools");
    wait_for(
        &mut session,
        r"Tools enabled for the next request: [0-9]+\r?\n",
    );
    let envoy_listing = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    for forbidden in [
        "skill__",
        "execute_command",
        "agent__",
        "mesh__",
        "memory__",
        "rag__",
        "fs_mkdir",
        "fs_rm",
    ] {
        assert!(
            !envoy_listing.contains(forbidden),
            "re-entered envoy: `{forbidden}` is enabled after the round trip: {envoy_listing:?}"
        );
    }

    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let leftovers = probe.envoy_temp_dirs();
    let agents_envoy_exists = probe.config_dir.join("agents").join("envoy").exists();
    probe.cleanup();

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert!(
        leftovers.is_empty(),
        ".exit: the envoy temp dir survived the REPL exit: {leftovers:?}"
    );
    assert!(
        !agents_envoy_exists,
        "an envoy dir was written under <config_dir>/agents/ during the session"
    );
}

/// The other way a human leaves the REPL: end-of-input (Ctrl-D) at the envoy
/// prompt rather than `.exit`. Expected: the process exits 0 and the envoy
/// temp dir is removed on that path too.
#[test]
fn repl_ctrl_d_at_the_envoy_prompt_exits_clean_and_removes_the_temp_dir() {
    let probe = Probe::new("ctrl-d");
    let mut session = spawn_repl(probe.command());

    wait_for(&mut session, TOP_PROMPT);
    session
        .send(".agent envoy\r")
        .expect("switch into the envoy");
    let switch_output = strip_escapes(&wait_for(&mut session, ENVOY_PROMPT));
    assert!(
        !switch_output.contains("reserved") && !switch_output.contains("Error"),
        ".agent envoy was refused for a human: {switch_output:?}"
    );
    assert_eq!(
        probe.envoy_temp_dirs().len(),
        1,
        ".agent envoy: expected exactly one envoy temp dir"
    );

    session
        .send("\x04")
        .expect("send end-of-input at the envoy prompt");
    session.expect(Eof).expect("the REPL exits after Ctrl-D");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let leftovers = probe.envoy_temp_dirs();
    probe.cleanup();

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        "Ctrl-D: expected a clean exit, got {status:?}"
    );
    assert!(
        leftovers.is_empty(),
        "Ctrl-D: the envoy temp dir survived the REPL exit: {leftovers:?}"
    );
}
