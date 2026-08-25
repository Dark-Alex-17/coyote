use super::{REPL_COMMANDS, ReplCommand};

use crate::config::{McpPromptCompletion, RequestContext};
use crate::mcp::ConnectedServer;
use crate::utils::fuzzy_filter;

use parking_lot::RwLock;
use reedline::{Completer, Span, Suggestion};
use rmcp::model::Prompt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const PROMPT_COMPLETION_RPC_TIMEOUT: Duration = Duration::from_secs(2);

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
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
        let (cmd, cmd_start) = parts[0];

        if !cmd.starts_with('.') {
            return suggestions;
        }

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

        let command_filter = parts
            .iter()
            .take(2)
            .map(|(v, _)| *v)
            .collect::<Vec<&str>>()
            .join(" ");
        let commands: Vec<_> = self
            .commands
            .iter()
            .filter(|cmd| {
                cmd.is_valid(state)
                    && (command_filter.len() == 1 || cmd.name.starts_with(&command_filter[..2]))
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
                let name = cmd.name;
                let description = cmd.description;
                let has_group = self.groups.get(name).map(|v| *v > 1).unwrap_or_default();
                let name = if has_group {
                    name.to_string()
                } else {
                    format!("{name} ")
                };
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
                    &format!("{name} "),
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
            .map(|prompt| (prompt.name, prompt.description))
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
                let description = match (arg.required == Some(true), arg.description) {
                    (true, Some(description)) => Some(format!("{description} (required)")),
                    (true, None) => Some("(required)".to_string()),
                    (false, description) => description,
                };
                (format!("{}=", arg.name), description)
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
