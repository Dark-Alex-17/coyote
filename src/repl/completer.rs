use super::{REPL_COMMANDS, ReplCommand};

use crate::config::{McpPromptCompletion, RequestContext, sanitize_display_text};
use crate::mcp::ConnectedServer;
use crate::utils::fuzzy_filter;

use parking_lot::RwLock;
use reedline::{Completer, CompletionResult, Span, Suggestion};
use rmcp::model::Prompt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const PROMPT_COMPLETION_RPC_TIMEOUT: Duration = Duration::from_secs(2);

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        CompletionResult::fresh(self.suggestions_for(line, pos))
    }
}

impl ReplCompleter {
    fn suggestions_for(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let mut suggestions = vec![];
        let line = &line[0..pos];
        let mut parts = split_line(line);
        if parts.is_empty() {
            return suggestions;
        }
        if parts[0].0 == r#":::"# {
            parts.remove(0);
        }

        let parts_len = parts.len();
        if parts_len == 0 {
            return suggestions;
        }
        let (typed_cmd, cmd_start) = parts[0];

        let prefix = self.ctx.read().command_prefix();
        let Some(cmd) = prefix.normalize(typed_cmd) else {
            return suggestions;
        };
        let cmd = cmd.as_ref();
        let sigil = typed_cmd.chars().next().unwrap_or('.');

        if cmd == ".prompt" && parts_len > 1 {
            let span = Span::new(parts[parts_len - 1].1, pos);
            let args: Vec<&str> = parts.iter().skip(1).map(|(v, _)| *v).collect();
            let filter = args.last().copied().unwrap_or_default().to_string();
            let stage = {
                let ctx = self.ctx.read();
                ctx.mcp_prompt_completion(&args)
            };
            return complete_prompt_stage(stage, &filter, PROMPT_COMPLETION_RPC_TIMEOUT)
                .iter()
                .map(|(value, description)| {
                    create_suggestion(value, description.as_deref().unwrap_or_default(), span)
                })
                .collect();
        }

        let ctx = self.ctx.read();
        let state = ctx.state();
        let model_has_reasoning = !ctx.current_model().reasoning_levels().is_empty();

        let command_filter = std::iter::once(cmd)
            .chain(parts.iter().skip(1).take(1).map(|(v, _)| *v))
            .collect::<Vec<&str>>()
            .join(" ");
        let commands: Vec<_> = self
            .commands
            .iter()
            .filter(|cmd| {
                cmd.is_valid(state)
                    && (command_filter.len() == 1
                        || command_filter
                            .get(..2)
                            .is_none_or(|head| cmd.name.starts_with(head)))
                    && (cmd.name != ".reasoning" || model_has_reasoning)
            })
            .collect();
        let commands = fuzzy_filter(commands, |v| v.name, &command_filter);

        if parts_len > 1 {
            let span = Span::new(parts[parts_len - 1].1, pos);
            let args_line = &line[parts[1].1..];
            let args: Vec<&str> = parts.iter().skip(1).map(|(v, _)| *v).collect();
            suggestions.extend(ctx.repl_complete(cmd, &args, args_line).iter().map(
                |(value, description)| {
                    let description = description.as_deref().unwrap_or_default();
                    create_suggestion(value, description, span)
                },
            ))
        }

        if suggestions.is_empty() {
            let span = Span::new(cmd_start, pos);
            suggestions.extend(commands.iter().map(|cmd| {
                let name = with_sigil(cmd.name, sigil);
                let description = cmd.description;
                let has_group = self
                    .groups
                    .get(cmd.name)
                    .map(|v| *v > 1)
                    .unwrap_or_default();
                let name = if has_group { name } else { format!("{name} ") };
                create_suggestion(&name, description, span)
            }));

            let macros: Vec<(String, Option<String>)> = ctx
                .visible_macro_completions()
                .into_iter()
                .map(|(name, description)| (format!(".{name}"), description))
                .filter(|(name, _)| {
                    command_filter.len() == 1
                        || name.starts_with(command_filter.get(..2).unwrap_or(&command_filter))
                })
                .collect();
            let macros = fuzzy_filter(macros, |(name, _)| name.as_str(), &command_filter);
            suggestions.extend(macros.iter().map(|(name, description)| {
                create_suggestion(
                    &format!("{} ", with_sigil(name, sigil)),
                    description.as_deref().unwrap_or_default(),
                    span,
                )
            }));
        }
        suggestions
    }
}

pub struct ReplCompleter {
    ctx: Arc<RwLock<RequestContext>>,
    commands: Vec<ReplCommand>,
    groups: HashMap<&'static str, usize>,
}

impl ReplCompleter {
    pub fn new(ctx: Arc<RwLock<RequestContext>>) -> Self {
        let mut groups = HashMap::new();

        let commands: Vec<ReplCommand> = REPL_COMMANDS.to_vec();

        for cmd in REPL_COMMANDS.iter() {
            let name = cmd.name;
            *groups.entry(name).or_insert(0) += 1;
        }

        Self {
            ctx,
            commands,
            groups,
        }
    }
}

fn with_sigil(name: &str, sigil: char) -> String {
    format!("{sigil}{}", &name[1..])
}

fn create_suggestion(value: &str, description: &str, span: Span) -> Suggestion {
    let description = if description.is_empty() {
        None
    } else {
        Some(description.to_string())
    };
    Suggestion {
        display_override: None,
        value: value.to_string(),
        description,
        style: None,
        extra: None,
        span,
        append_whitespace: false,
        match_indices: None,
    }
}

fn complete_prompt_stage(
    stage: McpPromptCompletion,
    filter: &str,
    rpc_timeout: Duration,
) -> Vec<(String, Option<String>)> {
    let values = match stage {
        McpPromptCompletion::Ready(values) => values,
        McpPromptCompletion::PromptNames { server } => list_prompts_blocking(server, rpc_timeout)
            .unwrap_or_default()
            .into_iter()
            .map(|prompt| {
                (
                    sanitize_display_text(&prompt.name),
                    prompt
                        .description
                        .map(|description| sanitize_display_text(&description)),
                )
            })
            .collect(),
        McpPromptCompletion::ArgumentKeys {
            server,
            prompt,
            typed_keys,
        } => list_prompts_blocking(server, rpc_timeout)
            .unwrap_or_default()
            .into_iter()
            .find(|candidate| candidate.name == prompt)
            .and_then(|candidate| candidate.arguments)
            .unwrap_or_default()
            .into_iter()
            .filter(|arg| !typed_keys.contains(&arg.name))
            .map(|arg| {
                let description = arg
                    .description
                    .map(|description| sanitize_display_text(&description));
                let description = match (arg.required == Some(true), description) {
                    (true, Some(description)) => Some(format!("{description} (required)")),
                    (true, None) => Some("(required)".to_string()),
                    (false, description) => description,
                };
                (
                    format!("{}=", sanitize_display_text(&arg.name)),
                    description,
                )
            })
            .collect(),
    };
    fuzzy_filter(values, |(value, _)| value.as_str(), filter)
}

fn list_prompts_blocking(
    server: Arc<ConnectedServer>,
    rpc_timeout: Duration,
) -> Option<Vec<Prompt>> {
    let fut = async move { tokio::time::timeout(rpc_timeout, server.list_all_prompts()).await };
    // block_in_place is only sound because the REPL's read_line runs inside the
    // main-thread block_on of the multi-thread runtime.
    let result = match tokio::runtime::Handle::try_current().ok() {
        Some(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        None => tokio::runtime::Runtime::new().ok()?.block_on(fut),
    };

    result.ok()?.ok()
}

fn split_line(line: &str) -> Vec<(&str, usize)> {
    let mut parts = vec![];
    let mut part_start = None;
    for (i, ch) in line.char_indices() {
        if ch == ' ' {
            if let Some(s) = part_start {
                parts.push((&line[s..i], s));
                part_start = None;
            }
        } else if part_start.is_none() {
            part_start = Some(i)
        }
    }
    if let Some(s) = part_start {
        parts.push((&line[s..], s));
    } else {
        parts.push(("", line.len()))
    }
    parts
}

#[test]
fn test_split_line() {
    assert_eq!(split_line(".role coder"), vec![(".role", 0), ("coder", 6)],);
    assert_eq!(
        split_line(" .role   coder"),
        vec![(".role", 1), ("coder", 9)],
    );
    assert_eq!(
        split_line(".set highlight "),
        vec![(".set", 0), ("highlight", 5), ("", 15)],
    );
    assert_eq!(
        split_line(".set highlight t"),
        vec![(".set", 0), ("highlight", 5), ("t", 15)],
    );
}

#[cfg(test)]
mod prefix_tests {
    use super::*;
    use crate::config::{AppConfig, AppState, CommandPrefix, WorkingMode};
    use crate::testing::TestConfigDirGuard;
    use crate::utils::create_abort_signal;
    use serial_test::serial;

    fn values(prefix: CommandPrefix, line: &str) -> Vec<String> {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            command_prefix: prefix,
            ..Default::default()
        });
        let ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
        let mut completer = ReplCompleter::new(Arc::new(RwLock::new(ctx)));
        completer
            .suggestions_for(line, line.len())
            .into_iter()
            .map(|s| s.value)
            .collect()
    }

    #[test]
    fn slash_token_completes_with_slash_under_slash_and_both() {
        for prefix in [CommandPrefix::Slash, CommandPrefix::Both] {
            let values = values(prefix, "/in");
            assert!(
                values.contains(&"/info ".to_string()),
                "{prefix}: {values:?}"
            );
            assert!(
                values.iter().all(|v| v.starts_with('/')),
                "{prefix}: {values:?}"
            );
        }
    }

    #[test]
    fn dot_token_keeps_dot_under_both() {
        let values = values(CommandPrefix::Both, ".in");
        assert!(values.contains(&".info ".to_string()), "{values:?}");
    }

    #[test]
    fn unaccepted_sigil_is_not_completed() {
        assert!(values(CommandPrefix::Dot, "/in").is_empty());
        assert!(values(CommandPrefix::Slash, ".in").is_empty());
    }

    #[test]
    fn multibyte_second_char_does_not_panic() {
        assert!(values(CommandPrefix::Dot, ".\u{e9}").is_empty());
        assert!(values(CommandPrefix::Slash, "/\u{e9}").is_empty());
    }

    #[test]
    fn usage_probe_argument_stage_reaches_repl_complete_through_a_slash_token() {
        // Second-stage suggestions (keys, then values) must come back for a
        // `/`-typed command exactly as they do for the `.` form: the typed
        // token is normalized before `repl_complete` sees it.
        for prefix in [CommandPrefix::Slash, CommandPrefix::Both] {
            let keys = values(prefix, "/set comm");
            assert!(
                keys.iter().any(|v| v.trim_end() == "command_prefix"),
                "{prefix}: {keys:?}"
            );
            let names: Vec<String> = values(prefix, "/set command_prefix ")
                .into_iter()
                .map(|v| v.trim_end().to_string())
                .collect();
            assert_eq!(names, ["dot", "slash", "both"], "{prefix}");
        }
        // The dot form keeps working under both.
        let names: Vec<String> = values(CommandPrefix::Both, ".set command_prefix ")
            .into_iter()
            .map(|v| v.trim_end().to_string())
            .collect();
        assert_eq!(names, ["dot", "slash", "both"]);
        // ...and a sigil the prefix rejects yields no argument completions
        // either, so a chat line never grows command suggestions.
        assert!(values(CommandPrefix::Dot, "/set command_prefix ").is_empty());
        assert!(values(CommandPrefix::Slash, ".set command_prefix ").is_empty());
    }

    #[test]
    fn usage_probe_multiline_marker_keeps_the_typed_sigil() {
        let values = values(CommandPrefix::Slash, "::: /in");
        assert!(values.contains(&"/info ".to_string()), "{values:?}");
        assert!(values.iter().all(|v| v.starts_with('/')), "{values:?}");
    }

    #[test]
    fn usage_probe_multiword_commands_swap_only_the_leading_sigil() {
        // `.info tools` and friends are distinct entries: only the first
        // character changes, the rest of the name and the trailing space
        // that every ungrouped suggestion carries survive the swap.
        let values = values(CommandPrefix::Slash, "/inf");
        assert!(values.contains(&"/info ".to_string()), "{values:?}");
        assert!(values.contains(&"/info tools ".to_string()), "{values:?}");
        assert!(
            values.contains(&"/info mcp-server ".to_string()),
            "{values:?}"
        );
        assert!(values.iter().all(|v| v.starts_with('/')), "{values:?}");
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn usage_probe_completer_follows_a_live_set_command_prefix_switch() {
        // One completer, built once under dot, must track the prefix the
        // user switches to mid-session: the sigil is read on every call,
        // never captured at construction.
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            command_prefix: CommandPrefix::Dot,
            ..Default::default()
        });
        let ctx = Arc::new(RwLock::new(RequestContext::new(
            Arc::new(app),
            WorkingMode::Repl,
        )));
        let mut completer = ReplCompleter::new(Arc::clone(&ctx));
        let suggest = |completer: &mut ReplCompleter, line: &str| -> Vec<String> {
            completer
                .suggestions_for(line, line.len())
                .into_iter()
                .map(|s| s.value)
                .collect()
        };

        assert!(suggest(&mut completer, "/in").is_empty());
        assert!(suggest(&mut completer, ".in").contains(&".info ".to_string()));

        block_on(
            ctx.write()
                .update("command_prefix slash", create_abort_signal()),
        )
        .unwrap();

        let values = suggest(&mut completer, "/in");
        assert!(values.contains(&"/info ".to_string()), "{values:?}");
        assert!(
            suggest(&mut completer, ".in").is_empty(),
            "a dot token is chat under slash and must not grow suggestions"
        );

        // Switching back through the slash form restores the dot suggestions.
        block_on(
            ctx.write()
                .update("command_prefix dot", create_abort_signal()),
        )
        .unwrap();
        assert!(suggest(&mut completer, "/in").is_empty());
        assert!(suggest(&mut completer, ".in").contains(&".info ".to_string()));
    }

    #[test]
    #[serial]
    fn usage_probe_macro_suggestions_carry_the_typed_sigil() {
        // Macros are custom commands: under slash, `/pro<TAB>` must offer the
        // enabled macro as `/probe-macro ` while the dot form stays chat;
        // under both, either sigil is kept as typed.
        let _guard = TestConfigDirGuard::new("completer-prefix-macros");
        let path = crate::config::paths::macro_file("probe-macro");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "isolated: false\nsteps:\n  - \".info\"\n").unwrap();

        let slash = values(CommandPrefix::Slash, "/pro");
        assert!(
            slash.contains(&"/probe-macro ".to_string()),
            "slash: {slash:?}"
        );
        assert!(slash.iter().all(|v| v.starts_with('/')), "slash: {slash:?}");
        assert!(values(CommandPrefix::Slash, ".pro").is_empty());

        let both_slash = values(CommandPrefix::Both, "/pro");
        assert!(
            both_slash.contains(&"/probe-macro ".to_string()),
            "both: {both_slash:?}"
        );
        let both_dot = values(CommandPrefix::Both, ".pro");
        assert!(
            both_dot.contains(&".probe-macro ".to_string()),
            "both: {both_dot:?}"
        );

        let dot = values(CommandPrefix::Dot, ".pro");
        assert!(dot.contains(&".probe-macro ".to_string()), "dot: {dot:?}");
        assert!(values(CommandPrefix::Dot, "/pro").is_empty());
    }
}

#[cfg(test)]
mod prompt_completion_tests {
    use super::*;
    use crate::config::test_fixtures::{FixtureServer, fixture_runtime};
    use std::sync::atomic::Ordering;

    fn prompts_fixture() -> FixtureServer {
        FixtureServer {
            prompts_capability: true,
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_two_lists_prompt_names_with_descriptions() {
        let (runtime, _server) = fixture_runtime(prompts_fixture()).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::PromptNames { server },
            "",
            Duration::from_secs(2),
        );

        assert_eq!(
            values,
            vec![(
                "summarize".to_string(),
                Some("Summarize a document".to_string())
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_three_suggests_argument_keys_with_required_marker() {
        let (runtime, _server) = fixture_runtime(prompts_fixture()).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::ArgumentKeys {
                server,
                prompt: "summarize".to_string(),
                typed_keys: vec![],
            },
            "",
            Duration::from_secs(2),
        );

        assert_eq!(
            values,
            vec![
                (
                    "path=".to_string(),
                    Some("Document path (required)".to_string())
                ),
                ("style=".to_string(), None),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_three_excludes_typed_keys_and_fuzzy_filters() {
        let (runtime, _server) = fixture_runtime(prompts_fixture()).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::ArgumentKeys {
                server: Arc::clone(&server),
                prompt: "summarize".to_string(),
                typed_keys: vec!["path".to_string()],
            },
            "",
            Duration::from_secs(2),
        );
        assert_eq!(values, vec![("style=".to_string(), None)]);

        let values = complete_prompt_stage(
            McpPromptCompletion::ArgumentKeys {
                server,
                prompt: "summarize".to_string(),
                typed_keys: vec![],
            },
            "sty",
            Duration::from_secs(2),
        );
        assert_eq!(values, vec![("style=".to_string(), None)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_three_unknown_prompt_is_empty() {
        let (runtime, _server) = fixture_runtime(prompts_fixture()).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::ArgumentKeys {
                server,
                prompt: "ghost".to_string(),
                typed_keys: vec![],
            },
            "",
            Duration::from_secs(2),
        );

        assert!(values.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hostile_prompt_strings_are_sanitized_in_suggestions() {
        let fixture = FixtureServer {
            hostile_prompt: true,
            ..prompts_fixture()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::PromptNames {
                server: Arc::clone(&server),
            },
            "evil",
            Duration::from_secs(2),
        );
        assert_eq!(
            values,
            vec![(
                "summarize-evil".to_string(),
                Some("Runs hostile text".to_string())
            )]
        );

        let values = complete_prompt_stage(
            McpPromptCompletion::ArgumentKeys {
                server,
                prompt: "sum\u{1b}[31mmarize-evil".to_string(),
                typed_keys: vec![],
            },
            "",
            Duration::from_secs(2),
        );
        assert_eq!(
            values,
            vec![("path=".to_string(), Some("Doc path (required)".to_string()))]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_listing_times_out_to_empty() {
        let fixture = FixtureServer {
            prompt_delay: Some(Duration::from_millis(200)),
            ..prompts_fixture()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::PromptNames { server },
            "",
            Duration::from_millis(20),
        );

        assert!(values.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_listing_is_swallowed_without_retry() {
        let fixture = FixtureServer {
            fail_prompt_listings: true,
            ..prompts_fixture()
        };
        let list_prompts_calls = Arc::clone(&fixture.list_prompts_calls);
        let (runtime, _server) = fixture_runtime(fixture).await;
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::PromptNames { server },
            "",
            Duration::from_secs(2),
        );

        assert!(values.is_empty());
        assert_eq!(list_prompts_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bridge_without_ambient_runtime_uses_fallback_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (runtime, _server) = rt.block_on(fixture_runtime(prompts_fixture()));
        let server = runtime.get("fixture").cloned().unwrap();

        let values = complete_prompt_stage(
            McpPromptCompletion::PromptNames { server },
            "",
            Duration::from_secs(2),
        );

        assert_eq!(
            values,
            vec![(
                "summarize".to_string(),
                Some("Summarize a document".to_string())
            )]
        );
    }
}
