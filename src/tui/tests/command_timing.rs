use super::*;
use crate::config::CommandPrefix;
use crate::repl::{repl_command_names, repl_help_text};
use crate::tui::command_timing::{COMMAND_TIMINGS, Timing, lookup};
use crate::tui::run_now::{CommandOutput, replay_deferred, run_now};
use std::collections::BTreeSet;

/// The forms only the dispatcher knows by their second word.
const DISPATCHER_ONLY: &[&str] = &[
    ".list roles",
    ".list sessions",
    ".list agents",
    ".list rags",
    ".list macros",
    ".list skills",
    ".list prompts",
    ".list tools",
    ".list mcp-servers",
    ".list bundles",
    ".vault add",
    ".vault get",
    ".vault update",
    ".vault delete",
    ".vault list",
    ".clear messages",
    ".macro enable",
    ".macro disable",
];

/// The `.set` keys whose update writes files, starts processes, prints or
/// rebuilds the tools.
const SIDE_EFFECT_SET_KEYS: &[&str] = &[
    "skills_enabled",
    "enabled_mcp_servers",
    "mcp_server_support",
    "mcp_tools.github",
    "rag_top_k",
    "rag_reranker_model",
];

/// The reasons a command that switches what the turn runs with gives.
const SWITCHING_REASONS: &[&str] = &[
    "changes the agent",
    "changes the conversation",
    "changes the model",
    "changes the RAG",
    "changes the role",
    "changes the session",
    "changes the tools the model is offered",
];

/// The first words of commands that switch what the turn runs with.
const SWITCHING_COMMANDS: &[&str] = &[
    ".agent",
    ".session",
    ".role",
    ".temp-role",
    ".model",
    ".rag",
    ".fork",
    ".undo",
    ".compress",
    ".empty",
    ".recover",
    ".exit",
    ".clear",
    ".mcp",
    ".tool",
];

fn timing(line: &str) -> Option<Timing> {
    lookup(line, CommandPrefix::Dot).map(|(_, timing)| *timing)
}

#[test]
fn every_command_has_exactly_one_timing_and_every_timing_a_command() {
    let commands: BTreeSet<&str> = repl_command_names()
        .chain([".keys"])
        .chain(DISPATCHER_ONLY.iter().copied())
        .chain(
            COMMAND_TIMINGS
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| {
                    name.strip_prefix(".set ").is_some_and(|key| {
                        SIDE_EFFECT_SET_KEYS.iter().any(|known| {
                            *known == key || (key.ends_with('.') && known.starts_with(key))
                        })
                    })
                }),
        )
        .collect();
    let mut table = BTreeSet::new();
    for (name, _) in COMMAND_TIMINGS {
        assert!(table.insert(*name), "{name} has two entries");
    }
    let unclassified: Vec<_> = commands.difference(&table).collect();
    assert!(unclassified.is_empty(), "no timing for {unclassified:?}");
    let unknown: Vec<_> = table.difference(&commands).collect();
    assert!(unknown.is_empty(), "not a command: {unknown:?}");
}

#[test]
fn every_queued_command_says_why() {
    for (name, timing) in COMMAND_TIMINGS {
        if let Timing::Queue(reason) = timing {
            assert!(!reason.trim().is_empty(), "{name} queues without a reason");
        }
    }
}

#[test]
fn the_longest_matching_entry_wins() {
    let name = |line: &str| lookup(line, CommandPrefix::Dot).map(|(name, _)| *name);
    assert_eq!(name(".info mcp-server foo"), Some(".info mcp-server"));
    assert_eq!(name(".info"), Some(".info"));
    assert_eq!(name(".info bogus"), Some(".info"));
    assert_eq!(name("  .skill   loaded  "), Some(".skill loaded"));
    assert_eq!(name(".skill my-skill"), Some(".skill"));
    assert_eq!(name(".edit role"), Some(".edit role"));
    assert_eq!(name(".edit"), None);
    assert_eq!(
        lookup("/info role", CommandPrefix::Slash).map(|(name, _)| *name),
        Some(".info role")
    );
}

#[test]
fn macros_shell_lines_and_prompts_have_no_entry() {
    for line in [".my-macro arg", "!ls -la", "explain this", ".infox"] {
        assert_eq!(lookup(line, CommandPrefix::Dot), None, "{line}");
    }
}

#[test]
fn reading_and_editor_commands_run_now_and_switches_wait() {
    for line in [
        ".help",
        ".info tools",
        ".list roles",
        ".edit config",
        ".sources rag",
    ] {
        assert_eq!(timing(line), Some(Timing::Now), "{line}");
    }
    assert_eq!(timing(".set temperature 0.2"), Some(Timing::Deferred));
    for line in [".agent coder", ".model x", ".edit role", ".vault list"] {
        assert!(matches!(timing(line), Some(Timing::Queue(_))), "{line}");
    }
}

#[test]
fn commands_that_switch_the_context_wait_for_the_turn() {
    for line in [
        ".session x",
        ".exit session",
        ".clear messages",
        ".undo",
        ".compress session",
        ".fork",
        ".model x",
        ".agent x",
        ".role x",
        ".rag x",
    ] {
        match timing(line) {
            Some(Timing::Queue(reason)) => assert!(
                SWITCHING_REASONS.contains(&reason),
                "{line} queues for {reason:?}"
            ),
            other => panic!("{line} does not queue: {other:?}"),
        }
    }
    for (name, timing) in COMMAND_TIMINGS {
        let first = name.split_whitespace().next().unwrap_or_default();
        if SWITCHING_COMMANDS.contains(&first) {
            assert!(matches!(timing, Timing::Queue(_)), "{name} does not queue");
        }
    }
}

#[test]
fn set_keys_with_side_effects_wait_and_other_settings_defer() {
    for key in SIDE_EFFECT_SET_KEYS {
        let line = format!(".set {key} x");
        let entry = lookup(&line, CommandPrefix::Dot).map(|(name, _)| *name);
        assert_ne!(entry, Some(".set"), "{line} has no entry of its own");
        assert!(matches!(timing(&line), Some(Timing::Queue(_))), "{line}");
    }
    for line in [
        ".set enabled_mcp_servers x",
        ".set rag_top_k 3",
        ".set skills_enabled false",
    ] {
        assert!(matches!(timing(line), Some(Timing::Queue(_))), "{line}");
    }
    assert_eq!(timing(".set temperature 0.5"), Some(Timing::Deferred));
    assert_eq!(timing(".set mcp_tools null"), Some(Timing::Deferred));
    assert_eq!(timing(".reasoning high"), Some(Timing::Deferred));
}

#[test]
#[serial]
fn run_now_gives_what_the_repl_prints() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-run-now");
        let mut ctx = chat_ctx();
        let sysinfo = ctx.sysinfo(&ctx.app.config).unwrap();
        assert_eq!(
            run_now(&mut ctx, ".info").await.unwrap(),
            CommandOutput::Text(sysinfo)
        );
        assert_eq!(
            run_now(&mut ctx, ".help").await.unwrap(),
            CommandOutput::Text(format!("{}\n", repl_help_text(CommandPrefix::Dot)))
        );
        let err = run_now(&mut ctx, ".info bogus")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown command"), "{err}");
    });
}

/// A setting typed during a turn changes the snapshot it is checked on,
/// and a bad one fails there as an idle `.set` would; with no value it
/// gives the REPL's usage line and nothing to apply.
#[test]
#[serial]
fn run_now_checks_a_setting_on_the_snapshot() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-run-now-setting");
        let mut snapshot = chat_ctx();
        assert_eq!(
            run_now(&mut snapshot, ".set temperature 0.5")
                .await
                .unwrap(),
            CommandOutput::Deferred
        );
        assert_eq!(snapshot.app.config.temperature, Some(0.5));
        assert!(run_now(&mut snapshot, ".set bogus 1").await.is_err());
        let CommandOutput::Text(usage) = run_now(&mut snapshot, ".set").await.unwrap() else {
            panic!("a bare .set was taken as a setting");
        };
        assert!(usage.contains("Usage: .set"), "{usage}");
    });
}

/// The replay applies each line in order and names the ones that fail.
#[test]
#[serial]
fn replaying_settings_applies_them_in_order_and_reports_failures() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-replay-deferred");
        let mut ctx = chat_ctx();
        let lines = [
            ".set temperature 0.5",
            ".set bogus 1",
            ".set temperature 0.7",
        ];
        let failed = replay_deferred(&mut ctx, lines.map(String::from).to_vec()).await;
        assert_eq!(ctx.app.config.temperature, Some(0.7));
        let failed: Vec<&str> = failed.iter().map(|(line, _)| line.as_str()).collect();
        assert_eq!(failed, [".set bogus 1"]);
    });
}
