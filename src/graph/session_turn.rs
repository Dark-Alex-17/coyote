use super::types::SessionBlock;
use super::validator::{
    CHAT_MEMORY_INLINED_TAIL, CHAT_MEMORY_UNAVAILABLE_PREFIX, is_chat_tool_denied,
};
use crate::client::{Model, ModelType};
use crate::config::{Input, RequestContext, RoleLike};
use crate::function::FunctionDeclaration;
use crate::function::memory::MEMORY_FUNCTION_PREFIX;
use anyhow::{Context, Result};
use log::warn;

/// Builds the attached input for one chat turn against a graph agent's
/// session: the session's role with the block's chat model, the block's tool
/// set, and the last graph run inlined ahead of the user's text for this
/// request only.
pub fn build_chat_input(
    ctx: &mut RequestContext,
    block: &SessionBlock,
    text: &str,
) -> Result<Input> {
    let app = ctx.app.config.clone();
    let mut role = ctx.extract_role(app.as_ref())?;
    let model = match &block.chat.model {
        Some(id) => Model::retrieve_model(app.as_ref(), id, ModelType::Chat)
            .with_context(|| format!("Unknown model '{id}' on session.chat"))?,
        None => role.model().clone(),
    };
    role.set_model(model.clone());
    let functions = chat_tool_declarations(ctx, block, &model);
    let mut input = Input::attached_with_role(ctx, text, role, functions)?;
    if let Some(run) = ctx.session.as_ref().and_then(|s| s.last_run()) {
        input.set_request_preamble(run.render());
    }
    Ok(input)
}

/// The chat turn's share of the agent's pool: the global tools named in
/// `chat.tools` plus the memory tools when `with_memory`, deduped by name and
/// in pool order. Agent-local tools stay with the graph's nodes and the
/// denied families are never listable.
pub fn chat_tool_pool(
    block: &SessionBlock,
    pool: &[FunctionDeclaration],
    with_memory: bool,
) -> Vec<FunctionDeclaration> {
    let mut selected: Vec<FunctionDeclaration> = Vec::new();
    for decl in pool {
        let listed = block.chat.tools.contains(&decl.name)
            && !decl.agent
            && !is_chat_tool_denied(&decl.name);
        let memory = with_memory && decl.name.starts_with(MEMORY_FUNCTION_PREFIX);
        if (listed || memory) && !selected.iter().any(|d| d.name == decl.name) {
            selected.push(decl.clone());
        }
    }
    selected
}

/// `chat_tool_pool` for the request's agent and memory settings, behind two
/// gates: nothing when the operator has disabled `function_calling_support`
/// globally (silently, the per-run validator pass owns that warning), and
/// nothing on a model without function calling, with a warning once per
/// RequestContext if the block would otherwise have declared something.
pub fn chat_tool_declarations(
    ctx: &mut RequestContext,
    block: &SessionBlock,
    model: &Model,
) -> Option<Vec<FunctionDeclaration>> {
    if !ctx.app.config.function_calling_support {
        return None;
    }
    let inlined = block.chat.memory && ctx.should_inject_memory();
    let pool = ctx
        .agent
        .as_ref()
        .map(|a| a.functions().declarations())
        .unwrap_or(&[]);
    let requested = chat_tool_pool(block, pool, inlined);
    if !model.supports_function_calling() {
        if !ctx.graph_chat_fc_warned && !requested.is_empty() {
            warn_tools_unavailable(block, &model.id(), inlined);
            ctx.graph_chat_fc_warned = true;
        }
        return None;
    }
    (!requested.is_empty()).then_some(requested)
}

fn warn_tools_unavailable(block: &SessionBlock, model_id: &str, memory_inlined: bool) {
    let subject = if block.chat.tools.is_empty() {
        CHAT_MEMORY_UNAVAILABLE_PREFIX
    } else {
        "session.chat.tools: tools unavailable"
    };
    let mut text = format!("Warning: {subject} on '{model_id}' (no function calling)");
    if memory_inlined {
        text.push_str(CHAT_MEMORY_INLINED_TAIL);
    }
    eprintln!("{text}");
    warn!("{text}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelData;
    use crate::config::{AppConfig, AppState, Session, WorkingMode, paths};
    use crate::graph::types::ChatBlock;
    use crate::graph::{GraphRunResult, LastRun};
    use crate::testing::{TestConfigDirGuard, install_log_collector, warn_snapshot};
    use crate::utils::create_abort_signal;
    use serial_test::serial;
    use std::collections::{HashMap, HashSet};
    use std::fs::{create_dir_all, write};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const GLOBAL_TOOL: &str = "#!/usr/bin/env bash\n# @describe Read a file.\n# @option --path! The file\nmain() { cat \"$argc_path\"; }\n";
    const AGENT_TOOLS: &str = "#!/usr/bin/env bash\n# @describe Agent-local tools\n\n# @cmd Probe the agent\nlocal_probe() { echo probe; }\n";
    const SESSION_BLOCK: &str =
        "session:\n  chat:\n    instructions: hi\n    memory: true\n    tools: [fs_read]\n";

    fn unique(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}_{nanos}")
    }

    /// A graph agent with one global tool (`fs_read`), one agent-local tool
    /// (`local_probe`) and a memory-enabled session block; the block handed
    /// to the functions under test varies per call.
    fn write_graph_agent(name: &str) {
        write_graph_agent_with_globals(name, &["fs_read"]);
    }

    /// `write_graph_agent` with every named global tool stubbed into the
    /// global tools dir and listed under `global_tools`.
    fn write_graph_agent_with_globals(name: &str, globals: &[&str]) {
        let tools_dir = paths::global_tools_dir();
        create_dir_all(&tools_dir).unwrap();
        let mut global_tools = String::new();
        for tool in globals {
            write(tools_dir.join(format!("{tool}.sh")), GLOBAL_TOOL).unwrap();
            global_tools.push_str(&format!("  - {tool}.sh\n"));
        }

        let agent_dir = paths::agent_data_dir(name);
        create_dir_all(&agent_dir).unwrap();
        write(agent_dir.join("tools.sh"), AGENT_TOOLS).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {name}\nglobal_tools:\n{global_tools}{SESSION_BLOCK}start: done\nnodes:\n  done:\n    type: end\n    output: done\n"
            ),
        )
        .unwrap();
    }

    fn write_memory_store() {
        let index = paths::global_memory_index_file();
        create_dir_all(index.parent().unwrap()).unwrap();
        write(index, "# Memory\n").unwrap();
    }

    /// The block in `write_graph_agent` lists a tool, which the engage-time
    /// validator only accepts on a chat model with function calling, so the
    /// context's own model (the agent's, absent a graph `model:`) is one.
    async fn ctx_with_agent(name: &str) -> RequestContext {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.model = chat_session("agent-model", true).model().clone();
        let app = ctx.app.config.clone();
        ctx.use_agent(&app, name, None, create_abort_signal())
            .await
            .unwrap();
        ctx
    }

    fn with_app_config(ctx: &mut RequestContext, update: impl FnOnce(&mut AppConfig)) {
        let mut config = (*ctx.app.config).clone();
        update(&mut config);
        let mut app = (*ctx.app).clone();
        app.config = Arc::new(config);
        ctx.app = Arc::new(app);
    }

    fn chat_session(model_name: &str, function_calling: bool) -> Session {
        let mut session = Session::default();
        let mut data = ModelData::new(model_name);
        data.supports_function_calling = function_calling;
        session.set_model(Model::from_config("openai", &[data]).remove(0));
        session
    }

    fn block(tools: &[&str], memory: bool) -> SessionBlock {
        SessionBlock {
            expose: vec![],
            route: None,
            chat: ChatBlock {
                model: None,
                instructions: "hi".to_string(),
                memory,
                tools: tools.iter().map(|t| t.to_string()).collect(),
            },
        }
    }

    fn names(declarations: Option<Vec<FunctionDeclaration>>) -> Vec<String> {
        declarations
            .map(|list| list.into_iter().map(|d| d.name).collect())
            .unwrap_or_default()
    }

    fn declaration(name: &str, agent: bool) -> FunctionDeclaration {
        FunctionDeclaration {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: Default::default(),
            agent,
        }
    }

    #[test]
    fn chat_tool_pool_rejects_reserved_families_and_agent_local_tools_by_name() {
        let pool = [
            declaration("fs_read", false),
            declaration("fs_write", false),
            declaration("local_probe", true),
            declaration("agent__send_message", false),
            declaration("job__start", false),
            declaration("todo__init", false),
            declaration("user__select", false),
            declaration("memory__read", false),
            declaration("memory__write", false),
        ];
        let everything: Vec<&str> = pool.iter().map(|d| d.name.as_str()).collect();
        let by_name = |selected: Vec<FunctionDeclaration>| -> Vec<String> {
            selected.into_iter().map(|d| d.name).collect()
        };

        assert_eq!(
            by_name(chat_tool_pool(&block(&everything, false), &pool, false)),
            ["fs_read", "fs_write"]
        );
        assert_eq!(
            by_name(chat_tool_pool(&block(&everything, true), &pool, true)),
            ["fs_read", "fs_write", "memory__read", "memory__write"],
            "memory__ enters once, through with_memory, never by name"
        );
        assert_eq!(
            by_name(chat_tool_pool(&block(&[], true), &pool, true)),
            ["memory__read", "memory__write"]
        );
        assert!(chat_tool_pool(&block(&["memory__read"], false), &pool, false).is_empty());
        assert!(chat_tool_pool(&block(&["nope"], false), &pool, false).is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn chat_tool_declarations_filters_the_agent_pool_by_the_block() {
        let _guard = TestConfigDirGuard::new("session-turn");
        write_memory_store();
        let name = unique("chat_tools");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session("fc-model", true));
        let model = ctx.session.as_ref().unwrap().model().clone();
        let pool: Vec<String> = ctx
            .agent
            .as_ref()
            .unwrap()
            .functions()
            .declarations()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for expected in ["fs_read", "local_probe", "user__select", "memory__read"] {
            assert!(pool.contains(&expected.to_string()), "pool: {pool:?}");
        }

        assert!(chat_tool_declarations(&mut ctx, &block(&[], false), &model).is_none());
        assert!(
            chat_tool_declarations(&mut ctx, &block(&["user__select"], false), &model).is_none()
        );
        assert!(
            chat_tool_declarations(&mut ctx, &block(&["memory__read"], false), &model).is_none(),
            "memory tools are reachable only through chat.memory"
        );

        let listed = chat_tool_declarations(&mut ctx, &block(&["fs_read"], false), &model);
        assert_eq!(names(listed), ["fs_read"]);

        let with_local =
            chat_tool_declarations(&mut ctx, &block(&["fs_read", "local_probe"], false), &model);
        assert_eq!(
            names(with_local),
            ["fs_read"],
            "agent-local tools belong to the graph's nodes, not the chat"
        );

        let with_memory = names(chat_tool_declarations(
            &mut ctx,
            &block(&["fs_read"], true),
            &model,
        ));
        assert_eq!(with_memory[0], "fs_read");
        assert!(
            with_memory.contains(&"memory__read".to_string()),
            "{with_memory:?}"
        );
        assert!(
            with_memory
                .iter()
                .skip(1)
                .all(|n| n.starts_with(MEMORY_FUNCTION_PREFIX)),
            "{with_memory:?}"
        );

        let named_and_enabled = names(chat_tool_declarations(
            &mut ctx,
            &block(&["memory__read"], true),
            &model,
        ));
        assert_eq!(
            named_and_enabled,
            with_memory[1..].to_vec(),
            "naming memory__read adds nothing and duplicates nothing"
        );
        assert!(!ctx.graph_chat_fc_warned);
    }

    #[tokio::test]
    #[serial]
    async fn chat_tool_declarations_drops_everything_on_a_non_fc_model_and_warns_once() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("session-turn");
        write_memory_store();
        let name = unique("chat_nofc");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent(&name).await;
        let model_name = unique("plain-model");
        ctx.session = Some(chat_session(&model_name, false));
        let model = ctx.session.as_ref().unwrap().model().clone();
        let warnings = || -> Vec<String> {
            warn_snapshot()
                .into_iter()
                .filter(|m| m.contains(&model_name))
                .collect()
        };

        assert!(chat_tool_declarations(&mut ctx, &block(&[], false), &model).is_none());
        assert!(
            warnings().is_empty(),
            "nothing requested, nothing to warn about"
        );

        let requested = block(&["fs_read"], true);
        assert!(chat_tool_declarations(&mut ctx, &requested, &model).is_none());
        assert!(chat_tool_declarations(&mut ctx, &requested, &model).is_none());

        let warnings_after_two_calls = warnings();
        assert_eq!(
            warnings_after_two_calls.len(),
            1,
            "{warnings_after_two_calls:?}"
        );
        assert_eq!(
            warnings_after_two_calls[0],
            format!(
                "Warning: session.chat.tools: tools unavailable on 'openai:{model_name}' (no function calling); the MEMORY.md section is inlined with the without-tools cap"
            )
        );
        assert!(warnings_after_two_calls[0].is_ascii());

        let mut fresh = ctx_with_agent(&name).await;
        fresh.session = Some(chat_session(&model_name, false));
        assert!(chat_tool_declarations(&mut fresh, &block(&[], true), &model).is_none());
        let warnings_after_fresh_ctx = warnings();
        assert_eq!(
            warnings_after_fresh_ctx.len(),
            2,
            "{warnings_after_fresh_ctx:?}"
        );
        assert_eq!(
            warnings_after_fresh_ctx[1],
            format!(
                "Warning: session.chat.memory: memory tools unavailable on 'openai:{model_name}' (no function calling); the MEMORY.md section is inlined with the without-tools cap"
            )
        );

        let mut tools_only = ctx_with_agent(&name).await;
        tools_only.session = Some(chat_session(&model_name, false));
        assert!(
            chat_tool_declarations(&mut tools_only, &block(&["fs_read"], false), &model).is_none()
        );
        let warnings_after_tools_only = warnings();
        assert_eq!(
            warnings_after_tools_only.len(),
            3,
            "{warnings_after_tools_only:?}"
        );
        assert_eq!(
            warnings_after_tools_only[2],
            format!(
                "Warning: session.chat.tools: tools unavailable on 'openai:{model_name}' (no function calling)"
            ),
            "no MEMORY.md sentence when the block did not ask for memory"
        );
    }

    /// Memory off through the config cascade (the `--no-memory` path) keeps
    /// MEMORY.md out of the prompt whatever the block says, so the warning
    /// must not claim otherwise, and a block whose requests all filter out
    /// has nothing to warn about.
    #[tokio::test]
    #[serial]
    async fn chat_tool_declarations_warns_only_for_what_would_have_been_declared() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("session-turn");
        let name = unique("chat_nomem");
        write_graph_agent(&name);
        let model_name = unique("plain-model");
        let model = chat_session(&model_name, false).model().clone();
        let warnings = || -> Vec<String> {
            warn_snapshot()
                .into_iter()
                .filter(|m| m.contains(&model_name))
                .collect()
        };
        let mut ctx = ctx_with_agent(&name).await;
        with_app_config(&mut ctx, |app| app.memory = Some(false));
        assert!(!ctx.should_inject_memory());

        assert!(chat_tool_declarations(&mut ctx, &block(&[], true), &model).is_none());
        assert!(
            chat_tool_declarations(&mut ctx, &block(&["user__select"], true), &model).is_none()
        );
        assert!(
            warnings().is_empty(),
            "memory off and a reserved tool: nothing would have been declared"
        );
        assert!(!ctx.graph_chat_fc_warned);

        assert!(chat_tool_declarations(&mut ctx, &block(&["fs_read"], true), &model).is_none());
        let warnings_after_tools = warnings();
        assert_eq!(
            warnings_after_tools,
            [format!(
                "Warning: session.chat.tools: tools unavailable on 'openai:{model_name}' (no function calling)"
            )],
            "no MEMORY.md sentence when memory is not actually inlined"
        );
    }

    #[tokio::test]
    #[serial]
    async fn chat_tool_declarations_declares_nothing_when_function_calling_is_off_globally() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("session-turn");
        write_memory_store();
        let name = unique("chat_fcoff");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent(&name).await;
        with_app_config(&mut ctx, |app| app.function_calling_support = false);
        let fc_name = unique("fc-model");
        let fc_model = chat_session(&fc_name, true).model().clone();
        let plain_name = unique("plain-model");
        let plain_model = chat_session(&plain_name, false).model().clone();
        let requested = block(&["fs_read"], true);

        assert!(chat_tool_declarations(&mut ctx, &requested, &fc_model).is_none());
        assert!(chat_tool_declarations(&mut ctx, &requested, &plain_model).is_none());
        assert!(
            warn_snapshot()
                .iter()
                .all(|m| !m.contains(&fc_name) && !m.contains(&plain_name)),
            "the global toggle wins silently, before the model capability check"
        );
        assert!(!ctx.graph_chat_fc_warned);
    }

    #[tokio::test]
    #[serial]
    async fn build_chat_input_inlines_the_last_run_for_the_request_only() {
        let _guard = TestConfigDirGuard::new("session-turn");
        let name = unique("chat_input");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session("fc-model", true));
        let block = block(&["fs_read"], false);

        let input = build_chat_input(&mut ctx, &block, "what happened?").unwrap();
        assert!(input.with_session());
        assert_eq!(input.message_content().to_text(), "what happened?");
        assert_eq!(input.role().model().id(), "openai:fc-model");
        assert_eq!(
            input.declared_function_names(),
            HashSet::from(["fs_read".to_string()])
        );

        let result = GraphRunResult {
            output: "final answer".to_string(),
            final_state: HashMap::new(),
            trace: vec![],
        };
        let run = LastRun::capture(&result, "do the thing", &[]);
        let rendered = run.render();
        ctx.session.as_mut().unwrap().set_last_run(Some(run));

        let input = build_chat_input(&mut ctx, &block, "what happened?").unwrap();
        assert_eq!(
            input.message_content().to_text(),
            format!("{}\n\nwhat happened?", rendered.trim_end())
        );
        assert_eq!(input.text(), "what happened?");
    }

    #[tokio::test]
    #[serial]
    async fn build_chat_input_declares_listed_write_and_exec_globals() {
        let _guard = TestConfigDirGuard::new("session-turn");
        let name = unique("chat_write_exec");
        write_graph_agent_with_globals(&name, &["fs_read", "fs_write", "execute_command"]);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session("fc-model", true));
        let block = block(&["fs_write", "execute_command"], false);

        let input = build_chat_input(&mut ctx, &block, "edit it").unwrap();

        assert_eq!(
            input.declared_function_names(),
            HashSet::from(["fs_write".to_string(), "execute_command".to_string()])
        );
    }

    /// A chat turn typed as nothing but a preamble (a bare `.regenerate`
    /// or an empty follow-up) still has to send a user message.
    #[tokio::test]
    #[serial]
    async fn build_chat_input_with_empty_text_still_sends_the_preamble_as_the_user_message() {
        let _guard = TestConfigDirGuard::new("session-turn");
        let name = unique("chat_empty");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session("fc-model", true));
        let result = GraphRunResult {
            output: "final answer".to_string(),
            final_state: HashMap::new(),
            trace: vec![],
        };
        let run = LastRun::capture(&result, "do the thing", &[]);
        ctx.session.as_mut().unwrap().set_last_run(Some(run));

        let input = build_chat_input(&mut ctx, &block(&[], false), "").unwrap();

        let content = input.message_content().to_text();
        assert!(
            content.starts_with("Last graph run (completed "),
            "{content}"
        );
        let messages = input.role().build_messages(&input);
        let user = messages.iter().rev().find(|m| m.role.is_user()).unwrap();
        let user_text = user.content.to_text();
        assert!(!user_text.is_empty());
        assert!(
            user_text.starts_with("Last graph run (completed "),
            "{user_text}"
        );
        assert_eq!(input.text(), "");
        assert_eq!(input.bare_text(), "");
    }

    /// The test binary seeds a `test-seeded` client (see the model registry
    /// seeding in request_context's tests), so any `test-seeded:<name>` chat
    /// id resolves through the create-from-name fallback.
    #[tokio::test]
    #[serial]
    async fn build_chat_input_uses_the_block_model_over_the_session_model() {
        let _guard = TestConfigDirGuard::new("session-turn");
        let name = unique("chat_model");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session("fc-model", true));
        let mut block = block(&[], false);
        block.chat.model = Some("test-seeded:other-model".to_string());

        let input = build_chat_input(&mut ctx, &block, "hi").unwrap();
        assert_eq!(input.role().model().id(), "test-seeded:other-model");
        assert_eq!(
            ctx.session.as_ref().unwrap().model().id(),
            "openai:fc-model",
            "the override is per request, the session keeps its model"
        );
    }

    #[tokio::test]
    #[serial]
    async fn build_chat_input_rejects_an_unknown_chat_model() {
        let _guard = TestConfigDirGuard::new("session-turn");
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.session = Some(Session::default());
        let mut block = block(&[], false);
        block.chat.model = Some("nope:nope".to_string());

        let err = build_chat_input(&mut ctx, &block, "hi").unwrap_err();
        assert!(
            err.to_string()
                .contains("Unknown model 'nope:nope' on session.chat"),
            "{err:#}"
        );
    }
}
