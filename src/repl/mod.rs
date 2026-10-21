mod completer;
mod highlighter;
mod prompt;
mod replay;

use self::completer::ReplCompleter;
use self::highlighter::ReplHighlighter;
use self::prompt::ReplPrompt;

use crate::client::{
    Message, MessageRole, call_chat_completions, call_chat_completions_streaming,
    call_chat_completions_streaming_quiet, init_client, oauth,
};
use crate::config::{
    AgentVariables, AppConfig, AssertState, CommandPrefix, Input, LastMessage, MacroState,
    RequestContext, Session, SessionScope, StateFlags, TURN_INTERRUPTED_TEXT,
    flatten_prompt_messages, macro_execute, resolve_prompt_args, sanitize_display_text,
};
use crate::config::{AssetCategory, paths};
use crate::function::agents::{GuardrailAction, check_pending_tasks_guardrail, pending_tasks};
use crate::graph::dispatch::{GraphTurn, run_graph_turn};
use crate::graph::llm::{CompletionFuture, CompletionRunner};
use crate::graph::route::Branch;
use crate::hooks::{self, HookEvent};
use crate::render::render_error;
use crate::supervisor::{Supervisor, TaskKind};
use crate::utils::{
    AbortSignal, SHELL, TaskCancelGuard, abortable_run_with_spinner, create_abort_signal,
    dimmed_text, drain_stale_tty_input, resync_cursor_queries, run_command, set_text, temp_file,
    wait_user_interrupt,
};

use crate::sandbox::SANDBOX_ENV_FLAG;
use crate::{config, graph, mcp, resolve_oauth_client};
use anyhow::{Context, Error, Result, bail};
use crossterm::cursor::SetCursorStyle;
use fancy_regex::Regex;
use indoc::indoc;
use inquire::Text;
use log::warn;
use parking_lot::RwLock;
use reedline::CursorConfig;
use reedline::{
    ColumnarMenu, EditCommand, EditMode, Emacs, FileBackedHistory, KeyCode, KeyModifiers,
    Keybindings, Reedline, ReedlineEvent, ReedlineMenu, ValidationResult, Validator, Vi,
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    default_vi_visual_keybindings,
};
use reedline::{MenuBuilder, Signal};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::LazyLock;
use std::time::Duration;
use std::{env, process, sync::Arc};
use tokio::task;

const MENU_NAME: &str = "completion_menu";

/// Bounded wait applied at process-exit seams (REPL teardown, headless and
/// shell-execute exits) so already-fired exit hooks — `session.ended`, the
/// final `turn.*` / `agent.*` events — finish spawning before the process
/// tears down. Never used mid-session.
pub(crate) const EXIT_HOOK_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

const CANCELLED_TASKS_HINT: &str =
    "Cancelled the background task(s) left running by the interrupted turn.";

/// Brackets one user-visible turn at an outer boundary (`ask`,
/// `start_directive`, `shell_execute`): `enter` fires `turn.started`, and
/// `finish` fires exactly one of `turn.completed` / `turn.interrupted` /
/// `turn.failed`. The interrupted-vs-failed discrimination lives here and
/// nowhere else: a ctrl-c abort wins over the result, then an error means
/// failed (carrying `COYOTE_ERROR`), otherwise completed. The finishing
/// event carries `COYOTE_GRAPH_BRANCH` / `COYOTE_GRAPH_ROUTE` when the turn
/// was routed through a graph agent's session block; `turn.started` fires
/// before classification and cannot. A chat branch under a graph agent
/// engaged with a `chat.agent` adds `COYOTE_GRAPH_CHAT_AGENT` naming that
/// agent, the one `chat_agent()` answers with for the block, whether or not
/// the request was delegated to it; the turn events themselves stay the
/// graph agent's, which is back in the agent slot by the time they fire.
/// Tool rounds, auto-continue, and compression retries run inside one
/// bracket and never re-fire turn events.
pub(crate) struct TurnBracket(());

impl TurnBracket {
    #[must_use]
    pub(crate) fn enter(ctx: &RequestContext) -> Self {
        hooks::fire(HookEvent::TurnStarted, ctx, &hooks::role_extras(ctx), None);
        Self(())
    }

    pub(crate) fn finish<T>(
        self,
        ctx: &RequestContext,
        abort_signal: &AbortSignal,
        result: Result<T>,
    ) -> Result<T> {
        let mut extras = hooks::role_extras(ctx);
        if let Some((branch, forced)) = ctx.last_graph_branch {
            let branch_name = match branch {
                Branch::Run => "run",
                Branch::Chat => "chat",
            };
            let route = if forced { "forced" } else { "classified" };
            extras.push(("COYOTE_GRAPH_BRANCH", branch_name.to_string()));
            extras.push(("COYOTE_GRAPH_ROUTE", route.to_string()));
            if branch == Branch::Chat
                && let Some(delegate) = ctx.delegate.as_ref()
            {
                extras.push(("COYOTE_GRAPH_CHAT_AGENT", delegate.name.clone()));
            }
        }
        if abort_signal.aborted_ctrlc() {
            hooks::fire(HookEvent::TurnInterrupted, ctx, &extras, None);
        } else {
            match &result {
                Ok(_) => hooks::fire(HookEvent::TurnCompleted, ctx, &extras, None),
                Err(err) => {
                    extras.push(("COYOTE_ERROR", format!("{err:#}")));
                    hooks::fire(HookEvent::TurnFailed, ctx, &extras, None);
                }
            }
        }
        result
    }
}

pub const DEFAULT_CONTINUATION_PROMPT: &str = indoc! {"
    [SYSTEM REMINDER - TODO CONTINUATION]
    You have incomplete tasks. Rules:
    1. BEFORE marking a todo done: verify the work compiles/works. No premature completion.
    2. If a todo is broad (e.g. \"implement X and implement Y\"): break it into specific subtasks FIRST using todo__add, then work on those.\n\
    3. Each todo should be atomic and be \"single responsibility\" - completable in one focused action.
    4. If you are blocked on user input or a user-side action: call todo__pause with the reason (if available); otherwise ask the user directly in your reply and end your turn. NEVER mark unfinished todos done just to stop this reminder.
    5. Otherwise, continue with the next pending item now. Call tools immediately."
};

static REPL_COMMANDS: LazyLock<[ReplCommand; 66]> = LazyLock::new(|| {
    [
        ReplCommand::new(".help", "Show this help guide", AssertState::pass()),
        ReplCommand::new(".info", "Show system info", AssertState::pass()),
        ReplCommand::new(
            ".info tools",
            "Show the list of enabled tools to be passed to the LLM",
            AssertState::True(StateFlags::FUNCTION_CALLING),
        ),
        ReplCommand::new(
            ".info mcp-server",
            "Show an MCP server's tool filters and effective catalog",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".authenticate",
            "Authenticate the current model client via OAuth (if configured)",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".mcp auth",
            "Authenticate with an MCP server via OAuth",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".mcp enable",
            "Enable a single MCP server in the current context",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".mcp disable",
            "Disable a single MCP server in the current context",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".tool enable",
            "Enable a single tool in the current context",
            AssertState::True(StateFlags::FUNCTION_CALLING),
        ),
        ReplCommand::new(
            ".tool disable",
            "Disable a single tool in the current context",
            AssertState::True(StateFlags::FUNCTION_CALLING),
        ),
        ReplCommand::new(
            ".edit config",
            "Modify configuration file",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".edit mcp-config",
            "Modify the MCP servers configuration file",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(".model", "Switch LLM model", AssertState::pass()),
        ReplCommand::new(
            ".prompt",
            "Invoke an MCP prompt and submit the result as chat input",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".temp-role",
            "Set a temporary role using a prompt",
            AssertState::False(StateFlags::SESSION | StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".role",
            "Create or switch to a role",
            AssertState::False(StateFlags::SESSION | StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".info role",
            "Show role info",
            AssertState::True(StateFlags::ROLE),
        ),
        ReplCommand::new(
            ".edit role",
            "Modify current role",
            AssertState::TrueFalse(StateFlags::ROLE, StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".save role",
            "Save current role to file",
            AssertState::TrueFalse(
                StateFlags::ROLE,
                StateFlags::SESSION_EMPTY | StateFlags::SESSION,
            ),
        ),
        ReplCommand::new(
            ".exit role",
            "Exit active role",
            AssertState::TrueFalse(StateFlags::ROLE, StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".session",
            "Start or switch to a session",
            AssertState::False(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".empty session",
            "Clear session messages",
            AssertState::True(StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".undo",
            "Undo the last exchange and restore the prompt",
            AssertState::True(StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".compress session",
            "Compress session messages (optional: upper bound on trailing messages to keep)",
            AssertState::True(StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".info session",
            "Show session info",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".edit session",
            "Modify current session",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".save session",
            "Save current session to file",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".exit session",
            "Exit active session",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".fork",
            "Fork the active session into a new named copy",
            AssertState::True(StateFlags::SESSION),
        ),
        ReplCommand::new(".agent", "Use an agent", AssertState::bare()),
        ReplCommand::new(
            ".starter",
            "Use a conversation starter",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".edit agent-config",
            "Modify agent configuration file",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".info agent",
            "Show agent info",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".exit agent",
            "Leave agent",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".clear todo",
            "Clear the todo list and stop auto-continuation",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".info todo",
            "Show the current todo list driving auto-continuation",
            AssertState::True(StateFlags::AUTO_CONTINUE),
        ),
        ReplCommand::new(
            ".rag",
            "Initialize or access RAG",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".rag attach",
            "Attach to a pre-existing external RAG",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".edit rag-docs",
            "Add or remove documents from an existing RAG",
            AssertState::TrueFalse(StateFlags::RAG, StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".rebuild rag",
            "Rebuild RAG for document changes",
            AssertState::True(StateFlags::RAG),
        ),
        ReplCommand::new(
            ".sources rag",
            "Show citation sources used in last query",
            AssertState::True(StateFlags::RAG),
        ),
        ReplCommand::new(
            ".info rag",
            "Show RAG info",
            AssertState::True(StateFlags::RAG),
        ),
        ReplCommand::new(
            ".exit rag",
            "Leave RAG",
            AssertState::TrueFalse(StateFlags::RAG, StateFlags::AGENT),
        ),
        ReplCommand::new(".macro", "Execute a macro", AssertState::pass()),
        ReplCommand::new(
            ".skill",
            "Create a new skill",
            AssertState::True(StateFlags::SKILLS_ENABLED),
        ),
        ReplCommand::new(
            ".skill load",
            "Load a skill into the current context",
            AssertState::True(StateFlags::SKILLS_ENABLED),
        ),
        ReplCommand::new(
            ".skill loaded",
            "List currently-loaded skills",
            AssertState::True(StateFlags::SKILLS_ENABLED),
        ),
        ReplCommand::new(
            ".skill unload",
            "Unload a skill from the current context",
            AssertState::True(StateFlags::SKILLS_ENABLED),
        ),
        ReplCommand::new(
            ".edit skill",
            "Modify an existing skill by name",
            AssertState::True(StateFlags::SKILLS_ENABLED),
        ),
        ReplCommand::new(
            ".file",
            "Include files, directories, URLs or commands",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".continue",
            "Continue previous response",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".recover",
            "Recover interrupted session after API error or Ctrl+C",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".regenerate",
            "Regenerate last response",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".run",
            "Run the graph with this prompt, skipping intent routing",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(".copy", "Copy last response", AssertState::pass()),
        ReplCommand::new(
            ".redraw",
            "Clear the screen and reprint the last conversation turn",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".redraw session",
            "Clear the screen and replay the whole session",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(".set", "Modify runtime settings", AssertState::pass()),
        ReplCommand::new(
            ".reasoning",
            "Set the reasoning effort level for the current model",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".delete",
            "Delete roles, sessions, RAGs, or agents",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".list",
            "List roles, sessions, agents, RAGs, macros, skills, prompts, tools, MCP servers, or bundles",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".vault",
            "View or modify the Coyote vault",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".install",
            "Reinstall bundled assets, install a bundle from a git repo, or update an installed bundle",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".uninstall",
            "Uninstall an installed bundle (delete its owned files and MCP entries)",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".update",
            "Update Coyote to the latest release (or a specified version)",
            AssertState::pass(),
        ),
        ReplCommand::new(".exit", "Exit REPL", AssertState::pass()),
    ]
});
static COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*([./]\S*)\s*").unwrap());
static MULTILINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\s*:::\s*(.*)\s*:::\s*$").unwrap());

pub struct Repl {
    ctx: Arc<RwLock<RequestContext>>,
    editor: Reedline,
    prompt: ReplPrompt,
    abort_signal: AbortSignal,
}

impl Repl {
    pub fn init(ctx: RequestContext) -> Result<Self> {
        let app = Arc::clone(&ctx.app.config);
        let ctx = Arc::new(RwLock::new(ctx));
        let editor = Self::create_editor(Arc::clone(&ctx), app.as_ref())?;
        let prompt = ReplPrompt::new(Arc::clone(&ctx));
        let abort_signal = create_abort_signal();

        Ok(Self {
            ctx,
            editor,
            prompt,
            abort_signal,
        })
    }

    #[allow(clippy::await_holding_lock)]
    pub async fn run(&mut self) -> Result<()> {
        if AssertState::False(StateFlags::AGENT | StateFlags::RAG).assert(self.ctx.read().state()) {
            print!(
                "Welcome to {} {}\n{}\n",
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION"),
                self.ctx
                    .read()
                    .render_hints(r#"Type ".help" for additional help."#),
            );
            if env::var_os(SANDBOX_ENV_FLAG).is_some() {
                eprintln!(
                    "Sandbox mode is enabled. All changes made to the Coyote config will not persist to the host machine."
                );
            }
        }

        {
            let (compressed, active) = {
                let ctx = self.ctx.read();
                match &ctx.session {
                    Some(session) => replay::snapshot(session),
                    None => (Vec::new(), Vec::new()),
                }
            };
            if !compressed.is_empty() || !active.is_empty() {
                let app = Arc::clone(&self.ctx.read().app.config);
                replay::render(app.as_ref(), &compressed, &active)?;
                let last_msgs: &[Message] = if !active.is_empty() {
                    &active
                } else {
                    &compressed
                };
                let assistant_text = last_msgs
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant)
                    .and_then(|m| m.content.as_text())
                    .map(str::to_string);
                if let Some(output) = assistant_text {
                    let user_text = last_msgs
                        .iter()
                        .rev()
                        .find(|m| m.role == MessageRole::User)
                        .and_then(|m| m.content.as_text())
                        .unwrap_or("")
                        .to_string();
                    let ctx = self.ctx.read();
                    if let Ok(input) = Input::from_str(&ctx, &user_text, None) {
                        drop(ctx);
                        self.ctx.write().last_message = Some(LastMessage::new(input, output));
                    }
                }
            }
        }

        // Discard any stray terminal-query reply bytes (e.g. late colorsaurus
        // OSC 11 / DA1 responses) so they don't get injected into the prompt.
        drain_stale_tty_input();

        print_pause_banner(&self.ctx.read());

        loop {
            if self.abort_signal.aborted_ctrld() {
                break;
            }
            // Drain stale ESC[6n replies so reedline anchors the prompt on the real row.
            resync_cursor_queries();
            let sig = self.editor.read_line(&self.prompt);
            match sig {
                Ok(Signal::Success(line)) => {
                    self.abort_signal.reset();
                    let result = {
                        let mut ctx = self.ctx.write();
                        run_repl_command(&mut ctx, self.abort_signal.clone(), &line).await
                    };
                    match result {
                        Ok(exit) => {
                            if exit {
                                break;
                            }
                            if let Some(text) = self.ctx.write().pending_prefill.take() {
                                self.editor
                                    .run_edit_commands(&[EditCommand::InsertString(text)]);
                            }
                        }
                        Err(err) => {
                            render_error(err);
                            println!()
                        }
                    }
                }
                Ok(Signal::CtrlC) => {
                    let cancelled_tasks =
                        self.ctx
                            .read()
                            .supervisor
                            .clone()
                            .is_some_and(|supervisor| {
                                latch_prompt_interrupt(&self.abort_signal, &supervisor)
                            });
                    if cancelled_tasks {
                        println!("{}", self.ctx.read().render_hints(CANCELLED_TASKS_HINT));
                    }
                    println!(
                        "{}\n",
                        self.ctx
                            .read()
                            .render_hints("(To exit, press Ctrl+D or enter \".exit\")")
                    );
                }
                Ok(Signal::CtrlD) => {
                    self.abort_signal.set_ctrld();
                    break;
                }
                _ => {}
            }
        }

        self.ctx.read().cancel_background_tasks();

        let exit_result = self.ctx.write().exit_session();
        self.ctx
            .write()
            .top_level_agent_finished(exit_result.as_ref().err(), Some(&self.abort_signal));
        hooks::drain_pending(EXIT_HOOK_DRAIN_TIMEOUT).await;
        exit_result
    }

    fn create_editor(ctx: Arc<RwLock<RequestContext>>, app: &AppConfig) -> Result<Reedline> {
        let completer = ReplCompleter::new(Arc::clone(&ctx));
        let highlighter = ReplHighlighter::new(Arc::clone(&ctx));
        let menu = Self::create_menu();
        let edit_mode = Self::create_edit_mode(app);
        let cursor_config = CursorConfig {
            vi_insert: Some(SetCursorStyle::BlinkingBar),
            vi_normal: Some(SetCursorStyle::SteadyBlock),
            ..CursorConfig::default()
        };
        let mut editor = Reedline::create()
            .with_completer(Box::new(completer))
            .with_highlighter(Box::new(highlighter))
            .with_menu(menu)
            .with_edit_mode(edit_mode)
            .with_cursor_config(cursor_config)
            .with_quick_completions(true)
            .with_partial_completions(true)
            .use_bracketed_paste(true)
            .with_validator(Box::new(ReplValidator))
            .with_ansi_colors(true);

        if let Ok(cmd) = app.editor() {
            let temp_file = temp_file("-repl-", ".md");
            let command = process::Command::new(cmd);
            editor = editor.with_buffer_editor(command, temp_file);
        }

        if app.save_shell_history {
            let ctx = ctx.read();
            let history_path = paths::repl_history_file(&ctx.session);
            if let Ok(history) = FileBackedHistory::with_file(1000, history_path) {
                editor = editor.with_history(Box::new(history));
            }
        }

        Ok(editor)
    }

    fn extra_keybindings(keybindings: &mut Keybindings) {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(MENU_NAME.to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keybindings.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::BackTab,
            ReedlineEvent::MenuPrevious,
        );
        keybindings.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Enter,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );
        keybindings.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('j'),
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );
    }

    fn create_edit_mode(app: &AppConfig) -> Box<dyn EditMode> {
        let edit_mode: Box<dyn EditMode> = if app.keybindings == "vi" {
            let mut insert_keybindings = default_vi_insert_keybindings();
            Self::extra_keybindings(&mut insert_keybindings);
            Box::new(Vi::new(
                insert_keybindings,
                default_vi_normal_keybindings(),
                default_vi_visual_keybindings(),
            ))
        } else {
            let mut keybindings = default_emacs_keybindings();
            Self::extra_keybindings(&mut keybindings);
            Box::new(Emacs::new(keybindings))
        };
        edit_mode
    }

    fn create_menu() -> ReedlineMenu {
        let completion_menu = ColumnarMenu::default().with_name(MENU_NAME);
        ReedlineMenu::EngineCompleter(Box::new(completion_menu))
    }
}

#[derive(Debug, Clone)]
pub struct ReplCommand {
    name: &'static str,
    description: &'static str,
    state: AssertState,
}

impl ReplCommand {
    fn new(name: &'static str, desc: &'static str, state: AssertState) -> Self {
        Self {
            name,
            description: desc,
            state,
        }
    }

    fn is_valid(&self, flags: StateFlags) -> bool {
        self.state.assert(flags)
    }
}

/// A default validator which checks for mismatched quotes and brackets
struct ReplValidator;

impl Validator for ReplValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let line = line.trim();
        if line.starts_with(r#":::"#) && !line[3..].ends_with(r#":::"#) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

pub async fn run_repl_command(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    line: &str,
) -> Result<bool> {
    let prefix = ctx.command_prefix();
    run_repl_command_with_prefix(ctx, abort_signal, line, prefix).await
}

pub async fn run_repl_command_with_prefix(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    mut line: &str,
    prefix: CommandPrefix,
) -> Result<bool> {
    ctx.pending_tasks_guardrail_count = 0;
    if let Ok(Some(captures)) = MULTILINE_RE.captures(line)
        && let Some(text_match) = captures.get(1)
    {
        line = text_match.as_str();
    }
    match parse_command(line, prefix) {
        Some((cmd, args)) => match cmd.as_ref() {
            ".help" => {
                dump_repl_help(ctx.command_prefix());
            }
            ".info" => match args {
                Some("role") => {
                    let info = ctx.role_info()?;
                    print!("{info}");
                }
                Some("session") => {
                    let app = Arc::clone(&ctx.app.config);
                    let info = ctx.session_info(app.as_ref())?;
                    print!("{info}");
                }
                Some("rag") => {
                    let info = ctx.rag_info()?;
                    print!("{info}");
                }
                Some("agent") => {
                    let info = ctx.agent_info()?;
                    print!("{info}");
                }
                Some("tools") => {
                    let info = ctx.tools_info()?;
                    print!("{info}");
                }
                Some("todo") => {
                    let info = ctx.todo_info()?;
                    print!("{info}");
                }
                Some(arg) if arg.starts_with("mcp-server") => {
                    let mut parts = arg.splitn(2, char::is_whitespace);
                    parts.next();
                    let name = parts.next().map(str::trim).unwrap_or("");
                    if name.is_empty() {
                        bail!("{}", ctx.render_hints("Usage: .info mcp-server <server>"));
                    }
                    let info = ctx.mcp_server_info(name).await?;
                    print!("{info}");
                }
                Some(_) => unknown_command(ctx)?,
                None => {
                    let app = Arc::clone(&ctx.app.config);
                    let output = ctx.sysinfo(app.as_ref())?;
                    print!("{output}");
                }
            },
            ".model" => match args {
                Some(name) => {
                    let app = Arc::clone(&ctx.app.config);
                    ctx.set_model_on_role_like(app.as_ref(), name)?;
                }
                None => println!("{}", ctx.render_hints("Usage: .model <name>")),
            },
            ".authenticate" => {
                let current_model = ctx.current_model().clone();
                let app = Arc::clone(&ctx.app.config);
                let client = init_client(&app, current_model)?;
                if !client.supports_oauth() {
                    bail!(
                        "Client '{}' doesn't either support OAuth or isn't configured to use it (i.e. uses an API key instead)",
                        client.name()
                    );
                }
                let clients = ctx.app.config.clients.clone();
                let (client_name, provider) = resolve_oauth_client(Some(client.name()), &clients)?;
                oauth::run_oauth_flow(&*provider, &client_name).await?;
            }
            ".mcp" => match args {
                Some(args) => {
                    let mut parts = args.splitn(2, char::is_whitespace);
                    let sub = parts.next().unwrap_or("").trim();
                    let rest = parts.next().map(str::trim).unwrap_or("");
                    match sub {
                        "auth" => {
                            if rest.is_empty() {
                                println!("{}", ctx.render_hints("Usage: .mcp auth <server_name>"));
                            } else {
                                let server_name = rest;
                                let server_spec = ctx
                                    .app
                                    .mcp_config
                                    .as_ref()
                                    .and_then(|c| c.mcp_servers.get(server_name))
                                    .cloned();
                                match server_spec {
                                    None => {
                                        bail!("MCP server '{}' not found in mcp.json.", server_name)
                                    }
                                    Some(spec) if !spec.is_remote() => bail!(
                                        "MCP server '{}' uses stdio transport; \
                                         OAuth is only supported for http/sse servers.",
                                        server_name
                                    ),
                                    Some(spec) => {
                                        let url = spec
                                            .url
                                            .as_deref()
                                            .expect("validated: remote spec has url");
                                        let client_id = spec
                                            .oauth
                                            .as_ref()
                                            .and_then(|o| o.client_id.as_deref());
                                        let callback_port =
                                            spec.oauth.as_ref().and_then(|o| o.callback_port);
                                        let redirect_host = spec
                                            .oauth
                                            .as_ref()
                                            .and_then(|o| o.redirect_host.as_deref());
                                        mcp::oauth::run_mcp_oauth_flow(
                                            server_name,
                                            url,
                                            client_id,
                                            callback_port,
                                            redirect_host,
                                        )
                                        .await?;
                                        println!("Authentication saved.");
                                        if ctx.app.config.mcp_server_support {
                                            let app = Arc::clone(&ctx.app.config);
                                            ctx.bootstrap_tools(
                                                app.as_ref(),
                                                true,
                                                abort_signal.clone(),
                                            )
                                            .await?;
                                            if ctx.tool_scope.mcp_runtime.get(server_name).is_some()
                                            {
                                                println!(
                                                    "✓ MCP server '{server_name}' started and attached to the current context."
                                                );
                                            } else {
                                                println!(
                                                    "MCP server '{server_name}' is not enabled in the current context. \
                                                     Run `{} {server_name}` to attach it.",
                                                    ctx.render_hints(".mcp enable")
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        "enable" | "disable" => {
                            if rest.is_empty() {
                                println!(
                                    "Usage: {} {sub} <server_name>",
                                    ctx.render_hints(".mcp")
                                );
                            } else {
                                ctx.toggle_mcp_server(sub, rest, abort_signal.clone())
                                    .await?;
                            }
                        }
                        _ => unknown_command(ctx)?,
                    }
                }
                None => println!(
                    "{}",
                    ctx.render_hints(
                        r#"Usage:
    .mcp auth <server_name>         # Authenticate with an MCP server via OAuth
    .mcp enable <server_name>       # Enable a single MCP server in the current context
    .mcp disable <server_name>      # Disable a single MCP server in the current context"#
                    )
                ),
            },
            ".tool" => match args {
                Some(args) => {
                    let mut parts = args.splitn(2, char::is_whitespace);
                    let sub = parts.next().unwrap_or("").trim();
                    let rest = parts.next().map(str::trim).unwrap_or("");
                    match sub {
                        "enable" | "disable" => {
                            if rest.is_empty() {
                                println!("Usage: {} {sub} <name>", ctx.render_hints(".tool"));
                            } else {
                                ctx.toggle_tool(sub, rest)?;
                            }
                        }
                        _ => unknown_command(ctx)?,
                    }
                }
                None => println!(
                    "{}",
                    ctx.render_hints(
                        r#"Usage:
    .tool enable <name>             # Enable a single tool in the current context
    .tool disable <name>            # Disable a single tool in the current context"#
                    )
                ),
            },
            ".prompt" => {
                let (words, _) = split_args_text(args.unwrap_or_default(), cfg!(windows));
                match words.as_slice() {
                    [server, name, rest @ ..] => {
                        let provided = parse_prompt_call_args(rest)?;
                        let prompts = ctx.tool_scope.mcp_runtime.list_prompts(server).await?;
                        let declared = prompts
                            .into_iter()
                            .find(|prompt| prompt.name == *name)
                            .with_context(|| {
                                format!("Prompt '{name}' not found on MCP server '{server}'")
                            })?
                            .arguments
                            .unwrap_or_default();
                        let (mut arguments, missing) = resolve_prompt_args(&declared, provided);
                        for key in missing {
                            let value = Text::new(&prompt_arg_inquire_label(server, name, &key))
                                .prompt()
                                .with_context(|| {
                                    format!("Failed to read prompt argument '{key}'")
                                })?;
                            arguments.insert(key, value);
                        }
                        let result = ctx
                            .tool_scope
                            .mcp_runtime
                            .prompt(server, name, arguments)
                            .await?;
                        let flattened = flatten_prompt_messages(&result.messages);
                        let input = Input::from_str(ctx, &flattened, None)?;
                        ask(ctx, abort_signal.clone(), input, true).await?;
                    }
                    _ => println!(
                        "{}",
                        ctx.render_hints("Usage: .prompt <server> <name> [key=value ...]")
                    ),
                }
            }
            ".temp-role" => match args {
                Some(text) => {
                    let app = Arc::clone(&ctx.app.config);
                    ctx.use_temp_role(app.as_ref(), text)?;
                }
                None => println!("{}", ctx.render_hints("Usage: .temp-role <text>...")),
            },
            ".role" => match args {
                Some(args) => match args.split_once(['\n', ' ']) {
                    Some((name, text)) => {
                        let app = Arc::clone(&ctx.app.config);
                        let role = ctx.retrieve_role(app.as_ref(), name.trim())?;
                        let input = Input::from_str(ctx, text, Some(role))?;
                        ask(ctx, abort_signal.clone(), input, false).await?;
                    }
                    None => {
                        let name = args;
                        let app = Arc::clone(&ctx.app.config);
                        if !paths::has_role(name) {
                            ctx.new_role(app.as_ref(), name)?;
                        }

                        ctx.use_role(app.as_ref(), name, abort_signal.clone())
                            .await?;
                    }
                },
                None => println!(
                    "{}",
                    ctx.render_hints(
                        r#"Usage:
    .role <name>                    # If the role exists, switch to it; otherwise, create a new role
    .role <name> [text]...          # Temporarily switch to the role, send the text, and switch back"#
                    )
                ),
            },
            ".skill" => {
                let trimmed = args.map(str::trim).unwrap_or("");
                let mut parts = trimmed.splitn(2, char::is_whitespace);
                let first = parts.next().unwrap_or("");
                let rest = parts.next().map(str::trim).unwrap_or("");
                match first {
                    "" => println!(
                        "{}",
                        ctx.render_hints(
                            r#"Usage:
    .skill loaded                   # List currently-loaded skills
    .skill load <name>              # Load a skill into the current context
    .skill unload <name>            # Unload a loaded skill
    .skill <name>                   # Open the skill in $EDITOR; create with a scaffold if missing
                                    # (Use `.edit skill <name>` to edit an existing skill without the create-if-missing behavior.)"#
                        )
                    ),
                    "loaded" => ctx.list_loaded_skills(),
                    "load" => {
                        if rest.is_empty() {
                            println!("{}", ctx.render_hints("Usage: .skill load <name>"));
                        } else {
                            ctx.load_skill_repl(rest, abort_signal.clone()).await?;
                        }
                    }
                    "unload" => {
                        if rest.is_empty() {
                            println!("{}", ctx.render_hints("Usage: .skill unload <name>"));
                        } else {
                            ctx.unload_skill_repl(rest, abort_signal.clone()).await?;
                        }
                    }
                    name => {
                        let app = Arc::clone(&ctx.app.config);
                        ctx.upsert_skill(app.as_ref(), name)?;
                    }
                }
            }
            ".session" => {
                if let Some(name) = graph::active_agent_graph_name(ctx)
                    && !graph::graph_supports_sessions(&name)
                {
                    bail!("{}", graph::no_session_block_text(&name));
                }
                let app = Arc::clone(&ctx.app.config);
                ctx.use_session(app.as_ref(), args, abort_signal.clone())
                    .await?;
                if ctx.maybe_autoname_session() {
                    eprintln!(
                        "\n📢 {}",
                        dim_notice_color(app.as_ref())
                            .italic()
                            .paint("Autonaming the session."),
                    );
                    if let Err(err) = ctx.autoname_session(app.as_ref()).await {
                        warn!("Failed to autonaming the session: {err}");
                    }
                    if let Some(session) = ctx.session.as_mut() {
                        session.set_autonaming(false);
                    }
                }
                if let Some(session) = &ctx.session {
                    let (compressed, active) = replay::snapshot(session);
                    replay::render(app.as_ref(), &compressed, &active)?;
                }
                print_pause_banner(ctx);
            }
            ".install" => match parse_repl_install(args) {
                ReplInstallDispatch::Builtins(category) => config::install_assets(category)?,
                ReplInstallDispatch::Unified(value) => {
                    config::install_or_update_from_repl_args(value, ctx.command_prefix())?;
                }
                ReplInstallDispatch::Help => {
                    println!("{}", ctx.render_hints(&repl_install_help()))
                }
                ReplInstallDispatch::Usage => println!(
                    "{}",
                    ctx.render_hints(&format!(
                        "Usage: .install <{}> | .install <git-url|owner/repo|installed-bundle> \
                         [--git-host <host>] [--filter <cat>] [--force] (see `.install --help`)",
                        AssetCategory::NAMES.join("|")
                    ))
                ),
            },
            ".update" => {
                if ctx.macro_flag {
                    bail!("Cannot perform this operation because you are in a macro")
                }
                let version = args.map(|s| s.trim().to_string());
                task::spawn_blocking(move || config::run_self_update(version, false)).await??;
            }
            ".rag" => match split_first_arg(args) {
                Some(("attach", rest)) => match rest {
                    Some(name) if !name.trim().is_empty() => {
                        ctx.attach_rag(name.trim(), abort_signal.clone()).await?;
                    }
                    _ => println!("{}", ctx.render_hints("Usage: .rag attach <name>")),
                },
                _ => {
                    ctx.use_rag(args, abort_signal.clone()).await?;
                }
            },
            ".agent" => match split_first_arg(args) {
                Some((agent_name, args)) => {
                    let (new_args, _) = split_args_text(args.unwrap_or_default(), cfg!(windows));
                    let (session_name, variable_pairs) = match new_args.first() {
                        Some(name) if name.contains('=') => (None, new_args.as_slice()),
                        Some(name) => (Some(name.as_str()), &new_args[1..]),
                        None => (None, &[] as &[String]),
                    };
                    let variables: AgentVariables = variable_pairs
                        .iter()
                        .filter_map(|v| v.split_once('='))
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .collect();
                    if variables.len() != variable_pairs.len() {
                        bail!("Some variable values are not key=value pairs");
                    }
                    if !variables.is_empty() {
                        ctx.agent_variables = Some(variables.clone());
                    }
                    let app = Arc::clone(&ctx.app.config);
                    ctx.use_agent(app.as_ref(), agent_name, session_name, abort_signal.clone())
                        .await?;
                    ctx.top_level_agent_started();
                    if let Some(session) = &ctx.session {
                        let (compressed, active) = replay::snapshot(session);
                        replay::render(app.as_ref(), &compressed, &active)?;
                        print_pause_banner(ctx);
                    }
                }
                None => {
                    println!(
                        "{}",
                        ctx.render_hints(
                            "Usage: .agent <agent-name> [session-name] [key=value]..."
                        )
                    )
                }
            },
            ".starter" => match args {
                Some(id) => {
                    let mut text = None;
                    if let Some(agent) = ctx.agent.as_ref() {
                        for (i, value) in agent.conversation_starters().iter().enumerate() {
                            if (i + 1).to_string() == id {
                                text = Some(value.clone());
                            }
                        }
                    }
                    match text {
                        Some(text) => {
                            println!("{}", dimmed_text(&format!(">> {text}")));
                            let input = Input::from_str(ctx, &text, None)?;
                            ask(ctx, abort_signal.clone(), input, true).await?;
                        }
                        None => {
                            bail!("Invalid starter value");
                        }
                    }
                }
                None => {
                    let banner = ctx.agent_banner()?;
                    ctx.app.config.print_markdown(&banner)?;
                }
            },
            ".fork" => {
                ctx.fork_session(args)?;
            }
            ".save" => match split_first_arg(args) {
                Some(("role", name)) => {
                    ctx.save_role(name)?;
                }
                Some(("session", rest)) => match parse_repl_save_session(rest) {
                    ReplSaveSessionDispatch::Run { name, scope } => {
                        ctx.save_session(name.as_deref(), scope)?;
                    }
                    ReplSaveSessionDispatch::Help => {
                        println!("{}", ctx.render_hints(&repl_save_session_help()))
                    }
                    ReplSaveSessionDispatch::Conflict => {
                        println!("--workspace and --global are mutually exclusive")
                    }
                    ReplSaveSessionDispatch::Usage => {
                        println!(
                            "{}",
                            ctx.render_hints(
                                "Usage: .save session [name] [--workspace|--global] (see `.save session --help`)"
                            )
                        )
                    }
                },
                _ => {
                    println!(
                        "{}",
                        ctx.render_hints(
                            "Usage: .save role [name] | .save session [name] [--workspace|--global]"
                        )
                    )
                }
            },
            ".edit" => {
                if ctx.macro_flag {
                    bail!("Cannot perform this operation because you are in a macro")
                }
                match args {
                    Some("config") => {
                        ctx.edit_config()?;
                    }
                    Some("role") => {
                        let app = Arc::clone(&ctx.app.config);
                        ctx.edit_role(app.as_ref(), abort_signal.clone()).await?;
                    }
                    Some("session") => {
                        let app = Arc::clone(&ctx.app.config);
                        ctx.edit_session(app.as_ref())?;
                    }
                    Some("rag-docs") => {
                        ctx.edit_rag_docs(abort_signal.clone()).await?;
                    }
                    Some("agent-config") => {
                        let app = Arc::clone(&ctx.app.config);
                        ctx.edit_agent_config(app.as_ref())?;
                    }
                    Some("mcp-config") => {
                        ctx.edit_mcp_config()?;
                    }
                    Some(s) if s == "skill" || s.starts_with("skill ") => {
                        let name = s.strip_prefix("skill").unwrap_or("").trim();
                        if name.is_empty() {
                            println!("{}", ctx.render_hints("Usage: .edit skill <name>"));
                        } else if let Err(e) = paths::validate_skill_name(name) {
                            bail!(e);
                        } else if !paths::has_skill(name) {
                            bail!(
                                "Skill '{name}' is not installed (expected at {})",
                                paths::skill_file(name).display()
                            );
                        } else {
                            let app = Arc::clone(&ctx.app.config);
                            ctx.upsert_skill(app.as_ref(), name)?;
                        }
                    }
                    _ => {
                        println!(
                            "{}",
                            ctx.render_hints(
                                "Usage: .edit <config|mcp-config|role|session|rag-docs|agent-config|skill <name>>"
                            )
                        )
                    }
                }
            }
            ".compress" => {
                let parts: Vec<&str> = args
                    .map(|v| v.split_whitespace().collect())
                    .unwrap_or_default();
                let keep_last = match parts.as_slice() {
                    ["session"] => Some(None),
                    ["session", value] => value.parse::<usize>().ok().map(Some),
                    _ => None,
                };
                match keep_last {
                    Some(keep_last) => {
                        abortable_run_with_spinner(
                            ctx.compress_session_with(keep_last),
                            "Compressing",
                            abort_signal.clone(),
                        )
                        .await?;
                        hooks::fire(
                            HookEvent::SessionCompressed,
                            ctx,
                            &hooks::role_extras(ctx),
                            None,
                        );
                        println!("✓ Successfully compressed the session.");
                    }
                    None => println!(
                        "{}",
                        ctx.render_hints("Usage: .compress session [keep_last]")
                    ),
                }
            }
            ".empty" => match args {
                Some("session") => {
                    ctx.empty_session()?;
                }
                _ => {
                    println!("{}", ctx.render_hints("Usage: .empty session"))
                }
            },
            ".undo" => {
                if let Some(name) = graph::active_agent_graph_name(ctx)
                    && !graph::graph_supports_sessions(&name)
                {
                    bail!("{}", graph::no_session_block_text(&name));
                }
                ctx.undo_last_exchange()?;
            }
            ".rebuild" => match args {
                Some("rag") => {
                    ctx.rebuild_rag(abort_signal.clone()).await?;
                }
                _ => {
                    println!("{}", ctx.render_hints("Usage: .rebuild rag"))
                }
            },
            ".sources" => match args {
                Some("rag") => {
                    let output = ctx.rag_sources()?;
                    println!("{output}");
                }
                _ => {
                    println!("{}", ctx.render_hints("Usage: .sources rag"))
                }
            },
            ".macro" => match split_first_arg(args) {
                Some((sub @ ("enable" | "disable"), rest)) => {
                    match rest.and_then(|v| v.split_whitespace().next()) {
                        Some(name) => ctx.macro_toggle(name, sub == "enable")?,
                        None => println!("Usage: {} {sub} <name>", ctx.render_hints(".macro")),
                    }
                }
                Some((name, extra)) => {
                    let policy = ctx.macro_policy();
                    match policy.find(name).map(|row| &row.state) {
                        Some(state) if state.is_invocable() => {
                            // macro_execute recurses back into run_repl_command; boxing here
                            // keeps its future out of this already-huge state machine.
                            Box::pin(macro_execute(ctx, name, extra, abort_signal.clone())).await?;
                        }
                        Some(MacroState::DisabledRuntime) => bail!(
                            r#"Macro '{name}' is disabled. Re-enable it with "{} {name}""#,
                            ctx.render_hints(".macro enable")
                        ),
                        Some(MacroState::Locked { level }) => bail!(
                            "Macro '{name}' is restricted by {} enabled_macros",
                            ctx.macro_lock_owner(*level)
                        ),
                        Some(MacroState::Invalid { reason }) => {
                            bail!("Macro '{name}' is invalid: {reason}")
                        }
                        Some(_) | None => {
                            if extra.is_none() {
                                let app = Arc::clone(&ctx.app.config);
                                ctx.new_macro(app.as_ref(), name)?;
                            } else {
                                Box::pin(macro_execute(ctx, name, extra, abort_signal.clone()))
                                    .await?;
                            }
                        }
                    }
                }
                None => println!(
                    "{}",
                    ctx.render_hints(
                        r#"Usage:
    .macro <name> [text]...         # Execute a macro
    .macro enable <name>            # Re-enable a runtime-disabled macro
    .macro disable <name>           # Disable a macro for the rest of this process"#
                    )
                ),
            },
            ".file" => match args {
                Some(args) => {
                    let (files, text) = split_args_text(args, cfg!(windows));
                    let input = Input::from_files_with_spinner(
                        ctx,
                        text,
                        files,
                        None,
                        abort_signal.clone(),
                    )
                    .await?;
                    ask(ctx, abort_signal.clone(), input, true).await?;
                }
                None => println!(
                    "{}",
                    ctx.render_hints(
                        r#"Usage: .file <file|dir|url|cmd|loader:resource|%%>... [-- <text>...]

.file /tmp/file.txt
.file src/ Cargo.toml -- analyze
.file https://example.com/file.txt -- summarize
.file https://example.com/image.png -- recognize text
.file `git diff` -- Generate git commit message
.file jina:https://example.com
.file %% -- translate last reply to english"#
                    )
                ),
            },
            ".continue" => {
                let LastMessage {
                    mut input, output, ..
                } = match ctx
                    .last_message
                    .as_ref()
                    .filter(|v| v.continuous && !v.output.is_empty())
                    .cloned()
                {
                    Some(v) => v,
                    None => bail!("Unable to continue the response"),
                };
                input.set_continue_output(&output);
                ask(ctx, abort_signal.clone(), input, true).await?;
            }
            ".recover" => {
                if !ctx.has_recoverable_interruption() {
                    bail!("Unable to recover: no interrupted session response to recover from");
                }
                let recovery_text = args
                    .unwrap_or("Please continue from where you left off.")
                    .to_string();
                let recovery_input = Input::from_str(ctx, &recovery_text, None)?;
                ask(ctx, abort_signal.clone(), recovery_input, false).await?;
            }
            ".regenerate" => {
                let LastMessage { mut input, .. } =
                    match ctx.last_message.as_ref().filter(|v| v.continuous).cloned() {
                        Some(v) => v,
                        None => bail!("Unable to regenerate the response"),
                    };
                let app = Arc::clone(&ctx.app.config);
                input.set_regenerate(ctx.extract_role(&app)?);
                ask(ctx, abort_signal.clone(), input, true).await?;
            }
            ".run" => {
                if graph::active_agent_graph_name(ctx).is_none() {
                    match ctx.agent.as_ref().map(|a| a.name()) {
                        Some(name) => bail!(
                            "{} only applies to graph-based agents; '{name}' is a config agent",
                            ctx.render_hints(".run")
                        ),
                        None => bail!("No agent"),
                    }
                }
                match args {
                    Some(prompt) => {
                        reset_continuation(ctx);
                        ctx.resume_auto_continue();
                        let input = Input::from_str(ctx, prompt, None)?;
                        ask_with_branch(
                            ctx,
                            abort_signal.clone(),
                            input,
                            true,
                            Some(Branch::Run),
                        )
                        .await?;
                    }
                    None => {
                        println!("{}", ctx.render_hints("Usage: .run <prompt>"))
                    }
                }
            }
            ".set" => match args {
                Some(args) => {
                    ctx.update(args, abort_signal).await?;
                }
                _ => {
                    println!("{}", ctx.render_hints("Usage: .set <key> <value>..."))
                }
            },
            ".reasoning" => match args {
                Some(level) => {
                    let set_args = format!("reasoning_effort {level}");
                    ctx.update(&set_args, abort_signal).await?;
                }
                None => {
                    println!("{}", ctx.render_hints("Usage: .reasoning <level>"))
                }
            },
            ".delete" => match args {
                Some(args) => {
                    ctx.delete(args)?;
                }
                _ => {
                    println!(
                        "{}",
                        ctx.render_hints("Usage: .delete <role|session|rag|macro|skill|agent-data>")
                    )
                }
            },
            ".uninstall" => match parse_repl_uninstall(args) {
                ReplUninstallDispatch::Run(name, assume_yes) => {
                    config::uninstall_bundle(&name, assume_yes)?
                }
                ReplUninstallDispatch::Help => {
                    println!("{}", ctx.render_hints(&repl_uninstall_help()))
                }
                ReplUninstallDispatch::Usage => {
                    println!(
                        "{}",
                        ctx.render_hints(
                            "Usage: .uninstall <bundle-name> [--yes] (see `.uninstall --help`)"
                        )
                    )
                }
            },
            ".list" => match args.map(str::trim) {
                Some("prompts") => {
                    ctx.list_mcp_prompts().await?;
                }
                Some(args) => {
                    ctx.list_assets(args)?;
                }
                _ => {
                    println!(
                        "{}",
                        ctx.render_hints(
                            "Usage: .list <roles|sessions|agents|rags|macros|skills|prompts|tools|mcp-servers|bundles>"
                        )
                    )
                }
            },
            ".copy" => {
                let output = ctx
                    .last_message
                    .as_ref()
                    .filter(|v| !v.output.is_empty())
                    .map(|v| v.output.clone())
                    .or_else(|| {
                        ctx.session.as_ref().and_then(|s| {
                            s.messages()
                                .iter()
                                .rev()
                                .chain(s.compressed_messages().iter().rev())
                                .find(|m| m.role == MessageRole::Assistant)
                                .and_then(|m| m.content.as_text())
                                .map(str::to_string)
                        })
                    });
                match output {
                    Some(v) if !v.is_empty() => {
                        set_text(&v).context("Failed to copy the last chat response")?;
                    }
                    _ => bail!("No chat response to copy"),
                }
            }
            ".redraw" => match args {
                None => {
                    let messages = ctx
                        .session
                        .as_ref()
                        .and_then(|session| {
                            let (compressed, active) = replay::snapshot(session);
                            replay::last_turn(&compressed, &active)
                        })
                        .or_else(|| {
                            ctx.last_message
                                .as_ref()
                                .filter(|v| !v.output.is_empty())
                                .map(replay::synthesize)
                        });
                    let Some(messages) = messages else {
                        bail!("Nothing to redraw yet");
                    };
                    let app = Arc::clone(&ctx.app.config);
                    replay::clear_viewport()?;
                    replay::render_messages(app.as_ref(), &messages)?;
                    println!("{}", dimmed_text("─── ↑ redrawn ↑ ───"));
                    println!();
                    io::stdout().flush()?;
                    print_pause_banner(ctx);
                }
                Some("session") => {
                    let Some(session) = &ctx.session else {
                        bail!(
                            "{}",
                            ctx.render_hints(
                                "No active session to redraw (use \".redraw\" for the last conversation turn)"
                            )
                        );
                    };
                    let (compressed, active) = replay::snapshot(session);
                    if compressed.is_empty() && active.is_empty() {
                        println!("Session is empty.");
                    } else {
                        let app = Arc::clone(&ctx.app.config);
                        replay::clear_viewport()?;
                        replay::render(app.as_ref(), &compressed, &active)?;
                        io::stdout().flush()?;
                        print_pause_banner(ctx);
                    }
                }
                _ => unknown_command(ctx)?,
            },
            ".exit" => match args {
                Some("role") => {
                    ctx.exit_role()?;
                    let app = Arc::clone(&ctx.app.config);
                    ctx.bootstrap_tools(app.as_ref(), true, abort_signal.clone())
                        .await?;
                }
                Some("session") => {
                    if ctx.agent.is_some() {
                        ctx.exit_agent_session()?;
                    } else {
                        ctx.exit_session()?;
                    }
                    let app = Arc::clone(&ctx.app.config);
                    ctx.bootstrap_tools(app.as_ref(), true, abort_signal.clone())
                        .await?;
                }
                Some("rag") => {
                    ctx.exit_rag()?;
                }
                Some("agent") => {
                    let app = Arc::clone(&ctx.app.config);
                    ctx.exit_agent(app.as_ref())?;
                    ctx.bootstrap_tools(app.as_ref(), true, abort_signal.clone())
                        .await?;
                }
                Some(_) => unknown_command(ctx)?,
                None => {
                    return Ok(true);
                }
            },
            ".clear" => match args {
                Some("messages") => {
                    bail!("Use '{}' instead", ctx.render_hints(".empty session"));
                }
                Some("todo") => {
                    let config = ctx.auto_continue_config();
                    if !config.enabled {
                        bail!(
                            "Auto-continue is not enabled. Set 'auto_continue: true' in your config to enable it."
                        );
                    }
                    if ctx.todo_list.is_empty() {
                        println!("Todo list is already empty.");
                    } else {
                        ctx.clear_todo_list();
                        println!("Todo list cleared.");
                    }
                }
                _ => unknown_command(ctx)?,
            },
            ".vault" => match split_first_arg(args) {
                Some(("add", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.add_secret(name)?;
                    } else {
                        println!("{}", ctx.render_hints("Usage: .vault add <name>"));
                    }
                }
                Some(("get", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.get_secret(name, true)?;
                    } else {
                        println!("{}", ctx.render_hints("Usage: .vault get <name>"));
                    }
                }
                Some(("update", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.update_secret(name)?;
                    } else {
                        println!("{}", ctx.render_hints("Usage: .vault update <name>"));
                    }
                }
                Some(("delete", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.delete_secret(name)?;
                    } else {
                        println!("{}", ctx.render_hints("Usage: .vault delete <name>"));
                    }
                }
                Some(("list", _)) => {
                    ctx.app.vault.list_secrets(true)?;
                }
                None | Some(_) => {
                    println!(
                        "{}",
                        ctx.render_hints("Usage: .vault <add|get|update|delete|list> [name]")
                    )
                }
            },
            _ => {
                let name = cmd.strip_prefix('.').unwrap_or(&cmd);
                let policy = ctx.macro_policy();
                match policy.find(name).map(|row| &row.state) {
                    Some(MacroState::Enabled) => {
                        Box::pin(macro_execute(ctx, name, args, abort_signal.clone())).await?;
                    }
                    Some(MacroState::DisabledRuntime) => bail!(
                        r#"Macro '{name}' is disabled. Re-enable it with "{} {name}""#,
                        ctx.render_hints(".macro enable")
                    ),
                    Some(MacroState::Locked { level }) => bail!(
                        "Macro '{name}' is restricted by {} enabled_macros",
                        ctx.macro_lock_owner(*level)
                    ),
                    Some(MacroState::Invalid { reason }) => {
                        bail!("Macro '{name}' is invalid: {reason}")
                    }
                    _ => unknown_command(ctx)?,
                }
            }
        },
        None => {
            if let Some(cmd) = try_extract_shell_command(line) {
                handle_shell_passthrough(cmd)?;
            } else {
                reset_continuation(ctx);
                ctx.resume_auto_continue();
                let input = Input::from_str(ctx, line, None)?;
                ask(ctx, abort_signal.clone(), input, true).await?;
            }
        }
    }

    if !ctx.macro_flag {
        println!();
    }

    Ok(false)
}

enum Transport {
    Quiet,
    Streaming,
    Plain,
}

/// The route classifier's detached call prints nothing; every other call
/// keeps the REPL's streaming/plain split.
fn select_transport(input: &Input) -> Transport {
    if input.role().name() == graph::route::ROUTE_ROLE_NAME {
        Transport::Quiet
    } else if input.stream() {
        Transport::Streaming
    } else {
        Transport::Plain
    }
}

/// Marks an error raised before any request went out, so the completion
/// step can return it bare instead of treating it as a failed request.
#[derive(Debug)]
struct PreflightError(anyhow::Error);

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for PreflightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// One model call as the REPL makes it: build the client, run the
/// before-hook, then the transport `select_transport` picks. The same
/// runner serves the route classifier and the turn's own completion, so a
/// test can drive a whole turn with a canned closure. A `create_client`
/// failure is wrapped in `PreflightError`: nothing was sent, so neither the
/// compress-and-retry nor the INTERRUPTED bookkeeping applies to it.
fn repl_completion_runner()
-> impl for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a> + Send
{
    |input, ctx, abort| {
        Box::pin(async move {
            let client = input
                .create_client()
                .map_err(|err| anyhow::Error::new(PreflightError(err)))?;
            ctx.before_chat_completion(input)?;
            match select_transport(input) {
                Transport::Quiet => {
                    call_chat_completions_streaming_quiet(input, client.as_ref(), ctx, abort).await
                }
                Transport::Streaming => {
                    call_chat_completions_streaming(input, client.as_ref(), ctx, abort).await
                }
                Transport::Plain => {
                    call_chat_completions(input, true, false, client.as_ref(), ctx, abort).await
                }
            }
        })
    }
}

async fn ask(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
) -> Result<()> {
    ask_with_branch(ctx, abort_signal, input, with_embeddings, None).await
}

/// `forced` bypasses the route classifier on a graph agent with a session
/// block; `None` lets the classifier decide.
async fn ask_with_branch(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
    forced: Option<Branch>,
) -> Result<()> {
    ask_with_branch_with_runner(
        ctx,
        abort_signal,
        input,
        with_embeddings,
        forced,
        &mut repl_completion_runner(),
    )
    .await
}

async fn ask_with_branch_with_runner(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
    forced: Option<Branch>,
    runner: &mut CompletionRunner<'_>,
) -> Result<()> {
    restore_graph_agent_if_left_swapped_in(ctx);
    let bracket = TurnBracket::enter(ctx);
    let result = ask_inner(
        ctx,
        abort_signal.clone(),
        input,
        with_embeddings,
        forced,
        runner,
    )
    .await;
    bracket.finish(ctx, &abort_signal, result)
}

/// Puts the graph agent back in the agent slot when a chat agent is somehow
/// still swapped in from an earlier turn, with a warning. Runs before the
/// turn's `turn.started` so every event of the turn, that one included,
/// names the graph agent.
fn restore_graph_agent_if_left_swapped_in(ctx: &mut RequestContext) {
    if ctx.delegate.as_ref().is_none_or(|d| d.parked.is_none()) {
        return;
    }
    let chat_agent = ctx
        .agent
        .as_ref()
        .map(|a| a.name().to_string())
        .unwrap_or_default();
    let graph_agent = ctx
        .delegate
        .as_ref()
        .map(|d| d.agent.name().to_string())
        .unwrap_or_default();
    warn!(
        "Chat agent '{chat_agent}' was still swapped in for graph agent '{graph_agent}' at the start of a turn; restoring the graph agent"
    );
    ctx.leave_delegate_turn();
}

/// Runs the turn and puts the graph agent back in the agent slot on every
/// way out: a delegated chat turn swaps the block's `chat.agent` in inside
/// `run_graph_turn` and relies on this single exit to swap it out.
async fn ask_inner(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
    forced: Option<Branch>,
    runner: &mut CompletionRunner<'_>,
) -> Result<()> {
    let result = ask_loop(ctx, abort_signal, input, with_embeddings, forced, runner).await;
    ctx.leave_delegate_turn();
    result
}

async fn ask_loop(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
    forced: Option<Branch>,
    runner: &mut CompletionRunner<'_>,
) -> Result<()> {
    let mut input = input;
    let mut with_embeddings = with_embeddings;
    // One compress-and-retry per fresh input; tool-loop iterations share it.
    let mut retried_after_compress = false;
    // The graph gate runs once per user turn: tool rounds, the compress
    // retry, the guardrail inject and auto-continue all re-enter the loop
    // with `routed` set, so a chat turn's tool rounds are never
    // re-classified and the graph never runs mid tool-loop.
    let mut routed = false;
    ctx.last_graph_branch = None;
    let _sigint_bridge = spawn_turn_interrupt_bridge(&abort_signal);
    loop {
        if input.is_empty() {
            return Ok(());
        }
        ctx.session_abort = Some(abort_signal.clone());
        if with_embeddings {
            input.use_embeddings(abort_signal.clone()).await?;
            with_embeddings = false;
        }
        while ctx.is_compressing_session() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let app = Arc::clone(&ctx.app.config);

        if maybe_auto_compress(ctx, app.as_ref()).await.succeeded {
            input.refresh_session(ctx);
        }

        if !routed && input.tool_calls().is_none() && graph::active_agent_graph_name(ctx).is_some()
        {
            routed = true;
            ctx.before_chat_completion(&input)?;
            match run_graph_turn(ctx, &input, abort_signal.clone(), forced, runner).await? {
                GraphTurn::Run { output, last_run } => {
                    app.print_markdown(&output)?;
                    input.clear_continue_output();
                    ctx.record_chat_completion(app.as_ref(), &input, &output, &[])?;
                    if !app.dry_run
                        && let Some(session) = input.session_mut(&mut ctx.session)
                    {
                        // `None` (a run on a graph without a session block)
                        // clears any stale record from an earlier run.
                        session.set_last_run(last_run);
                    }
                    if input.session(&ctx.session).is_some() {
                        ctx.checkpoint_session_to_disk(app.dry_run);
                    }
                    settle_turn(ctx, app.as_ref()).await;
                    print_turn_divider();
                    return Ok(());
                }
                GraphTurn::Chat(attached) => {
                    input = *attached;
                }
            }
        }

        let (output, tool_results) = {
            let result = runner(&input, ctx, abort_signal.clone()).await;
            match result {
                Ok(v) => v,
                Err(err) => {
                    let err = match err.downcast::<PreflightError>() {
                        Ok(preflight) => return Err(preflight.0),
                        Err(err) => err,
                    };
                    if compress_for_error_retry(
                        ctx,
                        app.as_ref(),
                        retried_after_compress,
                        &abort_signal,
                        &err,
                    )
                    .await
                    {
                        input.refresh_session(ctx);
                        retried_after_compress = true;
                        continue;
                    }
                    ctx.on_chat_completion_error(app.as_ref(), &input);
                    return Err(err);
                }
            }
        };
        ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;
        let interrupted = abort_signal.aborted_ctrlc();
        if !tool_results.is_empty() {
            if interrupted {
                debug!(
                    "turn interrupted: {} tool results committed, unfinished tasks {:?}",
                    tool_results.len(),
                    pending_tasks(ctx)
                        .into_iter()
                        .filter(|task| !task.finished)
                        .map(|task| task.id)
                        .collect::<Vec<_>>()
                );
                let merged = input.merge_tool_results(output.clone(), tool_results);
                ctx.after_chat_completion(app.as_ref(), &merged, TURN_INTERRUPTED_TEXT, &[])?;
                // The marker is bookkeeping, not model text: `.continue` and
                // `%%` must not replay it. `continuous` stays set so
                // `.recover` and `.regenerate` work as after any tool turn.
                if let Some(last) = ctx.last_message.as_mut() {
                    last.output.clear();
                }
            } else {
                input = input.merge_tool_results(output, tool_results);
                let (next, checkpoint) = ctx.maybe_checkpoint_tool_loop(input)?;
                input = next;
                if let Some(checkpoint) = checkpoint {
                    eprintln!(
                        "\n📎 {}",
                        dim_notice_color(app.as_ref()).italic().paint(format!(
                            "Checkpointed {} tool results (~{} tokens) to the session; the tool loop continues.",
                            checkpoint.committed_results, checkpoint.committed_tokens
                        ))
                    );
                    retried_after_compress = false;
                }
                continue;
            }
        }

        // The guardrail must not run on an interrupted turn: its reminders
        // would be saved as phantom exchanges and its force-terminate would
        // cancel the background tasks the user chose to leave running.
        if interrupted {
            print_interrupted_turn_notice(ctx);
        } else {
            match check_pending_tasks_guardrail(ctx) {
                GuardrailAction::Inject(prompt) => {
                    input = Input::from_str(ctx, &prompt, None)?;
                    retried_after_compress = false;
                    continue;
                }
                GuardrailAction::ForceTerminate(ids) => {
                    warn!(
                        "Pending-agent guardrail force-cancelled {} agent(s) after max reminders: {:?}",
                        ids.len(),
                        ids
                    );
                }
                GuardrailAction::NoAction => {}
            }
        }
        let do_continue = should_continue(ctx) && !abort_signal.aborted_ctrlc();

        if do_continue {
            let full_prompt = {
                let config = ctx.auto_continue_config();
                let todo_state = ctx.todo_list.render_for_model();
                let remaining = ctx.todo_list.incomplete_count();
                ctx.set_last_continuation_response(output.clone());
                ctx.increment_auto_continue_count();
                let count = ctx.auto_continue_count;
                let max = config.max_continues;

                let prompt = config
                    .continuation_prompt
                    .as_deref()
                    .unwrap_or(DEFAULT_CONTINUATION_PROMPT);

                eprintln!(
                    "\n📋 {}",
                    dim_notice_color(app.as_ref()).italic().paint(format!(
                        "Auto-continuing ({count}/{max}): {remaining} incomplete todo(s) remain"
                    ))
                );

                format!("{prompt}\n\n{todo_state}")
            };
            input = Input::from_str(ctx, &full_prompt, None)?;
            retried_after_compress = false;
            continue;
        }

        settle_turn(ctx, app.as_ref()).await;

        let agent_can_continue_after_compress =
            should_continue(ctx) && !abort_signal.aborted_ctrlc();

        // Auto-continue keys off the compression CONDITION, not the outcome:
        // a transiently failed compression retries at the next turn end, and
        // gating on success would stall an unattended agent at the pause
        // banner with incomplete todos.
        let compression = maybe_auto_compress(ctx, app.as_ref()).await;
        if compression.fired && agent_can_continue_after_compress {
            let full_prompt = {
                let config = ctx.auto_continue_config();
                let todo_state = ctx.todo_list.render_for_model();
                let remaining = ctx.todo_list.incomplete_count();
                ctx.increment_auto_continue_count();
                let count = ctx.auto_continue_count;
                let max = config.max_continues;

                let prompt = config
                    .continuation_prompt
                    .as_deref()
                    .unwrap_or(DEFAULT_CONTINUATION_PROMPT);

                eprintln!(
                    "\n📋 {}",
                    dim_notice_color(app.as_ref()).italic().paint(format!(
                        "Auto-continuing after compression ({count}/{max}): {remaining} incomplete todo(s) remain"
                    ))
                );

                format!("{prompt}\n\n{todo_state}")
            };
            input = Input::from_str(ctx, &full_prompt, None)?;
            retried_after_compress = false;
            continue;
        }

        print_turn_divider();
        return Ok(());
    }
}

/// The end-of-turn bookkeeping every completed turn shares, graph run or
/// chat: the continuation counter resets, the pause banner prints and a
/// temp session gets its autoname.
async fn settle_turn(ctx: &mut RequestContext, app: &AppConfig) {
    reset_continuation(ctx);
    print_pause_banner(ctx);
    if ctx.maybe_autoname_session() {
        eprintln!(
            "\n📢 {}",
            dim_notice_color(app)
                .italic()
                .paint("Autonaming the session."),
        );
        if let Err(err) = ctx.autoname_session(app).await {
            warn!("Failed to autonaming the session: {err}");
        }
        if let Some(session) = ctx.session.as_mut() {
            session.set_autonaming(false);
        }
    }
}

fn print_turn_divider() {
    let width = crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(33);
    println!("{}", dimmed_text(&"─".repeat(width)));
    println!();
}

struct AutoCompressOutcome {
    fired: bool,
    succeeded: bool,
}

async fn maybe_auto_compress(ctx: &mut RequestContext, app: &AppConfig) -> AutoCompressOutcome {
    let keep = ctx.compression_keep_policy(None);
    let should_compress = ctx.session.as_ref().is_some_and(|session| {
        session.needs_compression(app.compression_threshold, keep)
            || session.safety_valve_triggered(
                app.compression_safety_valve,
                keep,
                ctx.last_prompt_token_usage,
            )
    });
    if !should_compress {
        return AutoCompressOutcome {
            fired: false,
            succeeded: false,
        };
    }
    AutoCompressOutcome {
        fired: true,
        succeeded: run_auto_compress(ctx, app).await,
    }
}

async fn run_auto_compress(ctx: &mut RequestContext, app: &AppConfig) -> bool {
    if let Some(session) = ctx.session.as_mut() {
        session.set_compressing(true);
    }

    eprintln!(
        "\n📢 {}",
        dim_notice_color(app)
            .italic()
            .paint("Compressing the session."),
    );

    let compressed = auto_compress_session(ctx, app).await;
    if let Some(session) = ctx.session.as_mut() {
        session.set_compressing(false);
    }
    compressed
}

/// A context-overflow rejection is proof the prompt is too big even when the
/// model declares no window for the valve to measure against, so it earns
/// the same compress-and-retry as a tripped valve, provided there is a
/// session with something to fold.
async fn compress_for_error_retry(
    ctx: &mut RequestContext,
    app: &AppConfig,
    already_retried: bool,
    abort_signal: &AbortSignal,
    err: &Error,
) -> bool {
    if already_retried || abort_signal.aborted() {
        return false;
    }
    let keep = ctx.compression_keep_policy(None);
    let valve_triggered = ctx.session.as_ref().is_some_and(|session| {
        session.safety_valve_triggered(
            app.compression_safety_valve,
            keep,
            ctx.last_prompt_token_usage,
        )
    });
    let compressible = ctx.session.as_ref().is_some_and(Session::has_user_messages);
    (valve_triggered || (compressible && graph::is_context_overflow_error(err)))
        && run_auto_compress(ctx, app).await
}

async fn auto_compress_session(ctx: &mut RequestContext, app: &AppConfig) -> bool {
    match ctx.compress_session().await {
        Ok(()) => {
            hooks::fire(
                HookEvent::SessionCompressed,
                ctx,
                &hooks::role_extras(ctx),
                None,
            );
            true
        }
        Err(err) => {
            warn!("Failed to compress the session: {err:#}");
            eprintln!("\n{}", compression_failure_notice(app, &err));
            false
        }
    }
}

fn compression_failure_notice(app: &AppConfig, err: &Error) -> String {
    dim_notice_color(app)
        .italic()
        .paint(format!("⚠️ Compression failed: {err:#}"))
        .to_string()
}

fn dim_notice_color(app: &AppConfig) -> nu_ansi_term::Color {
    if app.light_theme() {
        nu_ansi_term::Color::LightGray
    } else {
        nu_ansi_term::Color::DarkGray
    }
}

fn should_continue(ctx: &RequestContext) -> bool {
    let config = ctx.auto_continue_config();
    ctx.app.config.function_calling_support
        && config.enabled
        && ctx.auto_continue_count < config.max_continues
        && ctx.todo_list.has_incomplete()
        && ctx.auto_continue_paused.is_none()
}

fn reset_continuation(ctx: &mut RequestContext) {
    ctx.reset_continuation_count();
}

/// Returns whether any background task was still running and got cancelled.
pub(crate) fn latch_prompt_interrupt(
    abort_signal: &AbortSignal,
    supervisor: &RwLock<Supervisor>,
) -> bool {
    let supervisor = supervisor.read();
    let had_active_tasks = supervisor.has_active_tasks();
    if had_active_tasks {
        abort_signal.set_ctrlc();
    }
    supervisor.cancel_recursive();
    had_active_tasks
}

fn spawn_turn_interrupt_bridge(abort_signal: &AbortSignal) -> TaskCancelGuard {
    let signal = abort_signal.clone();
    TaskCancelGuard::spawn_one(async move { wait_user_interrupt(Some(&signal)).await })
}

fn pause_banner_text(ctx: &RequestContext) -> Option<String> {
    let reason = ctx.auto_continue_paused.as_deref()?;
    if !ctx.todo_list.has_incomplete() {
        return None;
    }
    let remaining = ctx.todo_list.incomplete_count();
    Some(format!(
        "Auto-continue paused, awaiting user ({remaining} incomplete todo(s) preserved): {reason}"
    ))
}

fn print_pause_banner(ctx: &RequestContext) {
    if let Some(text) = pause_banner_text(ctx) {
        eprintln!(
            "\n⏸ {}",
            dim_notice_color(&ctx.app.config).italic().paint(text)
        );
    }
}

fn interrupted_turn_notice(ctx: &RequestContext) -> Option<String> {
    let running: Vec<String> = pending_tasks(ctx)
        .into_iter()
        .filter(|task| !task.finished)
        .map(|task| {
            let kind = match task.kind {
                TaskKind::Agent => "agent",
                TaskKind::Job => "job",
            };
            format!("{} ({kind})", task.id)
        })
        .collect();
    if running.is_empty() {
        // Grandchild-only activity has no top-level id to list, but it is
        // what the second Ctrl-C would cancel, so the notice must still
        // agree with `latch_prompt_interrupt`.
        let has_active_tasks = ctx
            .supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.read().has_active_tasks());
        if has_active_tasks {
            return Some("Turn interrupted; background work is still running. Press Ctrl-C again to cancel it, or type to continue.".into());
        }
        return None;
    }
    Some(format!(
        "Turn interrupted; {} background task(s) still running: {}. Press Ctrl-C again to cancel them, or type to continue.",
        running.len(),
        running.join(", ")
    ))
}

fn print_interrupted_turn_notice(ctx: &RequestContext) {
    if let Some(text) = interrupted_turn_notice(ctx) {
        eprintln!(
            "\n⏸ {}",
            dim_notice_color(&ctx.app.config).italic().paint(text)
        );
    }
}

fn unknown_command(ctx: &RequestContext) -> Result<()> {
    bail!(
        "{}",
        ctx.render_hints(r#"Unknown command. Type ".help" for additional help."#)
    );
}

#[derive(Debug, PartialEq)]
enum ReplInstallDispatch<'a> {
    Builtins(AssetCategory),
    Unified(&'a str),
    Help,
    Usage,
}

fn parse_repl_install(args: Option<&str>) -> ReplInstallDispatch<'_> {
    let trimmed = args.map(str::trim).unwrap_or("");
    if trimmed
        .split_whitespace()
        .any(|token| token == "--help" || token == "-h")
    {
        return ReplInstallDispatch::Help;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    match parts.next() {
        Some(name) if !name.is_empty() => {
            let rest = parts.next().map(str::trim).unwrap_or("");
            match AssetCategory::parse(name) {
                Some(category) if rest.is_empty() => ReplInstallDispatch::Builtins(category),
                Some(_) => ReplInstallDispatch::Usage,
                None => ReplInstallDispatch::Unified(trimmed),
            }
        }
        _ => ReplInstallDispatch::Usage,
    }
}

fn repl_install_help() -> String {
    format!(
        r#"Install built-in assets, install a bundle from a Git source, or update an installed bundle.

Usage:
  .install <category>                  Reinstall built-in assets ({categories})
  .install <owner/repo>[#ref]          Install a bundle from {default_host} (change the host with --git-host)
  .install <git-url>[#ref]             Install a bundle from any Git URL, scp-style path, or local path
  .install <installed-bundle>[#ref]    Update an installed bundle from its recorded source

Flags:
  --git-host <host>   Host the <owner/repo> shorthand expands against (default {default_host})
  --filter <cat>      Restrict a remote install to one category ({filters})
  --force             Overwrite all conflicts without prompting (remote installs only)

Suffix #<ref> to pin a branch, tag, or commit. List installed bundles with
`.list bundles`; remove one with `.uninstall <name>`."#,
        categories = AssetCategory::NAMES.join("|"),
        default_host = config::DEFAULT_GIT_HOST,
        filters = config::InstallFilter::NAMES.join("|"),
    )
}

fn repl_uninstall_help() -> String {
    r#"Remove an installed bundle: delete the files it owns and the mcp.json entries it added.

Usage:
  .uninstall <bundle-name> [--yes]

Flags:
  --yes, -y   Skip the confirmation prompt

Files you modified after install are prompted for individually and kept by
default; --yes never deletes modified files. List installed bundles with
`.list bundles`."#
        .to_string()
}

#[derive(Debug, PartialEq)]
enum ReplUninstallDispatch {
    Run(String, bool),
    Help,
    Usage,
}

fn parse_repl_uninstall(args: Option<&str>) -> ReplUninstallDispatch {
    let mut assume_yes = false;
    let mut names = Vec::new();
    for token in args.unwrap_or("").split_whitespace() {
        match token {
            "--help" | "-h" => return ReplUninstallDispatch::Help,
            "--yes" | "-y" => assume_yes = true,
            other if other.starts_with('-') => return ReplUninstallDispatch::Usage,
            other => names.push(other),
        }
    }
    match names.as_slice() {
        [name] => ReplUninstallDispatch::Run(name.to_string(), assume_yes),
        _ => ReplUninstallDispatch::Usage,
    }
}

fn repl_save_session_help() -> String {
    r#"Save the current session to a file.

Usage:
  .save session [name] [--workspace|--global]

Flags:
  --workspace   Save under .coyote/ in the current workspace
  --global      Save under the global config dir

Without a flag the session is re-saved wherever it currently lives; a brand-new
session lives in the global scope. Re-homing a session never deletes the old file."#
        .to_string()
}

#[derive(Debug, PartialEq)]
enum ReplSaveSessionDispatch {
    Run {
        name: Option<String>,
        scope: Option<SessionScope>,
    },
    Help,
    Conflict,
    Usage,
}

fn parse_repl_save_session(args: Option<&str>) -> ReplSaveSessionDispatch {
    let tokens: Vec<&str> = args.unwrap_or("").split_whitespace().collect();
    if tokens
        .iter()
        .any(|token| *token == "--help" || *token == "-h")
    {
        return ReplSaveSessionDispatch::Help;
    }
    let mut scope = None;
    let mut names = Vec::new();
    for token in tokens {
        let requested = match token {
            "--workspace" => SessionScope::Workspace,
            "--global" => SessionScope::Global,
            other if other.starts_with('-') => return ReplSaveSessionDispatch::Usage,
            other => {
                names.push(other);
                continue;
            }
        };
        if scope.is_some_and(|current| current != requested) {
            return ReplSaveSessionDispatch::Conflict;
        }
        scope = Some(requested);
    }
    let name = (!names.is_empty()).then(|| names.join(" "));
    ReplSaveSessionDispatch::Run { name, scope }
}

pub fn builtin_command_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = REPL_COMMANDS
        .iter()
        .filter_map(|cmd| cmd.name.split_whitespace().next())
        .filter_map(|name| name.strip_prefix('.'))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

fn dump_repl_help(prefix: CommandPrefix) {
    println!("{}", repl_help_text(prefix));
}

fn repl_help_text(prefix: CommandPrefix) -> String {
    let head = REPL_COMMANDS
        .iter()
        .map(|cmd| format!("{:<24} {}", cmd.name, cmd.description))
        .collect::<Vec<String>>()
        .join("\n");
    let text = format!(
        r###"{head}
{:<24} Run an arbitrary shell command (stdout/stderr stream to your terminal; Ctrl+C interrupts)

Custom commands (macros): macros are coyote's custom commands. An enabled
macro <name> runs top-level as .<name> [args...], equivalent to ".macro <name>".
List them with ".list macros"; toggle them with ".macro enable|disable <name>".

Type ::: to start multi-line editing, type ::: to finish it.
Press Ctrl+O to open an editor for editing the input buffer.
Press Ctrl+L to clear the screen; type ".redraw" to clear it and bring the last conversation turn back.
Press Ctrl+C to cancel the response, Ctrl+D to exit the REPL."###,
        "!<command>",
    );
    prefix.render_hints(&text).into_owned()
}

fn parse_command(line: &str, prefix: CommandPrefix) -> Option<(Cow<'_, str>, Option<&str>)> {
    match COMMAND_RE.captures(line) {
        Ok(Some(captures)) => {
            let cmd = prefix.normalize(captures.get(1)?.as_str())?;
            let args = line[captures[0].len()..].trim();
            let args = if args.is_empty() { None } else { Some(args) };
            Some((cmd, args))
        }
        _ => None,
    }
}

fn try_extract_shell_command(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('!')?;
    Some(rest.trim_start())
}

fn handle_shell_passthrough(cmd: &str) -> Result<()> {
    if cmd.is_empty() {
        eprintln!("Usage: !<command>");
        return Ok(());
    }

    let status = run_command(&SHELL.cmd, &[&SHELL.arg, cmd], None)?;
    if status != 0 {
        eprintln!("[exit {status}]");
    }

    Ok(())
}

fn split_first_arg(args: Option<&str>) -> Option<(&str, Option<&str>)> {
    args.map(|v| match v.split_once(' ') {
        Some((subcmd, args)) => (subcmd, Some(args.trim())),
        None => (v, None),
    })
}

fn parse_prompt_call_args(words: &[String]) -> Result<HashMap<String, String>> {
    let mut args = HashMap::new();
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            bail!("Invalid prompt argument '{word}': arguments must be key=value pairs");
        };
        args.insert(key.to_string(), unquote_prompt_value(value).to_string());
    }
    Ok(args)
}

fn unquote_prompt_value(value: &str) -> &str {
    let quoted = value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if quoted {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn prompt_arg_inquire_label(server: &str, prompt: &str, arg: &str) -> String {
    format!(
        "Prompt '{}' on '{}' requires '{}':",
        sanitize_display_text(prompt),
        sanitize_display_text(server),
        sanitize_display_text(arg)
    )
}

pub fn split_args_text(line: &str, is_win: bool) -> (Vec<String>, &str) {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut unbalance: Option<char> = None;
    let mut prev_char: Option<char> = None;
    let mut text_starts_at = None;
    let unquote_word = |word: &str| {
        if ((word.starts_with('"') && word.ends_with('"'))
            || (word.starts_with('\'') && word.ends_with('\'')))
            && word.len() >= 2
        {
            word[1..word.len() - 1].to_string()
        } else {
            word.to_string()
        }
    };
    let chars: Vec<char> = line.chars().collect();

    for (i, char) in chars.iter().cloned().enumerate() {
        match unbalance {
            Some(ub_char) if ub_char == char => {
                word.push(char);
                unbalance = None;
            }
            Some(_) => {
                word.push(char);
            }
            None => match char {
                ' ' | '\t' | '\r' | '\n' => {
                    if char == '\r' && chars.get(i + 1) == Some(&'\n') {
                        continue;
                    }
                    if let Some('\\') = prev_char.filter(|_| !is_win) {
                        word.push(char);
                    } else if !word.is_empty() {
                        if word == "--" {
                            word.clear();
                            text_starts_at = Some(i + 1);
                            break;
                        }
                        words.push(unquote_word(&word));
                        word.clear();
                    }
                }
                '\'' | '"' | '`' => {
                    word.push(char);
                    unbalance = Some(char);
                }
                '\\' => {
                    if is_win || prev_char.map(|c| c == '\\').unwrap_or_default() {
                        word.push(char);
                    }
                }
                _ => {
                    word.push(char);
                }
            },
        }
        prev_char = Some(char);
    }

    if !word.is_empty() && word != "--" {
        words.push(unquote_word(&word));
    }
    let text = match text_starts_at {
        Some(start) => &line[start..],
        None => "",
    };

    (words, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConfig, MessageContent, Model, ModelData, ToolCall};
    use crate::config::{AppState, Role, RoleLike, TEMP_ROLE_NAME, WorkingMode};
    use crate::function::ToolResult;
    use crate::function::agents::AGENT_FUNCTION_PREFIX;
    use crate::function::memory::MEMORY_FUNCTION_PREFIX;
    use crate::function::user_interaction::USER_FUNCTION_PREFIX;
    use crate::graph::llm::test_support::{boxed_completion, completion_runner};
    use crate::graph::route::ROUTE_ROLE_NAME;
    use crate::hooks::{HookDef, HooksMap, test_sink};
    use anyhow::anyhow;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashSet;
    use std::future::Future;

    const TURN_EVENTS: [&str; 4] = [
        "turn.started",
        "turn.completed",
        "turn.interrupted",
        "turn.failed",
    ];

    fn ctx_with_hooks(events: &[&str], marker: &str) -> RequestContext {
        let mut hooks_map = HooksMap::default();
        for event in events {
            hooks_map.insert(
                event.to_string(),
                vec![HookDef {
                    name: format!("{marker}-{event}"),
                    command: "true".to_string(),
                }],
            );
        }
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            hooks: hooks_map,
            ..Default::default()
        });
        RequestContext::new(Arc::new(app), WorkingMode::Repl)
    }

    /// Sink captures for `marker`, ordered (started, completed, interrupted, failed).
    fn turn_counts(marker: &str) -> (usize, usize, usize, usize) {
        let captures = test_sink::snapshot();
        let count = |event: &str| {
            captures
                .iter()
                .filter(|capture| capture.hook_name == format!("{marker}-{event}"))
                .count()
        };
        (
            count("turn.started"),
            count("turn.completed"),
            count("turn.interrupted"),
            count("turn.failed"),
        )
    }

    #[tokio::test]
    #[serial]
    async fn turn_bracket_ok_final_fires_started_and_completed() {
        let _sink = test_sink::install();
        let marker = "tb-ok-x7q";
        let ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let abort_signal = create_abort_signal();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = async { Ok(()) }.await;
        bracket.finish(&ctx, &abort_signal, result).unwrap();

        assert_eq!(turn_counts(marker), (1, 1, 0, 0));
    }

    #[tokio::test]
    #[serial]
    async fn turn_bracket_tool_rounds_then_final_fire_one_turn() {
        let _sink = test_sink::install();
        let marker = "tb-rounds-p2m";
        let ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let abort_signal = create_abort_signal();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = async {
            // Simulates a turn that loops through tool rounds before the
            // final round; rounds happen inside the bracket and never re-fire.
            let mut rounds = 0;
            loop {
                rounds += 1;
                if rounds == 3 {
                    break Ok(());
                }
            }
        }
        .await;
        bracket.finish(&ctx, &abort_signal, result).unwrap();

        assert_eq!(turn_counts(marker), (1, 1, 0, 0));
    }

    #[tokio::test]
    #[serial]
    async fn turn_bracket_error_fires_failed_with_error_env() {
        let _sink = test_sink::install();
        let marker = "tb-err-k4d";
        let ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let abort_signal = create_abort_signal();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = async { Err(anyhow!("turn exploded")) }.await;
        let err = bracket.finish(&ctx, &abort_signal, result).unwrap_err();
        assert_eq!(err.to_string(), "turn exploded");

        assert_eq!(turn_counts(marker), (1, 0, 0, 1));
        let captures = test_sink::snapshot();
        let failed = captures
            .iter()
            .find(|capture| capture.hook_name == format!("{marker}-turn.failed"))
            .expect("turn.failed capture");
        assert_eq!(
            failed.envs.get("COYOTE_ERROR").map(String::as_str),
            Some("turn exploded")
        );
    }

    #[tokio::test]
    #[serial]
    async fn turn_bracket_ctrlc_error_fires_interrupted_not_failed() {
        let _sink = test_sink::install();
        let marker = "tb-ctrlc-err-j6w";
        let ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let abort_signal = create_abort_signal();
        abort_signal.set_ctrlc();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = async { Err(anyhow!("Aborted!")) }.await;
        assert!(bracket.finish(&ctx, &abort_signal, result).is_err());

        assert_eq!(turn_counts(marker), (1, 0, 1, 0));
    }

    #[tokio::test]
    #[serial]
    async fn turn_bracket_ctrlc_with_ok_exit_fires_interrupted() {
        let _sink = test_sink::install();
        let marker = "tb-ctrlc-ok-r3v";
        let ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let abort_signal = create_abort_signal();
        abort_signal.set_ctrlc();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = async { Ok(()) }.await;
        bracket.finish(&ctx, &abort_signal, result).unwrap();

        assert_eq!(turn_counts(marker), (1, 0, 1, 0));
    }

    #[tokio::test]
    #[serial]
    async fn turn_events_carry_the_role_a_session_holds() {
        let _sink = test_sink::install();
        let marker = "tb-role-m3x";
        let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        // The role lives on the session (as after `.role` inside a session);
        // fire-time resolution must find it there.
        let mut session = Session::default();
        session.set_role(Role::new("dev-role", "Prompt"));
        ctx.session = Some(session);
        let abort_signal = create_abort_signal();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = Ok(());
        bracket.finish(&ctx, &abort_signal, result).unwrap();

        let captures: Vec<_> = test_sink::snapshot()
            .into_iter()
            .filter(|capture| capture.hook_name.starts_with(marker))
            .collect();
        assert_eq!(captures.len(), 2, "{captures:?}");
        for capture in &captures {
            assert_eq!(
                capture.envs.get("COYOTE_ROLE").map(String::as_str),
                Some("dev-role"),
                "{} must carry the session-held role",
                capture.hook_name
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn turn_interrupted_carries_the_role_a_session_holds() {
        let _sink = test_sink::install();
        let marker = "tb-role-int-f7c";
        let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let mut session = Session::default();
        session.set_role(Role::new("dev-role", "Prompt"));
        ctx.session = Some(session);
        let abort_signal = create_abort_signal();
        abort_signal.set_ctrlc();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = Ok(());
        bracket.finish(&ctx, &abort_signal, result).unwrap();

        let interrupted = test_sink::snapshot()
            .into_iter()
            .find(|capture| capture.hook_name == format!("{marker}-turn.interrupted"))
            .expect("turn.interrupted capture");
        assert_eq!(
            interrupted.envs.get("COYOTE_ROLE").map(String::as_str),
            Some("dev-role"),
            "turn.interrupted must carry the session-held role"
        );
    }

    #[tokio::test]
    #[serial]
    async fn turn_failed_carries_the_role_a_session_holds() {
        let _sink = test_sink::install();
        let marker = "tb-role-fail-z2n";
        let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let mut session = Session::default();
        session.set_role(Role::new("dev-role", "Prompt"));
        ctx.session = Some(session);
        let abort_signal = create_abort_signal();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = Err(anyhow!("turn exploded"));
        bracket.finish(&ctx, &abort_signal, result).unwrap_err();

        let failed = test_sink::snapshot()
            .into_iter()
            .find(|capture| capture.hook_name == format!("{marker}-turn.failed"))
            .expect("turn.failed capture");
        assert_eq!(
            failed.envs.get("COYOTE_ROLE").map(String::as_str),
            Some("dev-role"),
            "turn.failed must carry the session-held role"
        );
    }

    #[tokio::test]
    #[serial]
    async fn turn_events_omit_the_role_var_when_no_role_is_active() {
        let _sink = test_sink::install();
        let marker = "tb-norole-q8j";
        let ctx = ctx_with_hooks(&TURN_EVENTS, marker);
        let abort_signal = create_abort_signal();

        let bracket = TurnBracket::enter(&ctx);
        let result: Result<()> = Ok(());
        bracket.finish(&ctx, &abort_signal, result).unwrap();

        let captures: Vec<_> = test_sink::snapshot()
            .into_iter()
            .filter(|capture| capture.hook_name.starts_with(marker))
            .collect();
        assert_eq!(captures.len(), 2, "{captures:?}");
        for capture in &captures {
            assert!(
                !capture.envs.contains_key("COYOTE_ROLE"),
                "{} must omit COYOTE_ROLE without an active role",
                capture.hook_name
            );
        }
    }

    /// Drives a deep REPL future to completion on a thread with extra stack
    /// headroom: nested `run_repl_command`/`compress_session` poll frames
    /// are deep in debug builds and overflow the default test-thread stack
    /// (Windows exhausts it first).
    fn run_async<F>(f: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn_scoped(scope, || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(f)
                })
                .unwrap()
                .join()
                .unwrap()
        })
    }

    const SESSION_BLOCK: &str = "session:\n  chat:\n    instructions: hi\n";
    const ECHO_GRAPH: &str = "initial_state:\n  initial_prompt: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{initial_prompt}}\"\n";
    /// An agent gates the global hooks; this admits them all.
    const ADMIT_GLOBAL_HOOKS: &str = "global_hooks:\n  - \"*\"\n";

    fn unique(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}_{nanos}")
    }

    /// A graph agent whose end node echoes `initial_prompt`.
    fn write_graph_agent(name: &str, session_block: &str) {
        let agent_dir = paths::agent_data_dir(name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("graph.yaml"),
            format!("name: {name}\n{session_block}{ECHO_GRAPH}"),
        )
        .unwrap();
    }

    fn repl_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl)
    }

    fn with_app_config(ctx: &mut RequestContext, update: impl FnOnce(&mut AppConfig)) {
        let mut config = (*ctx.app.config).clone();
        update(&mut config);
        let mut app = (*ctx.app).clone();
        app.config = Arc::new(config);
        ctx.app = Arc::new(app);
    }

    /// Engages the graph agent `name` on `ctx` with a session whose model
    /// can call tools, so the chat branch declares the block's tools.
    async fn engage_graph_agent(mut ctx: RequestContext, name: &str) -> RequestContext {
        let app = ctx.app.config.clone();
        ctx.use_agent(&app, name, None, create_abort_signal())
            .await
            .unwrap();
        let mut session = Session::new_from_ctx(&ctx, &app, "graph-chat").unwrap();
        session.set_model(fc_model(SESSION_MODEL));
        ctx.session = Some(session);
        ctx
    }

    const SESSION_MODEL: &str = "fc-model";
    const CHAT_AGENT_MODEL: &str = "delegate-model";

    fn fc_model(name: &str) -> Model {
        let mut data = ModelData::new(name);
        data.supports_function_calling = true;
        Model::from_config("openai", &[data]).remove(0)
    }

    fn plain_model(name: &str) -> Model {
        Model::from_config("openai", &[ModelData::new(name)]).remove(0)
    }

    /// The chat agent's agent-local tool. It writes the shim's agent name,
    /// whether that name resolves on the tool's PATH to a shim inside the
    /// agent's own `agents/<name>/bin` dir, and the `tone` variable, so one
    /// call shows where it ran and whose variables it saw. The middle
    /// field is reduced to [`SHIM_ON_PATH`] so the expectation holds on
    /// every platform: the shim is `<name>.cmd` on Windows and the path
    /// bash prints there is MSYS-converted, so neither the suffix nor the
    /// path shape can be pinned. A miss echoes what did resolve.
    const CHAT_AGENT_TOOLS_SH: &str = "#!/usr/bin/env bash\n\
# @describe Agent-local tools\n\
\n\
# @cmd Probe the agent\n\
local_hello() {\n\
  resolved=$(command -v \"$LLM_AGENT_NAME\" || command -v \"$LLM_AGENT_NAME.cmd\")\n\
  resolved=${resolved//\\\\//}\n\
  dir=${resolved%/*}\n\
  base=${resolved##*/}\n\
  base=${base%.cmd}\n\
  if [[ \"$dir\" == */agents/\"$LLM_AGENT_NAME\"/bin && \"$base\" == \"$LLM_AGENT_NAME\" ]]; then\n\
    where=shim-on-path\n\
  else\n\
    where=\"missing:${resolved:-none}\"\n\
  fi\n\
  echo \"$LLM_AGENT_NAME|$where|$LLM_AGENT_VAR_TONE\" >> \"$LLM_OUTPUT\"\n\
}\n";

    /// What `local_hello` writes in its middle field when the chat agent's
    /// own shim resolved from its `agents/<name>/bin` dir.
    const SHIM_ON_PATH: &str = "shim-on-path";

    /// The chat agent's configuration: its own global tool, spawning (so
    /// `agent__*` is in its pool), auto-continue, and a declared `tone`
    /// variable the block pre-sets.
    const CHAT_AGENT_BODY: &str = "instructions: Be brief.\nglobal_tools:\n  - b_probe.sh\ncan_spawn_agents: true\nauto_continue: true\nvariables:\n  - name: tone\n    description: How to speak\n";

    /// A stub global tool `<stem>.sh`, declared as `<stem>`.
    fn write_global_tool(stem: &str) {
        let tools_dir = paths::global_tools_dir();
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::write(
            tools_dir.join(format!("{stem}.sh")),
            format!(
                "#!/usr/bin/env bash\n# @describe Probe {stem}.\n# @option --path! The file\nmain() {{ echo \"$argc_path\"; }}\n"
            ),
        )
        .unwrap();
    }

    /// A config agent for a graph agent's block to delegate its chat turns
    /// to, carrying `body` after the name. It names no model, so it inherits
    /// the context's model when engaged; its global tool is written too.
    fn write_chat_agent(name: &str, body: &str) {
        write_global_tool("b_probe");
        let agent_dir = paths::agent_data_dir(name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.yaml"),
            format!("name: {name}\n{body}"),
        )
        .unwrap();
        std::fs::write(agent_dir.join("tools.sh"), CHAT_AGENT_TOOLS_SH).unwrap();
    }

    /// A graph agent with its own global tool whose block delegates the chat
    /// turns to `chat_agent` with `tone` pre-set; `prefix` adds top-level
    /// graph keys.
    fn write_delegating_graph_agent(name: &str, prefix: &str, chat_agent: &str) {
        write_global_tool("a_probe");
        write_graph_agent(
            name,
            &format!(
                "{prefix}global_tools:\n  - a_probe.sh\nsession:\n  chat:\n    agent: {chat_agent}\n    variables:\n      tone: terse\n"
            ),
        );
    }

    fn write_global_memory_store() {
        let index = paths::global_memory_index_file();
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        std::fs::write(index, "# Memory\n- The user prefers terse answers.\n").unwrap();
    }

    /// Engages the delegating graph agent `name` on `ctx` with a session
    /// seated through `use_agent`, so the chat agent's variables and the
    /// session's prompt are seated as a user's engage seats them. The chat
    /// agent inherits the context's model; the session is then moved to
    /// `session_model`, so the runner can tell the two apart.
    async fn engage_delegating_graph_agent(
        mut ctx: RequestContext,
        name: &str,
        session_model: Model,
    ) -> RequestContext {
        ctx.model = fc_model(CHAT_AGENT_MODEL);
        let app = ctx.app.config.clone();
        ctx.use_agent(&app, name, Some("s"), create_abort_signal())
            .await
            .unwrap();
        ctx.session.as_mut().unwrap().set_model(session_model);
        ctx
    }

    /// Runs the chat agent's `local_hello` through the tool path the model
    /// would take, returning what the tool wrote.
    async fn eval_local_hello(ctx: &mut RequestContext) -> Result<String> {
        let value = ToolCall::new("local_hello".into(), json!({}), None)
            .eval(ctx)
            .await?;
        Ok(value["output"]
            .as_str()
            .unwrap_or_else(|| panic!("{value}"))
            .trim()
            .to_string())
    }

    /// Gate for the tests that run the chat agent's bash tools. A bare
    /// `bash` lookup would count the WSL launcher Windows keeps in System32
    /// as a shell; on unix the skip is never silent.
    fn bash_and_jq_available() -> bool {
        let jq_available = which::which("jq").is_ok();
        if cfg!(unix) {
            assert!(
                crate::testing::bash_program().is_some(),
                "bash must resolve on unix; a silent skip would hide these tests"
            );
            assert!(
                jq_available,
                "jq must be on PATH on unix; a silent skip would hide these tests"
            );
        }
        if crate::testing::bash_program().is_none() || !jq_available {
            eprintln!("skipping: bash/jq not available");
            return false;
        }
        true
    }

    /// Asserts the graph agent is back in the agent slot with the chat
    /// agent parked beside it.
    fn assert_graph_agent_restored(ctx: &RequestContext, name: &str, chat_agent: &str) {
        assert_eq!(ctx.agent.as_ref().unwrap().name(), name);
        let delegate = ctx.delegate.as_ref().expect("the delegate is kept");
        assert!(delegate.parked.is_none());
        assert_eq!(delegate.agent.name(), chat_agent);
    }

    type SeenInputs = Arc<parking_lot::Mutex<Vec<Input>>>;

    /// Answers the route classifier with `decision` and chat calls from
    /// `replies` in order, recording every input it is handed.
    fn scripted_runner(
        decision: &'static str,
        replies: Vec<(&'static str, Vec<ToolResult>)>,
        seen: SeenInputs,
    ) -> impl for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a> + Send
    {
        let mut replies = replies.into_iter();
        completion_runner(move |input, _ctx, _abort| {
            seen.lock().push(input.clone());
            let reply = if input.role().name() == ROUTE_ROLE_NAME {
                Ok((format!(r#"{{"decision":"{decision}"}}"#), vec![]))
            } else {
                match replies.next() {
                    Some((text, results)) => Ok((text.to_string(), results)),
                    None => Err(anyhow!("no scripted chat reply left")),
                }
            };
            boxed_completion(async move { reply })
        })
    }

    fn route_calls(seen: &[Input]) -> usize {
        seen.iter()
            .filter(|input| input.role().name() == ROUTE_ROLE_NAME)
            .count()
    }

    fn chat_inputs(seen: &[Input]) -> Vec<&Input> {
        seen.iter()
            .filter(|input| input.role().name() != ROUTE_ROLE_NAME)
            .collect()
    }

    async fn turn(
        ctx: &mut RequestContext,
        text: &str,
        forced: Option<Branch>,
        runner: &mut CompletionRunner<'_>,
    ) -> Result<()> {
        let input = Input::from_str(ctx, text, None).unwrap();
        ask_with_branch_with_runner(ctx, create_abort_signal(), input, false, forced, runner).await
    }

    /// A run turn through the REPL, leaving the session with the run's
    /// exchange and record; returns the inputs the runner saw.
    async fn run_turn(ctx: &mut RequestContext, text: &str) -> Vec<Input> {
        let seen: SeenInputs = Default::default();
        let mut runner = scripted_runner("run", vec![], Arc::clone(&seen));
        turn(ctx, text, None, &mut runner).await.unwrap();
        seen.lock().clone()
    }

    fn capture_for(marker: &str, event: &str) -> test_sink::Capture {
        test_sink::snapshot()
            .into_iter()
            .find(|capture| capture.hook_name == format!("{marker}-{event}"))
            .unwrap_or_else(|| panic!("{marker}-{event} capture"))
    }

    fn graph_envs(capture: &test_sink::Capture) -> (Option<&str>, Option<&str>) {
        (
            capture.envs.get("COYOTE_GRAPH_BRANCH").map(String::as_str),
            capture.envs.get("COYOTE_GRAPH_ROUTE").map(String::as_str),
        )
    }

    /// The session's exchange messages: everything after the role's system
    /// prompt, as (role, text) pairs.
    fn exchange(ctx: &RequestContext) -> Vec<(MessageRole, String)> {
        ctx.session
            .as_ref()
            .unwrap()
            .messages()
            .iter()
            .skip_while(|m| m.role.is_system())
            .map(|m| (m.role, m.content.to_text()))
            .collect()
    }

    #[test]
    #[serial]
    fn graph_run_turn_records_the_exchange_and_the_last_run() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;

            let seen = run_turn(&mut ctx, "do the thing").await;

            assert_eq!(route_calls(&seen), 1);
            assert!(chat_inputs(&seen).is_empty());
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
            let session = ctx.session.as_ref().unwrap();
            let messages = session.messages();
            let [.., user, assistant] = messages else {
                panic!("expected the run exchange, got {messages:?}");
            };
            assert!(user.role.is_user());
            assert_eq!(user.content.to_text(), "do the thing");
            assert!(assistant.role.is_assistant());
            assert_eq!(assistant.content.to_text(), "do the thing");
            let last_run = session.last_run().expect("the run is recorded");
            assert_eq!(last_run.prompt, "do the thing");
            assert_eq!(last_run.output, "do the thing");
            assert_eq!(last_run.recorded_at_message_count, messages.len());
        });
    }

    #[test]
    #[serial]
    fn graph_run_turn_writes_the_last_run_to_an_autosaving_session_file() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_flush");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let path = ctx.session_file_for("graph-chat", SessionScope::Workspace);
            let session = ctx.session.as_mut().unwrap();
            session.save("graph-chat", &path, false).unwrap();
            session.set_save_session(Some(true));
            let writes_before = Session::save_temp_seq();

            run_turn(&mut ctx, "do the thing").await;
            let writes = Session::save_temp_seq() - writes_before;

            let on_disk: Session =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let last_run = on_disk.last_run().expect("the run is written to disk");
            assert_eq!(last_run.prompt, "do the thing");
            assert_eq!(last_run.recorded_at_message_count, on_disk.messages().len());
            assert!(!ctx.session.as_ref().unwrap().dirty());
            assert_eq!(
                writes, 1,
                "the exchange and the run record land in one write"
            );
        });
    }

    /// The run arm's single write obeys the same gate as every other
    /// funnel: `None`/`Some(false)` sessions keep the run record in memory
    /// only, while the one-shot `--save-session` flag writes it like
    /// `save_session: true` does.
    #[test]
    #[serial]
    fn usage_probe_graph_run_turn_writes_nothing_unless_the_session_autosaves() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_flush_off");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            for save_session in [None, Some(false)] {
                let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
                let path = ctx.session_file_for("graph-chat", SessionScope::Workspace);
                let session = ctx.session.as_mut().unwrap();
                session.set_save_session(save_session);
                session.save("graph-chat", &path, false).unwrap();
                let before = std::fs::read_to_string(&path).unwrap();
                let writes_before = Session::save_temp_seq();

                run_turn(&mut ctx, "do the thing").await;

                assert_eq!(
                    Session::save_temp_seq() - writes_before,
                    0,
                    "save_session {save_session:?} must not write the run"
                );
                assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
                let session = ctx.session.as_ref().unwrap();
                assert!(session.dirty());
                assert_eq!(
                    session.last_run().map(|run| run.prompt.as_str()),
                    Some("do the thing"),
                    "the record still lives in memory"
                );
                std::fs::remove_file(&path).unwrap();
            }

            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let path = ctx.session_file_for("graph-chat", SessionScope::Workspace);
            ctx.session
                .as_mut()
                .unwrap()
                .save("graph-chat", &path, false)
                .unwrap();
            assert_eq!(ctx.session.as_ref().unwrap().save_session(), None);
            ctx.set_save_session_this_time().unwrap();
            let writes_before = Session::save_temp_seq();

            run_turn(&mut ctx, "do it once").await;

            assert_eq!(Session::save_temp_seq() - writes_before, 1);
            let on_disk: Session =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                on_disk.last_run().map(|run| run.prompt.as_str()),
                Some("do it once")
            );
            assert!(!ctx.session.as_ref().unwrap().dirty());
        });
    }

    /// A detached input (`.role <name> <text>` on a graph agent) records
    /// nothing in the session, so the Run arm must not flush whatever the
    /// session already holds dirty, mirroring `after_chat_completion`.
    #[test]
    #[serial]
    fn graph_run_turn_on_a_detached_input_does_not_checkpoint_the_session() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_detached");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let path = ctx.session_file_for("graph-chat", SessionScope::Workspace);
            let session = ctx.session.as_mut().unwrap();
            session.save("graph-chat", &path, false).unwrap();
            // Flipping the setting after the save leaves the session dirty.
            session.set_save_session(Some(true));
            assert!(session.dirty());
            let before = std::fs::read_to_string(&path).unwrap();
            let writes_before = Session::save_temp_seq();
            let app = Arc::clone(&ctx.app.config);
            let role = ctx.extract_role(app.as_ref()).unwrap();
            let input = Input::from_str(&ctx, "do the thing", Some(role)).unwrap();
            assert!(input.session(&ctx.session).is_none());

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![], Arc::clone(&seen));
            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            assert_eq!(Session::save_temp_seq() - writes_before, 0);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            let session = ctx.session.as_ref().unwrap();
            assert!(session.dirty());
            assert!(
                session.last_run().is_none(),
                "nothing is recorded for a detached run"
            );
        });
    }

    /// A chat turn on a routed graph agent costs exactly one write, which
    /// carries the chat exchange. The route classifier runs first on a
    /// session that is already dirty, but its reply stays on the runner
    /// path (only its extractor fallback reaches `after_chat_completion`,
    /// with a detached input), so it contributes no write.
    #[test]
    #[serial]
    fn usage_probe_graph_chat_turn_lands_on_disk_in_one_write() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_chat_flush");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let path = ctx.session_file_for("graph-chat", SessionScope::Workspace);
            let session = ctx.session.as_mut().unwrap();
            session.save("graph-chat", &path, false).unwrap();
            // Flipping the setting after the save leaves the session dirty
            // when the classifier runs, so a classifier flush would show.
            session.set_save_session(Some(true));
            assert!(session.dirty());
            let seen: SeenInputs = Default::default();
            let mut runner =
                scripted_runner("chat", vec![("the answer", vec![])], Arc::clone(&seen));
            let writes_before = Session::save_temp_seq();

            turn(&mut ctx, "talk to me", None, &mut runner)
                .await
                .unwrap();

            assert_eq!(route_calls(&seen.lock()), 1, "the classifier did run");
            assert_eq!(
                Session::save_temp_seq() - writes_before,
                1,
                "one write for the chat exchange, none for the classifier"
            );
            let on_disk: Session =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let texts: Vec<String> = on_disk
                .messages()
                .iter()
                .map(|m| m.content.to_text())
                .collect();
            assert!(
                texts.iter().any(|t| t == "talk to me") && texts.iter().any(|t| t == "the answer"),
                "{texts:?}"
            );
            assert!(
                !texts.iter().any(|t| t.contains("decision")),
                "the classifier's exchange is not in the session: {texts:?}"
            );
            assert!(on_disk.last_run().is_none());
            assert!(!ctx.session.as_ref().unwrap().dirty());
        });
    }

    #[test]
    #[serial]
    fn graph_run_turn_under_dry_run_records_nothing() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_dry_run");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            with_app_config(&mut ctx, |config| config.dry_run = true);

            let seen = run_turn(&mut ctx, "do the thing").await;

            assert_eq!(route_calls(&seen), 1);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
            let session = ctx.session.as_ref().unwrap();
            assert!(session.messages().is_empty());
            assert!(session.last_run().is_none());
        });
    }

    /// Dispatches `line` as a REPL command. Boxed: `run_repl_command`'s
    /// state machine is far larger than a test thread's stack.
    async fn repl_command(ctx: &mut RequestContext, line: &str) -> Result<bool> {
        Box::pin(run_repl_command(ctx, create_abort_signal(), line)).await
    }

    fn write_config_agent(name: &str) {
        let agent_dir = paths::agent_data_dir(name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.yaml"),
            format!("name: {name}\ninstructions: hi\n"),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn run_command_without_an_agent_bails() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        run_async(async {
            let mut ctx = repl_ctx();

            let err = repl_command(&mut ctx, ".run x").await.unwrap_err();

            assert_eq!(err.to_string(), "No agent");
        });
    }

    #[test]
    #[serial]
    fn run_command_rejects_a_config_agent() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_cmd_config");
        write_config_agent(&name);
        run_async(async {
            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();

            let err = repl_command(&mut ctx, ".run x").await.unwrap_err();

            assert_eq!(
                err.to_string(),
                format!(".run only applies to graph-based agents; '{name}' is a config agent")
            );
        });
    }

    #[test]
    #[serial]
    fn run_command_without_a_prompt_prints_usage_and_runs_nothing() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_cmd_usage");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;

            repl_command(&mut ctx, ".run").await.unwrap();

            assert_eq!(ctx.last_graph_branch, None);
            assert!(ctx.session.as_ref().unwrap().messages().is_empty());
        });
    }

    /// A forced run never calls the model, so the real runner serves.
    #[test]
    #[serial]
    fn run_command_forces_the_run_branch_and_records_the_exchange() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_cmd");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;

            repl_command(&mut ctx, ".run hello").await.unwrap();

            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, true)));
            let session = ctx.session.as_ref().unwrap();
            let messages = session.messages();
            let [.., user, assistant] = messages else {
                panic!("expected the run exchange, got {messages:?}");
            };
            assert!(user.role.is_user());
            assert_eq!(user.content.to_text(), "hello");
            assert!(assistant.role.is_assistant());
            assert_eq!(assistant.content.to_text(), "hello");
            assert_eq!(session.last_run().unwrap().prompt, "hello");
        });
    }

    /// Without a session block there is no session and no branch to
    /// force: the graph simply runs and no branch is recorded.
    #[test]
    #[serial]
    fn run_command_on_a_blockless_graph_records_no_branch() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_cmd_blockless");
        write_graph_agent(&name, "");
        run_async(async {
            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            assert!(ctx.session.is_none());

            repl_command(&mut ctx, ".run hello").await.unwrap();

            assert_eq!(ctx.last_graph_branch, None);
            assert!(ctx.session.is_none());
        });
    }

    /// `.run <prompt>` opens a fresh turn the way a plain prompt does: a
    /// paused auto-continue is resumed and the continuation count starts
    /// over, and the session copy of that state follows, so a later save
    /// does not carry the stale pause.
    #[test]
    #[serial]
    fn run_command_resumes_a_paused_auto_continue_like_a_plain_prompt() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_cmd_resume");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            ctx.init_todo_list("Ship feature");
            ctx.add_todo("Write code");
            ctx.auto_continue_count = 3;
            ctx.pause_auto_continue("Which database?");
            assert_eq!(
                ctx.session.as_ref().unwrap().auto_continue_paused(),
                Some("Which database?")
            );

            repl_command(&mut ctx, ".run hello").await.unwrap();

            assert_eq!(ctx.auto_continue_paused, None);
            assert_eq!(ctx.auto_continue_count, 0);
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.auto_continue_paused(), None);
            assert_eq!(session.last_run().unwrap().prompt, "hello");
        });
    }

    /// The two `.run` paths that run no turn (the usage hint and the
    /// config-agent refusal) touch no turn state: a paused auto-continue
    /// stays paused and the continuation count is kept.
    #[test]
    #[serial]
    fn run_command_paths_that_run_nothing_leave_the_auto_continue_state_alone() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let graph = unique("repl_run_cmd_keep_graph");
        write_graph_agent(&graph, SESSION_BLOCK);
        let config = unique("repl_run_cmd_keep_config");
        write_config_agent(&config);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &graph).await;
            ctx.auto_continue_count = 3;
            ctx.pause_auto_continue("Which database?");

            repl_command(&mut ctx, ".run").await.unwrap();

            assert_eq!(ctx.auto_continue_paused.as_deref(), Some("Which database?"));
            assert_eq!(ctx.auto_continue_count, 3);

            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &config, None, create_abort_signal())
                .await
                .unwrap();
            ctx.auto_continue_count = 3;
            ctx.pause_auto_continue("Which database?");

            repl_command(&mut ctx, ".run x").await.unwrap_err();

            assert_eq!(ctx.auto_continue_paused.as_deref(), Some("Which database?"));
            assert_eq!(ctx.auto_continue_count, 3);
        });
    }

    #[test]
    #[serial]
    fn undo_in_a_block_graph_agent_pops_the_run_exchange_and_its_record() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_undo_run");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            run_turn(&mut ctx, "do the thing").await;
            assert!(ctx.session.as_ref().unwrap().last_run().is_some());

            repl_command(&mut ctx, ".undo").await.unwrap();

            let session = ctx.session.as_ref().unwrap();
            assert!(!session.has_user_messages());
            assert!(session.last_run().is_none());
        });
    }

    /// The session-block gate answers before the "No session" bail would.
    #[test]
    #[serial]
    fn undo_in_a_blockless_graph_agent_bails_with_the_teaching_text() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_undo_blockless");
        write_graph_agent(&name, "");
        run_async(async {
            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            assert!(ctx.session.is_none());

            let err = repl_command(&mut ctx, ".undo").await.unwrap_err();

            assert_eq!(
                err.to_string(),
                format!(
                    "Graph-based agent '{name}' has no `session:` block in its graph.yaml, so it cannot hold a session. Add a `session:` block (see the Graph Agents wiki page, \"Sessions\") to enable one."
                )
            );
        });
    }

    /// A multimodal user message has no single text to prefill, so the
    /// exchange is not undoable; the one existing bail covers it.
    #[test]
    #[serial]
    fn undo_after_a_multimodal_exchange_bails_with_nothing_to_undo() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_undo_multimodal");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            // Hand-built: no REPL path records a multimodal user message
            // under dry-run, so the session is written directly.
            ctx.session = Some(
                serde_yaml::from_str(indoc! {"
                    model: openai:fc-model
                    messages:
                      - role: user
                        content:
                          - type: text
                            text: describe this
                          - type: image_url
                            image_url:
                              url: data:image/png;base64,AAAA
                      - role: assistant
                        content: a picture
                "})
                .unwrap(),
            );

            let err = repl_command(&mut ctx, ".undo").await.unwrap_err();

            assert_eq!(err.to_string(), "Nothing to undo");
            assert_eq!(ctx.session.as_ref().unwrap().messages().len(), 2);
        });
    }

    /// Undoing a chat turn that followed the run pops only that exchange:
    /// the run's exchange and its `last_run` record stay, since the record
    /// is cleared only when the undone turn was the run itself.
    #[test]
    #[serial]
    fn usage_probe_undo_after_a_chat_turn_in_a_block_graph_agent_keeps_the_run_record() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_undo_chat_keeps_run");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            run_turn(&mut ctx, "do the thing").await;
            let after_run = ctx.session.as_ref().unwrap().messages().len();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("about it", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();
            assert_eq!(
                ctx.session.as_ref().unwrap().messages().len(),
                after_run + 2
            );

            repl_command(&mut ctx, ".undo").await.unwrap();

            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), after_run);
            let [.., user, assistant] = session.messages() else {
                panic!("the run exchange must survive the undo");
            };
            assert_eq!(user.content.to_text(), "do the thing");
            assert_eq!(assistant.content.to_text(), "do the thing");
            assert_eq!(
                session.last_run().map(|run| run.prompt.as_str()),
                Some("do the thing"),
                "undoing the chat turn must not clear the run's record"
            );
        });
    }

    /// The REPL `.session <name>` arm follows the graph's `session:` block:
    /// it engages a session inside a graph agent that carries one, and
    /// bails with the teaching text (the same sentence `.undo` and
    /// `use_agent` use) inside one that does not.
    #[test]
    #[serial]
    fn usage_probe_session_command_follows_the_session_block_inside_a_graph_agent() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let block = unique("repl_session_cmd_block");
        write_graph_agent(&block, SESSION_BLOCK);
        let blockless = unique("repl_session_cmd_blockless");
        write_graph_agent(&blockless, "");
        run_async(async {
            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &block, None, create_abort_signal())
                .await
                .unwrap();
            assert!(ctx.session.is_none());

            repl_command(&mut ctx, ".session s").await.unwrap();

            assert_eq!(ctx.session.as_ref().map(|s| s.name()), Some("s"));
            assert_eq!(ctx.agent.as_ref().map(|a| a.name()), Some(block.as_str()));

            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &blockless, None, create_abort_signal())
                .await
                .unwrap();

            let err = repl_command(&mut ctx, ".session s").await.unwrap_err();

            assert_eq!(
                err.to_string(),
                format!(
                    "Graph-based agent '{blockless}' has no `session:` block in its graph.yaml, so it cannot hold a session. Add a `session:` block (see the Graph Agents wiki page, \"Sessions\") to enable one."
                )
            );
            assert!(ctx.session.is_none());
        });
    }

    /// A `.run` line whose argument is only whitespace has no prompt: it is
    /// the usage hint, not a graph run with an empty prompt.
    #[test]
    #[serial]
    fn usage_probe_run_command_with_only_whitespace_is_the_usage_hint() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_cmd_blank");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;

            for line in [".run ", ".run    ", ".run\t"] {
                repl_command(&mut ctx, line).await.unwrap();

                assert_eq!(ctx.last_graph_branch, None, "{line:?} must run nothing");
                assert!(
                    ctx.session.as_ref().unwrap().messages().is_empty(),
                    "{line:?} must record no exchange"
                );
                assert!(
                    ctx.session.as_ref().unwrap().last_run().is_none(),
                    "{line:?} must record no run"
                );
            }
        });
    }

    /// The chat request inlines the last run ahead of the user's text; the
    /// session stores the text as typed, under the block's instructions.
    #[test]
    #[serial]
    fn graph_chat_turn_sends_the_preamble_and_stores_the_bare_text() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_chat");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            run_turn(&mut ctx, "do the thing").await;
            let before = ctx.session.as_ref().unwrap().messages().len();

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("about it", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            let request = chats[0];
            assert!(request.with_session());
            let content = request.message_content().to_text();
            assert!(
                content.starts_with("Last graph run (completed "),
                "{content}"
            );
            assert!(content.contains("do the thing"), "{content}");
            assert!(content.ends_with("\n\nwhat happened?"), "{content}");
            let messages = request.build_messages().unwrap();
            assert!(matches!(messages[0].role, MessageRole::System));
            assert_eq!(messages[0].content.to_text(), "hi");

            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));
            let session = ctx.session.as_ref().unwrap();
            let stored = session.messages();
            assert_eq!(stored.len(), before + 2);
            let [.., user, assistant] = stored else {
                panic!("expected the chat exchange");
            };
            assert_eq!(user.content.to_text(), "what happened?");
            assert_eq!(assistant.content.to_text(), "about it");
            assert!(
                !stored
                    .iter()
                    .any(|m| m.content.to_text().contains("Last graph run")),
                "{stored:?}"
            );
            assert_eq!(
                session.last_run().unwrap().prompt,
                "do the thing",
                "a chat turn leaves the record alone"
            );
        });
    }

    /// A chat reply with tool calls loops through its tool rounds inside the
    /// same turn: one classification, no graph run.
    #[test]
    #[serial]
    fn graph_chat_turn_tool_rounds_are_not_reclassified() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_tools");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let results = vec![ToolResult::new(
                ToolCall::new("t".into(), json!({}), Some("id-1".into())),
                json!("ok"),
            )];
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner(
                "chat",
                vec![("calling a tool", results), ("done", vec![])],
                Arc::clone(&seen),
            );

            turn(&mut ctx, "look it up", None, &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 2);
            assert!(chats[0].tool_calls().is_none());
            assert!(chats[1].tool_calls().is_some());
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));
            let session = ctx.session.as_ref().unwrap();
            assert!(session.last_run().is_none(), "the graph never ran");
            assert_eq!(session.messages().last().unwrap().content.to_text(), "done");
        });
    }

    /// `.regenerate` replays `last_message.input`: after a chat turn the
    /// last answer is replaced in place; after a run turn the graph runs
    /// again and its exchange replaces the last answer the same way.
    #[test]
    #[serial]
    fn graph_regenerate_replays_through_the_router() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_regen");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let app = Arc::clone(&ctx.app.config);
            run_turn(&mut ctx, "do the thing").await;
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("first", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();
            let before = ctx.session.as_ref().unwrap().messages().len();

            let mut replay = ctx.last_message.as_ref().unwrap().input.clone();
            replay.set_regenerate(ctx.extract_role(&app).unwrap());
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("second", vec![])], Arc::clone(&seen));
            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                replay,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            assert!(chat_inputs(&seen)[0].regenerate());
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), before);
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "second"
            );

            let seen = run_turn(&mut ctx, "do it again").await;
            assert_eq!(route_calls(&seen), 1);
            let before = ctx.session.as_ref().unwrap().messages().len();
            let mut replay = ctx.last_message.as_ref().unwrap().input.clone();
            replay.set_regenerate(ctx.extract_role(&app).unwrap());
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![], Arc::clone(&seen));
            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                replay,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            assert_eq!(route_calls(&seen.lock()), 1, "the graph is re-routed");
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), before);
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "do it again"
            );
            let last_run = session.last_run().unwrap();
            assert_eq!(last_run.prompt, "do it again");
            assert_eq!(last_run.recorded_at_message_count, before);
        });
    }

    #[test]
    #[serial]
    fn graph_continue_keeps_the_partial_output_on_the_chat_branch() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_continue");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("partial", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "tell me", None, &mut runner).await.unwrap();

            let LastMessage {
                mut input, output, ..
            } = ctx.last_message.clone().unwrap();
            assert_eq!(output, "partial");
            input.set_continue_output(&output);
            let seen: SeenInputs = Default::default();
            let mut runner =
                scripted_runner("chat", vec![(" and more", vec![])], Arc::clone(&seen));
            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            let seen = seen.lock().clone();
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            assert_eq!(chats[0].continue_output(), Some("partial"));
            assert_eq!(
                ctx.session
                    .as_ref()
                    .unwrap()
                    .messages()
                    .last()
                    .unwrap()
                    .content
                    .to_text(),
                "partial and more"
            );
        });
    }

    /// A `.continue` the classifier sends to the run branch is a fresh
    /// exchange: the graph output is recorded as a new pair, not appended
    /// onto the reply the continue was extending.
    #[test]
    #[serial]
    fn graph_continue_routed_to_run_records_a_fresh_exchange() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_continue_run");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("partial", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "tell me", None, &mut runner).await.unwrap();
            let before = ctx.session.as_ref().unwrap().messages().len();

            let LastMessage {
                mut input, output, ..
            } = ctx.last_message.clone().unwrap();
            assert_eq!(output, "partial");
            input.set_continue_output(&output);
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![], Arc::clone(&seen));
            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            assert_eq!(route_calls(&seen.lock()), 1);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
            let session = ctx.session.as_ref().unwrap();
            let messages = session.messages();
            assert_eq!(messages.len(), before + 2, "{messages:?}");
            assert_eq!(messages[before - 1].content.to_text(), "partial");
            let [.., user, assistant] = messages else {
                panic!("expected the run exchange, got {messages:?}");
            };
            assert!(user.role.is_user());
            assert_eq!(user.content.to_text(), "tell me");
            assert!(assistant.role.is_assistant());
            assert_eq!(assistant.content.to_text(), "tell me");
            let last_run = session.last_run().expect("the run is recorded");
            assert_eq!(last_run.recorded_at_message_count, messages.len());
        });
    }

    /// A client that cannot be built fails the turn before any request:
    /// the caller sees the bare error and the session is untouched.
    #[test]
    fn preflight_failure_returns_the_bare_error_and_leaves_the_session_alone() {
        run_async(async {
            let mut ctx = repl_ctx();
            ctx.session = Some(Session::default());
            let input = Input::from_str(&ctx, "hi", None).unwrap();
            let expected = match input.create_client() {
                Ok(_) => panic!("the test AppState has no model to build a client for"),
                Err(err) => err.to_string(),
            };
            assert!(expected.contains("Invalid model"), "{expected}");

            let err = ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut repl_completion_runner(),
            )
            .await
            .unwrap_err();

            assert_eq!(err.to_string(), expected);
            assert!(err.downcast_ref::<PreflightError>().is_none());
            assert!(ctx.last_message.is_none());
            let session = ctx.session.as_ref().unwrap();
            assert!(session.messages().is_empty());
            assert!(!session.dirty());
        });
    }

    #[test]
    fn preflight_error_delegates_display_and_source() {
        let inner = || anyhow!("root cause").context("outer");
        let marked = anyhow::Error::new(PreflightError(inner()));

        assert_eq!(marked.to_string(), inner().to_string());
        assert_eq!(format!("{marked:#}"), format!("{:#}", inner()));
        let source = std::error::Error::source(&*marked).expect("the inner error's source");
        assert_eq!(source.to_string(), "root cause");
    }

    #[test]
    fn select_transport_quiets_the_classifier_and_keeps_the_streaming_split() {
        let mut ctx = repl_ctx();
        for stream in [true, false] {
            with_app_config(&mut ctx, |config| config.stream = stream);
            let route = Input::detached_with_role(&ctx, "q", Role::new(ROUTE_ROLE_NAME, "p"));
            assert_eq!(route.stream(), stream);
            assert!(matches!(select_transport(&route), Transport::Quiet));

            let other = Input::detached_with_role(&ctx, "q", Role::new("other", "p"));
            assert_eq!(other.stream(), stream);
            if stream {
                assert!(matches!(select_transport(&other), Transport::Streaming));
            } else {
                assert!(matches!(select_transport(&other), Transport::Plain));
            }
        }
    }

    #[test]
    fn routing_line_names_the_branch() {
        assert!(graph::route::routing_line(Branch::Run).contains("routing: run"));
        assert!(graph::route::routing_line(Branch::Chat).contains("routing: chat"));
    }

    #[test]
    #[serial]
    fn graph_turn_events_carry_the_branch_and_how_it_was_chosen() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_hooks");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-branch-v3k";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;

            test_sink::drain();
            run_turn(&mut ctx, "do the thing").await;
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            let started = capture_for(marker, "turn.started");
            assert_eq!(graph_envs(&started), (None, None));
            let completed = capture_for(marker, "turn.completed");
            assert_eq!(graph_envs(&completed), (Some("run"), Some("classified")));

            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("about it", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.started")),
                (None, None)
            );
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.completed")),
                (Some("chat"), Some("classified"))
            );

            // A forced run never calls the model, so the real runner serves.
            test_sink::drain();
            let input = Input::from_str(&ctx, "do it again", None).unwrap();
            ask_with_branch(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                Some(Branch::Run),
            )
            .await
            .unwrap();
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.completed")),
                (Some("run"), Some("forced"))
            );
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, true)));
        });
    }

    #[test]
    #[serial]
    fn turn_events_omit_the_graph_vars_without_a_graph_agent() {
        let _sink = test_sink::install();
        run_async(async {
            let marker = "gt-nograph-h5r";
            let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
            ctx.session = Some(Session::default());
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![("hello", vec![])], Arc::clone(&seen));

            turn(&mut ctx, "hi", None, &mut runner).await.unwrap();

            assert_eq!(route_calls(&seen.lock()), 0);
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            for event in ["turn.started", "turn.completed"] {
                assert_eq!(
                    graph_envs(&capture_for(marker, event)),
                    (None, None),
                    "{event}"
                );
            }
            assert_eq!(ctx.last_graph_branch, None);
        });
    }

    /// A config agent has no graph, so its turns are never routed: the
    /// classifier is not called and neither turn event carries a branch.
    #[test]
    #[serial]
    fn turn_events_omit_the_graph_vars_with_a_config_agent() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_config_agent");
        let agent_dir = paths::agent_data_dir(&name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.yaml"),
            format!("name: {name}\ninstructions: hi\n{ADMIT_GLOBAL_HOOKS}"),
        )
        .unwrap();
        run_async(async {
            let marker = "gt-config-agent-m2d";
            let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            ctx.session = Some(Session::new_from_ctx(&ctx, &app, "config-chat").unwrap());
            assert!(graph::active_agent_graph_name(&ctx).is_none());
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![("hello", vec![])], Arc::clone(&seen));

            turn(&mut ctx, "hi", None, &mut runner).await.unwrap();

            assert_eq!(route_calls(&seen.lock()), 0);
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            for event in ["turn.started", "turn.completed"] {
                assert_eq!(
                    graph_envs(&capture_for(marker, event)),
                    (None, None),
                    "{event}"
                );
            }
            assert_eq!(ctx.last_graph_branch, None);
        });
    }

    #[test]
    #[serial]
    fn graph_turn_failed_event_carries_the_classified_chat_branch() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_chat_failed");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-chat-failed-p6n";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![], Arc::clone(&seen));

            let err = turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap_err();

            assert_eq!(err.to_string(), "no scripted chat reply left");
            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            assert_eq!(chat_inputs(&seen).len(), 1);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            let failed = capture_for(marker, "turn.failed");
            assert_eq!(graph_envs(&failed), (Some("chat"), Some("classified")));
            assert_eq!(
                failed.envs.get("COYOTE_ERROR").map(String::as_str),
                Some("no scripted chat reply left")
            );
        });
    }

    /// The classifier's one-shot failing before anything is sent: the turn
    /// errors with no branch recorded, the user's own input is still the
    /// last message, and the session is untouched.
    #[test]
    #[serial]
    fn graph_classifier_failure_leaves_the_users_input_as_the_last_message() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_classifier_down");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-classifier-down-k4w";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = {
                let seen = Arc::clone(&seen);
                // Runs the before-hook the real runner runs, so the route
                // input does displace `last_message` until it is restored.
                completion_runner(move |input, ctx, _abort| {
                    seen.lock().push(input.clone());
                    ctx.before_chat_completion(input).unwrap();
                    boxed_completion(async { Err(anyhow!("classifier down")) })
                })
            };

            let err = turn(&mut ctx, "is it done?", None, &mut runner)
                .await
                .unwrap_err();

            let chain = format!("{err:#}");
            assert!(
                chain.contains("Graph route classifier call failed"),
                "{chain}"
            );
            assert!(chain.contains("classifier down"), "{chain}");
            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            assert!(chat_inputs(&seen).is_empty());
            let last = ctx.last_message.as_ref().unwrap();
            assert_ne!(last.input.role().name(), ROUTE_ROLE_NAME);
            assert_eq!(last.input.bare_text(), "is it done?");
            assert!(last.continuous);
            assert_eq!(ctx.last_graph_branch, None);
            let session = ctx.session.as_ref().unwrap();
            assert!(session.messages().is_empty());
            assert!(session.last_run().is_none());
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            let failed = capture_for(marker, "turn.failed");
            assert_eq!(graph_envs(&failed), (None, None));
            assert_eq!(
                failed.envs.get("COYOTE_ERROR").map(String::as_str),
                Some(chain.as_str())
            );
        });
    }

    /// Today's production shape: a graph agent carries a session block but
    /// the REPL engages no session for it, so the turn is the plain graph
    /// run. The classifier is never consulted, no branch is recorded, and
    /// the turn events carry neither graph variable.
    #[test]
    #[serial]
    fn usage_probe_a_graph_agent_without_a_session_runs_plainly_and_records_no_branch() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_no_session");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-no-session-q8c";
            let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            assert!(ctx.session.is_none());
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            // A classifier that would say "chat" and a chat reply standing
            // by: neither may be consulted without a session.
            let mut runner = scripted_runner("chat", vec![("about it", vec![])], Arc::clone(&seen));

            turn(&mut ctx, "do the thing", None, &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert!(seen.is_empty(), "no model call at all: {seen:?}");
            assert_eq!(ctx.last_graph_branch, None);
            let last = ctx
                .last_message
                .as_ref()
                .expect("the run is the last message");
            assert_eq!(last.input.bare_text(), "do the thing");
            assert_eq!(last.output, "do the thing");
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            for event in ["turn.started", "turn.completed"] {
                assert_eq!(
                    graph_envs(&capture_for(marker, event)),
                    (None, None),
                    "{event}"
                );
            }
        });
    }

    /// A branch left behind by an earlier graph turn is forgotten at the
    /// start of the next turn, so a plain turn's events never report it.
    #[test]
    #[serial]
    fn usage_probe_a_plain_turn_clears_a_stale_branch_before_its_events_fire() {
        let _sink = test_sink::install();
        run_async(async {
            let marker = "gt-stale-branch-w3f";
            let mut ctx = ctx_with_hooks(&TURN_EVENTS, marker);
            ctx.session = Some(Session::default());
            ctx.set_last_graph_branch(Branch::Chat, true);
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![("hello", vec![])], Arc::clone(&seen));

            turn(&mut ctx, "hi", None, &mut runner).await.unwrap();

            assert_eq!(route_calls(&seen.lock()), 0);
            assert_eq!(ctx.last_graph_branch, None);
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.completed")),
                (None, None)
            );
        });
    }

    /// Two runs in a row keep one record (the latest), and undoing walks the
    /// history back the way the record was written: undoing the chat that
    /// followed a run keeps the record, undoing the run itself drops it.
    #[test]
    #[serial]
    fn usage_probe_consecutive_runs_replace_the_record_and_undo_drops_it_with_its_exchange() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_runs_undo");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let mut ctx = engage_graph_agent(repl_ctx(), &name).await;

            run_turn(&mut ctx, "first").await;
            // The first exchange stored also lays down the role's leading
            // rows, so lengths are measured from here on.
            let base = ctx.session.as_ref().unwrap().messages().len() - 2;
            run_turn(&mut ctx, "second").await;
            {
                let session = ctx.session.as_ref().unwrap();
                assert_eq!(session.messages().len(), base + 4);
                let last_run = session.last_run().expect("the latest run is recorded");
                assert_eq!(last_run.prompt, "second");
                assert_eq!(last_run.output, "second");
                assert_eq!(last_run.recorded_at_message_count, base + 4);
            }

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("about it", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();
            assert_eq!(ctx.session.as_ref().unwrap().messages().len(), base + 6);

            ctx.undo_last_exchange().unwrap();
            assert_eq!(ctx.pending_prefill.as_deref(), Some("what happened?"));
            {
                let session = ctx.session.as_ref().unwrap();
                assert_eq!(session.messages().len(), base + 4);
                assert_eq!(
                    session.last_run().map(|run| run.prompt.as_str()),
                    Some("second"),
                    "undoing the chat keeps the run's record"
                );
            }

            ctx.undo_last_exchange().unwrap();
            assert_eq!(ctx.pending_prefill.as_deref(), Some("second"));
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), base + 2);
            assert!(
                session.last_run().is_none(),
                "undoing the run drops its record"
            );
            let [.., user, assistant] = session.messages() else {
                panic!("the first run's exchange stays");
            };
            assert_eq!(user.content.to_text(), "first");
            assert_eq!(assistant.content.to_text(), "first");
        });
    }

    /// A config agent whose model cannot be resolved: the error reaches the
    /// caller bare, exactly as `create_client` phrases it, and the session
    /// saved on disk before the turn is byte-identical afterwards.
    #[test]
    #[serial]
    fn usage_probe_preflight_failure_with_a_config_agent_leaves_the_saved_session_bytes_alone() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_preflight_agent");
        let agent_dir = paths::agent_data_dir(&name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.yaml"),
            format!("name: {name}\ninstructions: hi\n"),
        )
        .unwrap();
        run_async(async {
            let mut ctx = repl_ctx();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            let mut session = Session::new_from_ctx(&ctx, &app, "cfg-chat").unwrap();
            session.set_save_session(Some(true));
            let path = ctx.sessions_dir_for(session.scope()).join("cfg-chat.yaml");
            session.save("cfg-chat", &path, true).unwrap();
            let saved = std::fs::read(&path).unwrap();
            assert!(!saved.is_empty());
            ctx.session = Some(session);
            let input = Input::from_str(&ctx, "hi", None).unwrap();
            let expected = input
                .create_client()
                .err()
                .expect("the test AppState has no client for the agent's model")
                .to_string();
            assert!(expected.contains("Invalid model"), "{expected}");

            let err = ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut repl_completion_runner(),
            )
            .await
            .unwrap_err();

            assert_eq!(err.to_string(), expected);
            assert!(err.downcast_ref::<PreflightError>().is_none());
            assert!(ctx.last_message.is_none());
            let session = ctx.session.as_ref().unwrap();
            assert!(!session.dirty());
            assert_eq!(std::fs::read(&path).unwrap(), saved);
            // Leaving the session rewrites nothing either.
            ctx.exit_session().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), saved);
        });
    }

    /// The real runner against a graph agent whose model cannot be
    /// resolved: the classifier's preflight fails the turn before any
    /// request, the failure names the classifier and the model, no branch is
    /// recorded, the session is untouched, and the user's own input (not the
    /// classifier's) stays the last message, as the before-hook on the
    /// original input runs ahead of the routing.
    #[test]
    #[serial]
    fn usage_probe_classifier_preflight_failure_records_no_branch_and_keeps_the_session() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_classifier_preflight");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-classifier-preflight-r2j";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            // The fixture's `set_model` already dirtied the session.
            let dirty_before = ctx.session.as_ref().unwrap().dirty();
            test_sink::drain();
            let input = Input::from_str(&ctx, "is it done?", None).unwrap();

            let err = ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut repl_completion_runner(),
            )
            .await
            .unwrap_err();

            let chain = format!("{err:#}");
            assert!(
                chain.contains("Graph route classifier call failed"),
                "{chain}"
            );
            assert!(chain.contains("Invalid model"), "{chain}");
            assert_eq!(ctx.last_graph_branch, None);
            let last = ctx.last_message.as_ref().expect("the user's input");
            assert_ne!(last.input.role().name(), ROUTE_ROLE_NAME);
            assert_eq!(last.input.bare_text(), "is it done?");
            assert!(last.continuous);
            let session = ctx.session.as_ref().unwrap();
            assert!(session.messages().is_empty());
            assert!(session.last_run().is_none());
            assert_eq!(session.dirty(), dirty_before);
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.failed")),
                (None, None)
            );
        });
    }

    /// A graph.yaml that turns unreadable after the agent was engaged fails
    /// the turn with the load error: no panic, no branch, nothing recorded.
    #[test]
    #[serial]
    fn usage_probe_a_graph_made_malformed_after_engaging_fails_the_turn_cleanly() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_malformed");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-malformed-n7v";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            std::fs::write(
                paths::agent_graph_file(&name),
                "name: [unterminated\nsession: {chat: {instructions: hi}\n",
            )
            .unwrap();
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![], Arc::clone(&seen));

            let err = turn(&mut ctx, "do the thing", None, &mut runner)
                .await
                .unwrap_err();

            let chain = format!("{err:#}");
            assert!(
                chain.contains(&format!("Failed to load graph.yaml for agent '{name}'")),
                "{chain}"
            );
            assert!(seen.lock().is_empty(), "nothing is classified or sent");
            assert_eq!(ctx.last_graph_branch, None);
            let session = ctx.session.as_ref().unwrap();
            assert!(session.messages().is_empty());
            assert!(session.last_run().is_none());
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            let failed = capture_for(marker, "turn.failed");
            assert_eq!(graph_envs(&failed), (None, None));
            assert_eq!(
                failed.envs.get("COYOTE_ERROR").map(String::as_str),
                Some(chain.as_str())
            );
        });
    }

    /// A forced chat never consults the classifier (a classifier that
    /// would have said "run" is ignored because it is never asked), carries
    /// the last run in its request, records the exchange, leaves the run's
    /// record alone, and tells hooks the branch was forced.
    #[test]
    #[serial]
    fn usage_probe_a_forced_chat_skips_the_classifier_and_marks_the_turn_forced() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_forced_chat");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-forced-chat-q8m";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            run_turn(&mut ctx, "do the thing").await;
            let before = ctx.session.as_ref().unwrap().messages().len();
            test_sink::drain();

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("run", vec![("about it", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", Some(Branch::Chat), &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 0, "a forced branch is never classified");
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            let content = chats[0].message_content().to_text();
            assert!(
                content.starts_with("Last graph run (completed "),
                "{content}"
            );
            assert!(content.ends_with("\n\nwhat happened?"), "{content}");
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, true)));
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), before + 2);
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "about it"
            );
            assert_eq!(
                session.last_run().map(|run| run.prompt.as_str()),
                Some("do the thing"),
                "a forced chat leaves the run's record alone"
            );
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.started")),
                (None, None)
            );
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.completed")),
                (Some("chat"), Some("forced"))
            );
        });
    }

    /// Ctrl-C during a classified chat completion ends the turn as
    /// interrupted, and that event carries the branch the way completed
    /// and failed do.
    #[test]
    #[serial]
    fn usage_probe_an_interrupted_chat_turn_reports_its_branch_on_turn_interrupted() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_interrupted_chat");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-interrupted-chat-w2p";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let recorder = Arc::clone(&seen);
            let mut runner = completion_runner(move |input, _ctx, abort| {
                recorder.lock().push(input.clone());
                let reply = if input.role().name() == ROUTE_ROLE_NAME {
                    Ok((r#"{"decision":"chat"}"#.to_string(), vec![]))
                } else {
                    // The user hits Ctrl-C while the answer is in flight.
                    abort.set_ctrlc();
                    Err(anyhow!("Aborted!"))
                };
                boxed_completion(async move { reply })
            });

            let result = turn(&mut ctx, "what happened?", None, &mut runner).await;

            assert!(result.is_err(), "the interrupted turn surfaces its abort");
            assert_eq!(route_calls(&seen.lock()), 1);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));
            assert_eq!(turn_counts(marker), (1, 0, 1, 0));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.interrupted")),
                (Some("chat"), Some("classified"))
            );
            // The interruption is recorded the way any REPL interruption is
            // (the user's text plus the placeholder answer), and even that
            // record carries none of the request's run artifact.
            let session = ctx.session.as_ref().unwrap();
            let stored = session.messages();
            assert!(
                stored
                    .iter()
                    .any(|m| m.role.is_user() && m.content.to_text() == "what happened?"),
                "{stored:?}"
            );
            assert!(
                !stored
                    .iter()
                    .any(|m| m.content.to_text().contains("Last graph run")),
                "{stored:?}"
            );
            assert!(session.last_run().is_none(), "the graph never ran");
        });
    }

    /// A saved run survives leaving the session: reloading the file in a
    /// fresh context brings the record back in step with the stored
    /// exchange, and the next chat turn inlines that run from disk under the
    /// block's instructions, storing only the typed text.
    #[test]
    #[serial]
    fn usage_probe_a_saved_run_is_recalled_by_a_chat_after_reloading_the_session() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_reload");
        write_graph_agent(&name, SESSION_BLOCK);
        run_async(async {
            let app = repl_ctx().app.config.clone();
            let mut ctx = repl_ctx();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            let mut session = Session::new_from_ctx(&ctx, &app, "graph-chat").unwrap();
            // A client name the test model registry knows, so the saved id
            // resolves again on load.
            let mut data = ModelData::new("fc-model");
            data.supports_function_calling = true;
            session.set_model(Model::from_config("test-seeded", &[data]).remove(0));
            session.set_save_session(Some(true));
            let scope = session.scope();
            ctx.session = Some(session);

            run_turn(&mut ctx, "before the restart").await;
            let stored = ctx.session.as_ref().unwrap().messages().len();
            ctx.exit_session().unwrap();
            assert!(ctx.session.is_none());
            assert_eq!(ctx.last_graph_branch, None);
            let path = ctx.sessions_dir_for(scope).join("graph-chat.yaml");
            assert!(
                path.is_file(),
                "leaving saved the session at {}",
                path.display()
            );

            // A fresh context, as after restarting the binary.
            let mut ctx = repl_ctx();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            let loaded = Session::load_from_ctx(&ctx, &app, "graph-chat", &path, scope).unwrap();
            assert_eq!(loaded.messages().len(), stored);
            let run = loaded.last_run().expect("the run record was saved");
            assert_eq!(run.prompt, "before the restart");
            assert_eq!(run.output, "before the restart");
            assert_eq!(run.recorded_at_message_count, stored);
            ctx.session = Some(loaded);

            let seen: SeenInputs = Default::default();
            let mut runner =
                scripted_runner("chat", vec![("it echoed", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what did it say?", None, &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            let content = chats[0].message_content().to_text();
            assert!(
                content.starts_with("Last graph run (completed "),
                "{content}"
            );
            assert!(content.contains("before the restart"), "{content}");
            assert!(content.ends_with("\n\nwhat did it say?"), "{content}");
            let messages = chats[0].build_messages().unwrap();
            assert!(matches!(messages[0].role, MessageRole::System));
            assert_eq!(messages[0].content.to_text(), "hi");
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), stored + 2);
            let [.., user, assistant] = session.messages() else {
                panic!("expected the chat exchange");
            };
            assert_eq!(user.content.to_text(), "what did it say?");
            assert_eq!(assistant.content.to_text(), "it echoed");
            assert_eq!(
                session.last_run().map(|run| run.prompt.as_str()),
                Some("before the restart")
            );
        });
    }

    /// Leaving the session after a chat drops the branch with it: the next
    /// turn on the same graph agent runs plainly (no session, no
    /// classifier) and its events carry no graph vars.
    #[test]
    #[serial]
    fn usage_probe_leaving_the_session_after_a_chat_leaves_the_next_turn_unrouted() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let name = unique("repl_leave_after_chat");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let marker = "gt-leave-chat-d5r";
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &name).await;
            ctx.session.as_mut().unwrap().set_save_session(Some(false));
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("about it", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));

            ctx.exit_session().unwrap();
            assert_eq!(ctx.last_graph_branch, None);
            test_sink::drain();

            let seen: SeenInputs = Default::default();
            let mut runner =
                scripted_runner("chat", vec![("never sent", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "do the thing", None, &mut runner)
                .await
                .unwrap();

            assert!(seen.lock().is_empty(), "no classifier, no chat call");
            assert_eq!(ctx.last_graph_branch, None);
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            assert_eq!(
                graph_envs(&capture_for(marker, "turn.completed")),
                (None, None)
            );
            let last = ctx.last_message.as_ref().expect("the run's output");
            assert_eq!(last.output, "do the thing");
        });
    }

    /// The system message is fixed at the session's first exchange, so a
    /// session whose first turn ran the graph and one whose first turn was
    /// answered by the chat agent must record the same one: the chat
    /// agent's prompt, with the memory section sized for its tools either
    /// way. `prefix` and `session_model` put the graph agent on a model of
    /// the caller's choosing while the chat agent inherits a tool-calling
    /// one.
    fn assert_births_record_the_same_system_message(prefix: &str, session_model: Model) {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        write_global_memory_store();
        let name = unique("repl_birth");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, &format!("{CHAT_AGENT_BODY}memory: true\n"));
        write_delegating_graph_agent(&name, prefix, &chat_agent);
        run_async(async {
            let mut run_first =
                engage_delegating_graph_agent(repl_ctx(), &name, session_model.clone()).await;
            run_turn(&mut run_first, "do the thing").await;

            let mut chat_first =
                engage_delegating_graph_agent(repl_ctx(), &name, session_model).await;
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut chat_first, "hello", Some(Branch::Chat), &mut runner)
                .await
                .unwrap();

            let first_message = |ctx: &RequestContext| {
                let messages = ctx.session.as_ref().unwrap().messages().to_vec();
                assert!(matches!(messages[0].role, MessageRole::System));
                serde_json::to_value(&messages[0]).unwrap()
            };
            let born_by_run = first_message(&run_first);
            assert_eq!(born_by_run, first_message(&chat_first));
            let prompt = born_by_run["content"].as_str().unwrap();
            assert!(prompt.starts_with("Be brief."), "{prompt}");
            assert!(prompt.contains("<global_index>"), "{prompt}");
            assert!(prompt.contains("## Memory\n"), "{prompt}");
            assert!(!prompt.contains("## Memory (read-only)"), "{prompt}");
            assert_graph_agent_restored(&run_first, &name, &chat_agent);
            assert_graph_agent_restored(&chat_first, &name, &chat_agent);
        });
    }

    #[test]
    #[serial]
    fn delegated_births_record_the_same_system_message_with_tool_calling_models() {
        assert_births_record_the_same_system_message("", fc_model(SESSION_MODEL));
    }

    /// The graph agent and its session cannot call tools; the chat agent
    /// can, and it is the chat agent's model that sizes the memory section.
    #[test]
    #[serial]
    fn delegated_births_record_the_same_system_message_when_only_the_chat_agent_calls_tools() {
        assert_births_record_the_same_system_message(
            "model: test-seeded:plain\n",
            plain_model("plain-session"),
        );
    }

    /// The delegated chat request is the chat agent's ordinary REPL request:
    /// it declares the chat agent's whole pool and nothing of the graph
    /// agent's, and the before-hook the real runner runs records that pool
    /// as the turn's declared functions.
    #[test]
    #[serial]
    fn delegated_chat_request_declares_the_chat_agents_pool() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        write_global_memory_store();
        let name = unique("repl_pool");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, &format!("{CHAT_AGENT_BODY}memory: true\n"));
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let between_turns = Input::from_str(&ctx, "x", None).unwrap();
            let graph_names = between_turns.declared_function_names();
            assert!(graph_names.contains("a_probe"), "{graph_names:?}");
            assert!(!graph_names.contains("b_probe"), "{graph_names:?}");
            assert!(!graph_names.contains("local_hello"), "{graph_names:?}");

            let seen: SeenInputs = Default::default();
            let mut runner = completion_runner(|input, ctx, _abort| {
                seen.lock().push(input.clone());
                boxed_completion(async move {
                    ctx.before_chat_completion(input)?;
                    let reply = if input.role().name() == ROUTE_ROLE_NAME {
                        r#"{"decision":"chat"}"#
                    } else {
                        "hi"
                    };
                    Ok((reply.to_string(), vec![]))
                })
            });
            turn(&mut ctx, "hello", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            let request = chats[0];
            assert!(request.with_session());
            assert!(request.with_agent());
            let names = request.declared_function_names();
            for expected in ["local_hello", "b_probe"] {
                assert!(names.contains(expected), "{expected} in {names:?}");
            }
            for prefix in [
                AGENT_FUNCTION_PREFIX,
                USER_FUNCTION_PREFIX,
                MEMORY_FUNCTION_PREFIX,
            ] {
                assert!(
                    names.iter().any(|n| n.starts_with(prefix)),
                    "{prefix}* in {names:?}"
                );
            }
            assert!(!names.contains("a_probe"), "{names:?}");
            let messages = request.build_messages().unwrap();
            assert!(messages[0].content.to_text().starts_with("Be brief."));
            let declared = &ctx.declared_function_names;
            for expected in ["local_hello", "b_probe"] {
                assert!(declared.contains(expected), "{expected} in {declared:?}");
            }
            assert!(!declared.contains("a_probe"), "{declared:?}");
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// The last run is inlined into the delegated chat request as a
    /// preamble to the typed text, for that request only: the session's
    /// messages keep the typed text alone and the run record stays put.
    #[test]
    #[serial]
    fn delegated_chat_turn_inlines_the_last_run_for_the_request_only() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_last_run");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            run_turn(&mut ctx, "do the thing").await;

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "typed", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            let content = chat_inputs(&seen)[0].message_content().to_text();
            assert!(
                content.starts_with("Last graph run (completed "),
                "{content}"
            );
            assert!(content.contains("do the thing"), "{content}");
            assert!(content.ends_with("\n\ntyped"), "{content}");
            let session = ctx.session.as_ref().unwrap();
            let persisted: Vec<String> = session
                .messages()
                .iter()
                .map(|m| m.content.to_text())
                .collect();
            assert!(
                !persisted.iter().any(|text| text.contains("Last graph run")),
                "{persisted:?}"
            );
            assert_eq!(session.last_run().unwrap().prompt, "do the thing");
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// A tool call evaluated inside the delegated turn runs the chat
    /// agent's binary from the chat agent's bin dir with the chat agent's
    /// variables in its environment; the classifier ran on the session's
    /// model while the chat request went to the chat agent's.
    #[test]
    #[serial]
    fn delegated_tool_call_runs_as_the_chat_agent() {
        if !bash_and_jq_available() {
            return;
        }
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_tool_call");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let seen: SeenInputs = Default::default();
            let probe: Arc<parking_lot::Mutex<Option<String>>> = Default::default();
            let mut runner = completion_runner(|input, ctx, _abort| {
                seen.lock().push(input.clone());
                let probe = Arc::clone(&probe);
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    *probe.lock() = Some(eval_local_hello(ctx).await?);
                    Ok(("done".to_string(), vec![]))
                })
            });

            turn(&mut ctx, "probe it", None, &mut runner).await.unwrap();

            let probe = probe.lock().clone().expect("the tool ran");
            assert_eq!(probe, format!("{chat_agent}|{SHIM_ON_PATH}|terse"));
            let seen = seen.lock().clone();
            let route = seen
                .iter()
                .find(|input| input.role().name() == ROUTE_ROLE_NAME)
                .unwrap();
            assert_eq!(route.role().model().id(), format!("openai:{SESSION_MODEL}"));
            assert_eq!(
                chat_inputs(&seen)[0].role().model().id(),
                format!("openai:{CHAT_AGENT_MODEL}")
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// A classified run turn never sees the chat agent's model: the
    /// classifier runs on the session's, and an llm node, building its
    /// input from the same context with the graph agent in the slot, would
    /// too. The graph's nodes complete through their own transport rather
    /// than the REPL's runner, so the runner pins the classifier's call
    /// alone; the delegated chat turn that follows is the only call that
    /// goes to the chat agent's model.
    #[test]
    #[serial]
    fn run_turn_classifies_on_the_session_model_and_only_the_delegated_chat_uses_the_chat_agents() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_run_model");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let session_model = format!("openai:{SESSION_MODEL}");
            let chat_agent_model = format!("openai:{CHAT_AGENT_MODEL}");

            let seen = run_turn(&mut ctx, "do the thing").await;

            assert_eq!(route_calls(&seen), 1);
            assert_eq!(
                seen.len(),
                1,
                "an echo graph makes no model call of its own"
            );
            assert_eq!(seen[0].role().model().id(), session_model);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
            let node_input = Input::from_str(&ctx, "node prompt", ctx.role.clone()).unwrap();
            assert_eq!(node_input.role().model().id(), session_model);
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let route = seen
                .iter()
                .find(|input| input.role().name() == ROUTE_ROLE_NAME)
                .unwrap();
            assert_eq!(route.role().model().id(), session_model);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            assert_eq!(chats[0].role().model().id(), chat_agent_model);
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// An auto-continue re-entry inside the delegated turn is rebuilt with
    /// the chat agent still in the slot, so it declares the chat agent's
    /// pool too.
    #[test]
    #[serial]
    fn delegated_auto_continue_reentry_declares_the_chat_agents_pool() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_reentry");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let seen: SeenInputs = Default::default();
            let mut runner = completion_runner(|input, ctx, _abort| {
                seen.lock().push(input.clone());
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                let chat_calls = chat_inputs(&seen.lock()).len();
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    if chat_calls == 1 {
                        ctx.init_todo_list("finish the probe");
                        ctx.add_todo("one more step");
                        return Ok(("working".to_string(), vec![]));
                    }
                    ctx.mark_todo_done(1);
                    Ok(("done".to_string(), vec![]))
                })
            });

            turn(&mut ctx, "start", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 2);
            let reentry = chats[1];
            assert!(
                reentry.text().starts_with(DEFAULT_CONTINUATION_PROMPT),
                "{}",
                reentry.text()
            );
            let names = reentry.declared_function_names();
            assert!(names.contains("local_hello"), "{names:?}");
            assert!(names.contains("b_probe"), "{names:?}");
            assert!(!names.contains("a_probe"), "{names:?}");
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// Registers a child agent that never finishes on the context's
    /// supervisor, giving the turn-end guardrail a pending task to remind
    /// about. Spawned on the runtime the turn runs on, so it is dropped
    /// with it.
    fn register_running_child(ctx: &RequestContext, id: &str) {
        use crate::supervisor::mailbox::Inbox;
        use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};

        let id_owned = id.to_string();
        let join_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(AgentResult {
                id: id_owned,
                agent_name: "child".to_string(),
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        ctx.supervisor
            .as_ref()
            .expect("the chat agent can spawn, so a supervisor was built")
            .write()
            .register(AgentHandle {
                id: id.to_string(),
                agent_name: "child".to_string(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
            })
            .unwrap();
    }

    /// The pending-task guardrail's reminder inside the delegated turn is
    /// rebuilt with the chat agent still in the slot, so it declares the
    /// chat agent's pool too.
    #[test]
    #[serial]
    fn delegated_guardrail_reentry_declares_the_chat_agents_pool() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_guardrail_reentry");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let seen: SeenInputs = Default::default();
            let mut runner = completion_runner(|input, ctx, _abort| {
                seen.lock().push(input.clone());
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                let chat_calls = chat_inputs(&seen.lock()).len();
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    if chat_calls == 1 {
                        register_running_child(ctx, "child_1");
                        return Ok(("spawned".to_string(), vec![]));
                    }
                    ctx.supervisor
                        .as_ref()
                        .unwrap()
                        .write()
                        .take("child_1")
                        .expect("the child is still registered");
                    Ok(("collected".to_string(), vec![]))
                })
            });

            turn(&mut ctx, "start", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 2);
            let reentry = chats[1];
            assert!(
                reentry.text().starts_with("[SYSTEM GUARDRAIL]"),
                "{}",
                reentry.text()
            );
            assert!(reentry.text().contains("child_1"), "{}", reentry.text());
            let names = reentry.declared_function_names();
            assert!(names.contains("local_hello"), "{names:?}");
            assert!(names.contains("b_probe"), "{names:?}");
            assert!(!names.contains("a_probe"), "{names:?}");
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// Every way out of a delegated turn puts the graph agent back: a
    /// completed turn, a preflight failure, a failed completion, and an
    /// error raised while the loop rebuilds its next input.
    #[test]
    #[serial]
    fn delegated_turn_restores_the_graph_agent_on_every_exit() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_exits");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "hello", None, &mut runner).await.unwrap();
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            let mut runner = completion_runner(|input, _ctx, _abort| {
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    Err(anyhow::Error::new(PreflightError(anyhow!("no client"))))
                })
            });
            let err = turn(&mut ctx, "hello", None, &mut runner)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), "no client");
            assert!(err.downcast_ref::<PreflightError>().is_none());
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![], Arc::clone(&seen));
            let err = turn(&mut ctx, "hello", None, &mut runner)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("no scripted chat reply left"),
                "{err:#}"
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // The reply leaves a todo open, so the loop rebuilds an
            // auto-continue input; the skill the reply enabled is not
            // installed, so that rebuild fails inside the loop.
            let mut runner = completion_runner(|input, ctx, _abort| {
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    with_app_config(ctx, |config| {
                        config.enabled_skills = Some(vec!["not-installed".to_string()]);
                    });
                    ctx.init_todo_list("keep going");
                    ctx.add_todo("one more step");
                    Ok(("working".to_string(), vec![]))
                })
            });
            let err = turn(&mut ctx, "hello", None, &mut runner)
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("enabled_skills references skill 'not-installed'"),
                "{err:#}"
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// Regenerating a delegated answer replays through the router: the
    /// classifier runs again, the request is rebuilt as the chat agent's,
    /// and the new answer replaces the old one in place.
    #[test]
    #[serial]
    fn delegated_regenerate_reclassifies_and_replaces_the_answer() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_regen_delegate");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let app = Arc::clone(&ctx.app.config);
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("first", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap();
            let before = ctx.session.as_ref().unwrap().messages().len();

            let mut replay = ctx.last_message.as_ref().unwrap().input.clone();
            replay.set_regenerate(ctx.extract_role(&app).unwrap());
            let mut runner = scripted_runner("chat", vec![("second", vec![])], Arc::clone(&seen));
            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                replay,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 2);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 2);
            assert!(!chats[0].regenerate());
            assert!(chats[1].regenerate());
            assert!(chats[1].declared_function_names().contains("local_hello"));
            assert!(!chats[1].declared_function_names().contains("a_probe"));
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), before);
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "second"
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// The turn events stay the graph agent's and name the chat agent in
    /// `COYOTE_GRAPH_CHAT_AGENT`; a tool fired inside the delegated turn
    /// runs under the chat agent's hooks as the chat agent.
    #[test]
    #[serial]
    fn delegated_turn_hooks_name_both_agents() {
        if !bash_and_jq_available() {
            return;
        }
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-q4m";
        let name = unique("repl_delegate_hooks");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(
            &chat_agent,
            &format!(
                "{CHAT_AGENT_BODY}hooks:\n  tool.started:\n    - name: {marker}-tool.started\n      command: \"true\"\n"
            ),
        );
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            test_sink::drain();
            let mut runner = completion_runner(|input, ctx, _abort| {
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    eval_local_hello(ctx).await?;
                    Ok(("done".to_string(), vec![]))
                })
            });

            turn(&mut ctx, "probe it", None, &mut runner).await.unwrap();

            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            let started = capture_for(marker, "turn.started");
            assert_eq!(started.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert!(!started.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
            let completed = capture_for(marker, "turn.completed");
            assert_eq!(completed.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                completed.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent)
            );
            assert_eq!(graph_envs(&completed), (Some("chat"), Some("classified")));
            let tool_started = capture_for(marker, "tool.started");
            assert_eq!(
                tool_started.envs.get("COYOTE_AGENT_NAME"),
                Some(&chat_agent)
            );
            assert_eq!(
                tool_started
                    .envs
                    .get("COYOTE_TOOL_NAME")
                    .map(String::as_str),
                Some("local_hello")
            );

            // A run turn names no chat agent.
            test_sink::drain();
            run_turn(&mut ctx, "do the thing").await;
            let completed = capture_for(marker, "turn.completed");
            assert!(!completed.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
            assert_eq!(graph_envs(&completed), (Some("run"), Some("classified")));
        });
    }

    /// `chat.agent` added to the graph after the agent was engaged finds no
    /// chat agent to swap in: the turn is refused with a hint rendered in
    /// the configured command prefix, and the graph agent stays put.
    #[test]
    #[serial]
    fn delegated_chat_bails_when_the_block_gained_its_chat_agent_after_engage() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        for (prefix, exit_hint, agent_hint) in [
            (CommandPrefix::Dot, "'.exit agent'", ".agent"),
            (CommandPrefix::Slash, "'/exit agent'", "/agent"),
        ] {
            let name = unique("repl_late_delegate");
            write_graph_agent(&name, SESSION_BLOCK);
            run_async(async {
                let mut ctx = repl_ctx();
                with_app_config(&mut ctx, |config| config.command_prefix = prefix);
                let mut ctx = engage_graph_agent(ctx, &name).await;
                assert!(ctx.delegate.is_none());
                write_delegating_graph_agent(&name, "", &chat_agent);

                let seen: SeenInputs = Default::default();
                let mut runner = scripted_runner("chat", vec![], Arc::clone(&seen));
                let err = turn(&mut ctx, "hello", Some(Branch::Chat), &mut runner)
                    .await
                    .unwrap_err();

                let text = format!("{err:#}");
                assert!(
                    text.contains("session.chat.agent changed since the agent was engaged"),
                    "{text}"
                );
                assert!(text.contains(exit_hint), "{text}");
                assert!(
                    text.contains(&format!("'{agent_hint} {name} graph-chat'")),
                    "{text}"
                );
                assert!(seen.lock().is_empty());
                assert_eq!(ctx.agent.as_ref().unwrap().name(), name);
                assert!(ctx.delegate.is_none());
                assert!(ctx.session.as_ref().unwrap().messages().is_empty());
            });
        }
    }

    /// `chat.agent` renamed in the graph after the agent was engaged names
    /// an agent other than the one parked beside the graph agent: the turn
    /// is refused the same way, nothing is swapped in, and the parked chat
    /// agent is left exactly as engaged.
    #[test]
    #[serial]
    fn delegated_chat_bails_when_the_block_renamed_its_chat_agent_after_engage() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_renamed_delegate");
        let chat_agent = unique("repl_chat_agent");
        let renamed = unique("repl_renamed_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_chat_agent(&renamed, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            write_delegating_graph_agent(&name, "", &renamed);

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![], Arc::clone(&seen));
            let err = turn(&mut ctx, "hello", Some(Branch::Chat), &mut runner)
                .await
                .unwrap_err();

            let text = format!("{err:#}");
            assert!(
                text.contains("session.chat.agent changed since the agent was engaged"),
                "{text}"
            );
            assert!(text.contains("'.exit agent'"), "{text}");
            assert!(text.contains(&format!("'.agent {name} s'")), "{text}");
            assert!(seen.lock().is_empty());
            assert!(ctx.session.as_ref().unwrap().messages().is_empty());
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
            assert_eq!(ctx.delegate.as_ref().unwrap().name, chat_agent);
        });
    }

    /// The renamed-key refusal reads the block as it is on disk at each
    /// turn and nothing else: its hint follows the configured command
    /// prefix, a `.regenerate` that meets it leaves the exchange it would
    /// have replaced untouched, and once the block names the parked chat
    /// agent again the very next turn is delegated with no re-engage.
    #[test]
    #[serial]
    fn usage_probe_renamed_chat_agent_refusal_follows_the_prefix_and_lifts_when_the_block_is_restored()
     {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_renamed_slash");
        let chat_agent = unique("repl_chat_agent");
        let renamed = unique("repl_renamed_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_chat_agent(&renamed, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx = repl_ctx();
            with_app_config(&mut ctx, |config| {
                config.command_prefix = CommandPrefix::Slash
            });
            let mut ctx = engage_delegating_graph_agent(ctx, &name, fc_model(SESSION_MODEL)).await;
            let app = Arc::clone(&ctx.app.config);

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("first", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap();
            let snapshot = |ctx: &RequestContext| -> Vec<String> {
                let session = ctx.session.as_ref().unwrap();
                session
                    .messages()
                    .iter()
                    .map(|m| format!("{m:?}"))
                    .collect()
            };
            let recorded = snapshot(&ctx);
            let last = ctx.session.as_ref().unwrap().messages().last().unwrap();
            assert_eq!(last.content.to_text(), "first");

            write_delegating_graph_agent(&name, "", &renamed);

            // A regenerate of the delegated answer meets the refusal: the
            // chat agent's model is never called and the old exchange stays.
            let mut replay = ctx.last_message.as_ref().unwrap().input.clone();
            replay.set_regenerate(ctx.extract_role(&app).unwrap());
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("never", vec![])], Arc::clone(&seen));
            let err = ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                replay,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("session.chat.agent changed since the agent was engaged"),
                "{text}"
            );
            assert!(text.contains("'/exit agent'"), "{text}");
            assert!(text.contains(&format!("'/agent {name} s'")), "{text}");
            assert!(!text.contains("'."), "{text}");
            {
                let seen = seen.lock();
                assert_eq!(
                    route_calls(&seen),
                    1,
                    "the classifier still ran on the session model"
                );
                assert!(chat_inputs(&seen).is_empty());
            }
            assert_eq!(snapshot(&ctx), recorded);
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // The block names the parked chat agent again: delegated as before.
            write_delegating_graph_agent(&name, "", &chat_agent);
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("second", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "and now?", None, &mut runner).await.unwrap();
            let seen = seen.lock().clone();
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            assert_eq!(
                chats[0].role().model().id(),
                format!("openai:{CHAT_AGENT_MODEL}")
            );
            assert!(chats[0].declared_function_names().contains("local_hello"));
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), recorded.len() + 2);
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "second"
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// The renamed-key refusal teaches a remedy; following it works. After
    /// one delegated exchange under the engaged chat agent the block names
    /// another agent: the turn is refused, `.exit agent` lets go of the
    /// parked one, and `.agent <A> s` resumes the same non-empty session
    /// under the newly named agent, which answers the very next chat turn
    /// with its own pool, model and instructions on top of the transcript
    /// recorded under its predecessor.
    #[test]
    #[serial]
    fn usage_probe_following_the_refusals_remedy_re_engages_under_the_renamed_chat_agent() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_remedy");
        let chat_agent = unique("repl_chat_agent");
        let renamed = unique("repl_renamed_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_global_tool("c_probe");
        write_chat_agent(
            &renamed,
            "instructions: Speak up.\nglobal_tools:\n  - c_probe.sh\nvariables:\n  - name: tone\n    description: How to speak\n",
        );
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            // The session must come back from disk, so it sits on a model the
            // app can resolve on resume rather than the synthetic fixture one.
            let ctx = repl_ctx();
            let session_model = Model::retrieve_model(
                &ctx.app.config,
                "test-seeded:session-model",
                crate::client::ModelType::Chat,
            )
            .unwrap();
            let mut ctx = engage_delegating_graph_agent(ctx, &name, session_model).await;
            let app = Arc::clone(&ctx.app.config);
            ctx.session.as_mut().unwrap().set_save_session(Some(true));

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("first", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap();
            {
                let seen = seen.lock();
                let names = chat_inputs(&seen)[0].declared_function_names();
                assert!(names.contains("b_probe"), "{names:?}");
                assert!(!names.contains("c_probe"), "{names:?}");
            }
            let recorded = ctx.session.as_ref().unwrap().messages().len();
            assert_eq!(
                ctx.session.as_ref().unwrap().chat_agent(),
                Some(chat_agent.as_str())
            );

            write_delegating_graph_agent(&name, "", &renamed);
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("never", vec![])], Arc::clone(&seen));
            let err = turn(&mut ctx, "again?", None, &mut runner)
                .await
                .unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains("'.exit agent'"), "{text}");
            assert!(text.contains(&format!("'.agent {name} s'")), "{text}");
            assert!(chat_inputs(&seen.lock()).is_empty());

            // The remedy, as taught.
            ctx.exit_agent(&app).unwrap();
            assert!(ctx.agent.is_none());
            assert!(ctx.delegate.is_none());
            assert!(ctx.session.is_none());
            ctx.use_agent(&app, &name, Some("s"), create_abort_signal())
                .await
                .unwrap();
            {
                let session = ctx.session.as_ref().unwrap();
                assert_eq!(session.messages().len(), recorded, "the session resumed");
                assert_eq!(session.chat_agent(), Some(renamed.as_str()));
                assert!(session.chat_agent_instructions().starts_with("Speak up."));
                assert_eq!(session.model().id(), "test-seeded:session-model");
                let delegate = ctx.delegate.as_ref().expect("the renamed agent is parked");
                assert_eq!(delegate.name, renamed);
                assert_eq!(
                    delegate.agent.variables().get("tone").map(String::as_str),
                    Some("terse")
                );
            }

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("second", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "and now?", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            let names = chats[0].declared_function_names();
            assert!(names.contains("c_probe"), "{names:?}");
            assert!(names.contains("local_hello"), "{names:?}");
            assert!(!names.contains("b_probe"), "{names:?}");
            assert!(!names.contains("a_probe"), "{names:?}");
            assert_eq!(
                chats[0].role().model().id(),
                format!("openai:{CHAT_AGENT_MODEL}")
            );
            assert!(chats[0].role().prompt().starts_with("Speak up."));
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.messages().len(), recorded + 2);
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "second"
            );
            assert_graph_agent_restored(&ctx, &name, &renamed);
        });
    }

    /// A refused chat turn still brackets as a chat turn: the renamed-key
    /// refusal fails the turn with the ENGAGED chat agent's name beside the
    /// graph agent's, not the name the block now carries, while the
    /// gained-key refusal, with nothing parked, names no chat agent.
    #[test]
    #[serial]
    fn usage_probe_a_refused_chat_turn_fails_naming_the_engaged_chat_agent_if_any() {
        if !bash_and_jq_available() {
            return;
        }
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-refused-p7q";
        let name = unique("repl_refused_hooks");
        let chat_agent = unique("repl_chat_agent");
        let renamed = unique("repl_renamed_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_chat_agent(&renamed, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        let late = unique("repl_late_hooks");
        write_graph_agent(&late, &format!("{ADMIT_GLOBAL_HOOKS}{SESSION_BLOCK}"));
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &renamed);
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("never", vec![])], Arc::clone(&seen));

            let err = turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap_err();

            assert!(
                format!("{err:#}")
                    .contains("session.chat.agent changed since the agent was engaged")
            );
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            let failed = capture_for(marker, "turn.failed");
            assert_eq!(failed.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                failed.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent),
                "the engaged agent, not the block's new name"
            );
            assert_eq!(graph_envs(&failed), (Some("chat"), Some("classified")));
            assert!(
                failed
                    .envs
                    .get("COYOTE_ERROR")
                    .is_some_and(|e| e.contains("session.chat.agent changed")),
                "{:?}",
                failed.envs.get("COYOTE_ERROR")
            );
            assert!(chat_inputs(&seen.lock()).is_empty());
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // The key gained after engage: a refusal with nothing parked.
            test_sink::drain();
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &late).await;
            write_delegating_graph_agent(&late, ADMIT_GLOBAL_HOOKS, &chat_agent);
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("never", vec![])], Arc::clone(&seen));
            let err = turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}")
                    .contains("session.chat.agent changed since the agent was engaged")
            );
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            let failed = capture_for(marker, "turn.failed");
            assert_eq!(failed.envs.get("COYOTE_AGENT_NAME"), Some(&late));
            assert!(!failed.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
            assert_eq!(graph_envs(&failed), (Some("chat"), Some("classified")));
            assert!(ctx.delegate.is_none());
            assert_eq!(ctx.agent.as_ref().unwrap().name(), late);
        });
    }

    /// The inverse of the late `chat.agent`: the key dropped from the block
    /// after engage. The turn runs inline as any block does, with the graph
    /// agent in the slot the whole time, while the parked chat agent keeps
    /// answering for the chat configuration (its memory preference), so
    /// `turn.completed` still names it in `COYOTE_GRAPH_CHAT_AGENT`.
    #[test]
    #[serial]
    fn usage_probe_delegated_block_dropping_its_chat_agent_runs_inline_with_the_delegate_parked() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-dropped-n8r";
        write_global_memory_store();
        let name = unique("repl_dropped_delegate");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, &format!("{CHAT_AGENT_BODY}memory: true\n"));
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            assert!(
                ctx.memory_config().enabled,
                "the parked chat agent has memory on"
            );
            let graph_names: HashSet<String> = ctx
                .agent
                .as_ref()
                .unwrap()
                .functions()
                .declarations()
                .iter()
                .map(|decl| decl.name.clone())
                .collect();
            write_graph_agent(&name, &format!("{ADMIT_GLOBAL_HOOKS}{SESSION_BLOCK}"));
            test_sink::drain();

            let seen: SeenInputs = Default::default();
            let observed: Arc<parking_lot::Mutex<Option<(String, String, bool)>>> =
                Default::default();
            let mut runner = completion_runner(|input, ctx, _abort| {
                seen.lock().push(input.clone());
                let observed = Arc::clone(&observed);
                let is_route = input.role().name() == ROUTE_ROLE_NAME;
                boxed_completion(async move {
                    if is_route {
                        return Ok((r#"{"decision":"chat"}"#.to_string(), vec![]));
                    }
                    *observed.lock() = Some((
                        ctx.agent.as_ref().unwrap().name().to_string(),
                        ctx.chat_agent().unwrap().name().to_string(),
                        ctx.delegate.as_ref().unwrap().parked.is_none(),
                    ));
                    Ok(("inline".to_string(), vec![]))
                })
            });

            turn(&mut ctx, "hello", None, &mut runner).await.unwrap();

            let (slot, authority, parked_untouched) =
                observed.lock().clone().expect("the chat call ran");
            assert_eq!(slot, name, "the graph agent answers an inline block");
            assert_eq!(
                authority, chat_agent,
                "the parked chat agent is still the authority"
            );
            assert!(parked_untouched);
            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1);
            let chats = chat_inputs(&seen);
            assert_eq!(chats.len(), 1);
            let request = chats[0];
            assert_eq!(
                request.role().model().id(),
                format!("openai:{SESSION_MODEL}"),
                "the request ran as the graph agent"
            );
            let names = request.declared_function_names();
            assert!(!names.contains("local_hello"), "{names:?}");
            assert!(!names.contains("b_probe"), "{names:?}");
            assert!(
                names.is_subset(&graph_names),
                "{names:?} within {graph_names:?}"
            );
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            let completed = capture_for(marker, "turn.completed");
            assert_eq!(completed.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                completed.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent),
                "the engaged chat agent is named although the graph agent answered"
            );
            assert_eq!(graph_envs(&completed), (Some("chat"), Some("classified")));
            assert_eq!(
                ctx.session
                    .as_ref()
                    .unwrap()
                    .messages()
                    .last()
                    .unwrap()
                    .content
                    .to_text(),
                "inline"
            );
            assert!(ctx.memory_config().enabled);
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// A chat agent somehow left swapped in between turns is put back at the
    /// start of the next turn, with a warning, before anything else runs:
    /// the turn then classifies and runs as the graph agent's.
    #[test]
    #[serial]
    fn usage_probe_a_delegate_left_swapped_in_is_restored_with_a_warning_before_the_next_turn() {
        crate::testing::install_log_collector();
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_stuck_delegate");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        const MARKER: &str = "Chat agent '";
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            assert!(ctx.enter_delegate_turn());
            assert_eq!(ctx.agent.as_ref().unwrap().name(), chat_agent);
            let before = crate::testing::warn_snapshot().len();

            let seen = run_turn(&mut ctx, "do the thing").await;

            let warnings: Vec<String> = crate::testing::warn_snapshot()
                .split_off(before)
                .into_iter()
                .filter(|m| m.contains(MARKER))
                .collect();
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            let expected = format!(
                "Chat agent '{chat_agent}' was still swapped in for graph agent '{name}' at the start of a turn; restoring the graph agent"
            );
            assert!(
                warnings[0].contains(&expected),
                "{expected:?} in {warnings:?}"
            );
            assert_eq!(route_calls(&seen), 1);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
            let session = ctx.session.as_ref().unwrap();
            assert!(session.last_run().is_some());
            assert_eq!(
                session.messages().last().unwrap().content.to_text(),
                "do the thing"
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // A turn that starts with the graph agent in place warns nothing.
            let before = crate::testing::warn_snapshot().len();
            run_turn(&mut ctx, "again").await;
            assert!(
                !crate::testing::warn_snapshot()
                    .split_off(before)
                    .iter()
                    .any(|m| m.contains(MARKER))
            );
        });
    }

    /// The restore of a chat agent left swapped in runs before the turn's
    /// own `turn.started`, so that event names the graph agent like every
    /// later one does; a stuck delegate never shows up as a turn of its own.
    #[test]
    #[serial]
    fn a_turn_after_a_stuck_delegate_names_the_graph_agent_from_turn_started_on() {
        if !bash_and_jq_available() {
            return;
        }
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-stuck-delegate-h7p";
        let name = unique("repl_stuck_delegate_hooks");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            test_sink::drain();
            assert!(ctx.enter_delegate_turn());
            assert_eq!(ctx.agent.as_ref().unwrap().name(), chat_agent);

            run_turn(&mut ctx, "do the thing").await;

            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            for event in ["turn.started", "turn.completed"] {
                let capture = capture_for(marker, event);
                assert_eq!(
                    capture.envs.get("COYOTE_AGENT_NAME"),
                    Some(&name),
                    "{event}"
                );
                assert!(
                    !capture.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"),
                    "{event}"
                );
            }
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// A delegated chat that fails still reports as the graph agent's turn
    /// and names the chat agent; an inline chat block names no chat agent
    /// on any turn event.
    #[test]
    #[serial]
    fn usage_probe_delegated_turn_failed_names_both_agents_and_inline_chat_names_none() {
        if !bash_and_jq_available() {
            return;
        }
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-failed-k2v";
        let name = unique("repl_delegate_failed");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        let inline = unique("repl_inline_hooks");
        write_graph_agent(&inline, &format!("{ADMIT_GLOBAL_HOOKS}{SESSION_BLOCK}"));
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![], Arc::clone(&seen));

            let err = turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap_err();

            assert_eq!(err.to_string(), "no scripted chat reply left");
            assert_eq!(turn_counts(marker), (1, 0, 0, 1));
            let failed = capture_for(marker, "turn.failed");
            assert_eq!(failed.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                failed.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent)
            );
            assert_eq!(graph_envs(&failed), (Some("chat"), Some("classified")));
            assert_eq!(
                failed.envs.get("COYOTE_ERROR").map(String::as_str),
                Some("no scripted chat reply left")
            );
            let started = capture_for(marker, "turn.started");
            assert!(!started.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // The same block answered inline: no chat agent to name.
            test_sink::drain();
            let mut ctx = engage_graph_agent(ctx_with_hooks(&TURN_EVENTS, marker), &inline).await;
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "what now?", None, &mut runner)
                .await
                .unwrap();
            assert_eq!(turn_counts(marker), (1, 1, 0, 0));
            let completed = capture_for(marker, "turn.completed");
            assert_eq!(graph_envs(&completed), (Some("chat"), Some("classified")));
            assert_eq!(completed.envs.get("COYOTE_AGENT_NAME"), Some(&inline));
            assert!(!completed.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
        });
    }

    /// Ctrl-C during the chat agent's tool round ends the delegated turn
    /// normally with the interrupted marker and no guardrail phantoms; the
    /// `turn.interrupted` event is the graph agent's and names the chat
    /// agent the way completed and failed do, and the graph agent is back in
    /// the slot afterwards. An autosaving session flushes the interrupted
    /// round once, under the graph agent's model.
    #[test]
    #[serial]
    fn delegated_interrupted_turn_names_the_chat_agent_on_turn_interrupted() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-interrupted-c3t";
        let name = unique("repl_delegate_interrupted");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            test_sink::drain();
            let scope = ctx.session.as_ref().unwrap().scope();
            let path = ctx.session_file_for("s", scope);
            let session = ctx.session.as_mut().unwrap();
            session.save("s", &path, false).unwrap();
            session.set_save_session(Some(true));
            let writes_before = Session::save_temp_seq();
            let seen: SeenInputs = Default::default();
            let recorder = Arc::clone(&seen);
            let mut round = Some(vec![ToolResult::new(
                ToolCall::new(
                    "agent__collect".into(),
                    json!({"id": "x"}),
                    Some("id-1".into()),
                ),
                json!({"status": "interrupted"}),
            )]);
            let mut runner = completion_runner(move |input, _ctx, abort| {
                recorder.lock().push(input.clone());
                let reply = if input.role().name() == ROUTE_ROLE_NAME {
                    Ok((r#"{"decision":"chat"}"#.to_string(), vec![]))
                } else {
                    // The user hits Ctrl-C while the chat agent's tool round
                    // is in flight; the round still comes back.
                    abort.set_ctrlc();
                    Ok((String::new(), round.take().expect("one chat call")))
                };
                boxed_completion(async move { reply })
            });

            turn(&mut ctx, "what happened?", None, &mut runner)
                .await
                .unwrap();

            assert_eq!(route_calls(&seen.lock()), 1);
            assert_eq!(chat_inputs(&seen.lock()).len(), 1);
            let exchange = exchange(&ctx);
            let roles: Vec<MessageRole> = exchange.iter().map(|(role, _)| *role).collect();
            assert_eq!(
                roles,
                [MessageRole::User, MessageRole::Tool, MessageRole::Assistant]
            );
            assert_eq!(exchange[2].1, TURN_INTERRUPTED_TEXT);
            assert!(
                exchange
                    .iter()
                    .all(|(_, text)| !text.contains("[SYSTEM GUARDRAIL]")),
                "the guardrail must not inject reminders into an interrupted turn"
            );
            assert!(
                ctx.last_message
                    .as_ref()
                    .is_some_and(|v| v.continuous && v.output.is_empty()),
                "the marker is bookkeeping, not a reply to continue"
            );
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));
            assert_eq!(turn_counts(marker), (1, 0, 1, 0));
            let interrupted = capture_for(marker, "turn.interrupted");
            assert_eq!(interrupted.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                interrupted.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent)
            );
            assert_eq!(graph_envs(&interrupted), (Some("chat"), Some("classified")));
            assert!(!interrupted.envs.contains_key("COYOTE_ERROR"));
            let started = capture_for(marker, "turn.started");
            assert!(!started.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
            assert_eq!(
                Session::save_temp_seq() - writes_before,
                1,
                "the interrupted tool round lands in one write"
            );
            assert!(
                !ctx.session.as_ref().unwrap().dirty(),
                "the flush left nothing pending"
            );
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert!(on_disk.contains(TURN_INTERRUPTED_TEXT));
            assert!(on_disk.contains("what happened?"));
            assert!(
                !on_disk.contains(CHAT_AGENT_MODEL),
                "the chat agent's model must not reach the file"
            );
        });
    }

    /// Ctrl-C has two shapes. A streaming completion returns normally with
    /// the abort flag raised (the test above); a spinner-wrapped one bails
    /// with "Aborted!" instead, so the turn surfaces an error. Under
    /// delegation the aborted shape must still produce the graph agent's
    /// `turn.interrupted` naming the chat agent, with no `COYOTE_ERROR`,
    /// and leave the graph agent back in the slot.
    #[test]
    #[serial]
    fn delegated_interrupted_turn_names_the_chat_agent_on_an_aborted_completion() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-aborted-k9f";
        let name = unique("repl_delegate_aborted");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            test_sink::drain();
            let seen: SeenInputs = Default::default();
            let recorder = Arc::clone(&seen);
            let mut runner = completion_runner(move |input, _ctx, abort| {
                recorder.lock().push(input.clone());
                let reply = if input.role().name() == ROUTE_ROLE_NAME {
                    Ok((r#"{"decision":"chat"}"#.to_string(), vec![]))
                } else {
                    // The user hits Ctrl-C while the chat agent's answer is
                    // in flight behind a spinner.
                    abort.set_ctrlc();
                    Err(anyhow!("Aborted!"))
                };
                boxed_completion(async move { reply })
            });

            let result = turn(&mut ctx, "what happened?", None, &mut runner).await;

            assert!(result.is_err(), "the aborted turn surfaces its abort");
            assert_eq!(route_calls(&seen.lock()), 1);
            assert_eq!(chat_inputs(&seen.lock()).len(), 1);
            assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));
            assert_eq!(turn_counts(marker), (1, 0, 1, 0));
            let interrupted = capture_for(marker, "turn.interrupted");
            assert_eq!(interrupted.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                interrupted.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent)
            );
            assert_eq!(graph_envs(&interrupted), (Some("chat"), Some("classified")));
            assert!(!interrupted.envs.contains_key("COYOTE_ERROR"));
            let started = capture_for(marker, "turn.started");
            assert!(!started.envs.contains_key("COYOTE_GRAPH_CHAT_AGENT"));
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// The chat agent's model is overlaid on the request only: after a
    /// delegated turn the shared session still carries the graph agent's
    /// model and server list, so the next run and a later resume stay on
    /// the graph agent's configuration.
    #[test]
    #[serial]
    fn usage_probe_delegated_turn_leaves_the_sessions_model_and_servers_as_the_graph_agents() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_session_knobs");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let session_model = format!("openai:{SESSION_MODEL}");
            let before_servers = ctx.session.as_ref().unwrap().enabled_mcp_servers();
            assert_eq!(ctx.session.as_ref().unwrap().model().id(), session_model);

            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "hello", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            assert_eq!(
                chat_inputs(&seen)[0].role().model().id(),
                format!("openai:{CHAT_AGENT_MODEL}"),
                "the request went to the chat agent's model"
            );
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.model().id(), session_model);
            assert_eq!(session.to_role().model().id(), session_model);
            assert_eq!(session.enabled_mcp_servers(), before_servers);
            assert_eq!(
                ctx.current_model().id(),
                session_model,
                "the next run and the classifier read the session's model"
            );
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // The run that follows classifies and records under that model.
            let seen = run_turn(&mut ctx, "do the thing").await;
            assert_eq!(seen[0].role().model().id(), session_model);
        });
    }

    /// A delegated chat turn flushes an autosaving session the way a plain
    /// chat turn does: once per turn, with nothing left dirty, and the file
    /// keeps the graph agent's model even though the request went to the
    /// chat agent's.
    #[test]
    #[serial]
    fn delegated_turn_flushes_an_autosaving_session_under_the_graph_agents_model() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let name = unique("repl_delegate_autosave");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            // The file is read back through the resume path, so the session
            // sits on a model the app can resolve again.
            let ctx = repl_ctx();
            let session_model = Model::retrieve_model(
                &ctx.app.config,
                "test-seeded:session-model",
                crate::client::ModelType::Chat,
            )
            .unwrap();
            let mut ctx = engage_delegating_graph_agent(ctx, &name, session_model).await;
            let app = Arc::clone(&ctx.app.config);
            let scope = ctx.session.as_ref().unwrap().scope();
            let path = ctx.session_file_for("s", scope);
            let session = ctx.session.as_mut().unwrap();
            session.save("s", &path, false).unwrap();
            session.set_save_session(Some(true));

            for (prompt, reply) in [("hello", "hi"), ("still there?", "yes")] {
                let writes_before = Session::save_temp_seq();
                let seen: SeenInputs = Default::default();
                let mut runner = scripted_runner("chat", vec![(reply, vec![])], Arc::clone(&seen));
                turn(&mut ctx, prompt, None, &mut runner).await.unwrap();

                assert_eq!(
                    chat_inputs(&seen.lock())[0].role().model().id(),
                    format!("openai:{CHAT_AGENT_MODEL}")
                );
                assert_eq!(
                    Session::save_temp_seq() - writes_before,
                    1,
                    "{prompt}: the exchange lands in one write"
                );
                let session = ctx.session.as_ref().unwrap();
                assert!(!session.dirty(), "{prompt}: the flush left nothing pending");
                let on_disk = Session::load_from_ctx(&ctx, &app, "s", &path, scope).unwrap();
                assert_eq!(on_disk.model().id(), "test-seeded:session-model");
                assert!(
                    !std::fs::read_to_string(&path)
                        .unwrap()
                        .contains(CHAT_AGENT_MODEL),
                    "{prompt}: the chat agent's model must not reach the file"
                );
                assert_eq!(on_disk.messages().len(), session.messages().len());
                let [.., user, assistant] = on_disk.messages() else {
                    panic!(
                        "expected the exchange on disk, got {:?}",
                        on_disk.messages()
                    );
                };
                assert!(user.role.is_user());
                assert_eq!(user.content.to_text(), prompt);
                assert!(assistant.role.is_assistant());
                assert_eq!(assistant.content.to_text(), reply);
            }
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// The turn after a soft Ctrl-C on a delegated chat turn is an ordinary
    /// delegated turn: the interrupted round left the chat agent parked
    /// (nothing to restore, so no restore warning), the next prompt is
    /// classified afresh and answered by the chat agent on its own model,
    /// the interrupted marker is history rather than a reply to continue,
    /// and the autosaving session lands each turn in one write that keeps
    /// the graph agent's model.
    #[test]
    #[serial]
    fn usage_probe_a_delegated_turn_after_a_soft_interrupt_runs_clean_and_flushes_once_more() {
        crate::testing::install_log_collector();
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "gt-delegate-after-interrupt-p4m";
        const RESTORE_WARNING: &str = "was still swapped in for graph agent";
        let name = unique("repl_delegate_after_interrupt");
        let chat_agent = unique("repl_chat_agent");
        write_chat_agent(&chat_agent, CHAT_AGENT_BODY);
        write_delegating_graph_agent(&name, ADMIT_GLOBAL_HOOKS, &chat_agent);
        run_async(async {
            let mut ctx = engage_delegating_graph_agent(
                ctx_with_hooks(&TURN_EVENTS, marker),
                &name,
                fc_model(SESSION_MODEL),
            )
            .await;
            test_sink::drain();
            let scope = ctx.session.as_ref().unwrap().scope();
            let path = ctx.session_file_for("s", scope);
            let session = ctx.session.as_mut().unwrap();
            session.save("s", &path, false).unwrap();
            session.set_save_session(Some(true));

            // Turn 1: Ctrl-C lands while the chat agent's tool round is in
            // flight; the round still comes back and the turn ends normally.
            let mut round = Some(vec![ToolResult::new(
                ToolCall::new(
                    "agent__collect".into(),
                    json!({"id": "x"}),
                    Some("id-1".into()),
                ),
                json!({"status": "interrupted"}),
            )]);
            let mut interrupted = completion_runner(move |input, _ctx, abort| {
                let reply = if input.role().name() == ROUTE_ROLE_NAME {
                    Ok((r#"{"decision":"chat"}"#.to_string(), vec![]))
                } else {
                    abort.set_ctrlc();
                    Ok((String::new(), round.take().expect("one chat call")))
                };
                boxed_completion(async move { reply })
            });
            turn(&mut ctx, "what happened?", None, &mut interrupted)
                .await
                .unwrap();
            assert_eq!(turn_counts(marker), (1, 0, 1, 0));
            assert_graph_agent_restored(&ctx, &name, &chat_agent);

            // Turn 2: a plain prompt, with a fresh abort signal as the REPL
            // hands every turn.
            let warnings_before = crate::testing::warn_snapshot().len();
            let writes_before = Session::save_temp_seq();
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("yes", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "still there?", None, &mut runner)
                .await
                .unwrap();

            let seen = seen.lock().clone();
            assert_eq!(route_calls(&seen), 1, "the turn is classified afresh");
            assert_eq!(
                chat_inputs(&seen)[0].role().model().id(),
                format!("openai:{CHAT_AGENT_MODEL}")
            );
            assert!(
                !crate::testing::warn_snapshot()
                    .split_off(warnings_before)
                    .iter()
                    .any(|m| m.contains(RESTORE_WARNING)),
                "the interrupted turn parked the chat agent itself"
            );
            assert_eq!(turn_counts(marker), (2, 1, 1, 0));
            let completed = capture_for(marker, "turn.completed");
            assert_eq!(completed.envs.get("COYOTE_AGENT_NAME"), Some(&name));
            assert_eq!(
                completed.envs.get("COYOTE_GRAPH_CHAT_AGENT"),
                Some(&chat_agent)
            );
            let exchange = exchange(&ctx);
            let roles: Vec<MessageRole> = exchange.iter().map(|(role, _)| *role).collect();
            assert_eq!(
                roles,
                [
                    MessageRole::User,
                    MessageRole::Tool,
                    MessageRole::Assistant,
                    MessageRole::User,
                    MessageRole::Assistant,
                ]
            );
            assert_eq!(exchange[2].1, TURN_INTERRUPTED_TEXT);
            assert_eq!(exchange[3].1, "still there?");
            assert_eq!(exchange[4].1, "yes");
            assert_eq!(Session::save_temp_seq() - writes_before, 1);
            assert!(!ctx.session.as_ref().unwrap().dirty());
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert!(on_disk.contains(TURN_INTERRUPTED_TEXT), "{on_disk}");
            assert!(on_disk.contains("still there?"), "{on_disk}");
            assert!(
                on_disk.contains(&format!("openai:{SESSION_MODEL}")),
                "{on_disk}"
            );
            assert!(!on_disk.contains(CHAT_AGENT_MODEL), "{on_disk}");
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    /// "All of the chat agent's tools" means its exact pool: the delegated
    /// request declares the same function names a user engaging the chat
    /// agent directly with `.agent` would get, nothing more and nothing
    /// less.
    #[test]
    #[serial]
    fn usage_probe_delegated_request_declares_exactly_the_directly_engaged_chat_agents_pool() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        write_global_memory_store();
        let name = unique("repl_exact_pool");
        let chat_agent = unique("repl_chat_agent");
        // `tone` gets a default so the direct engage needs no prompt; the
        // block still pre-sets it.
        write_chat_agent(
            &chat_agent,
            &format!("{CHAT_AGENT_BODY}    default: terse\nmemory: true\n"),
        );
        write_delegating_graph_agent(&name, "", &chat_agent);
        run_async(async {
            // Engaged into a session too, since the pool itself depends on
            // one being active (`todo__pause` needs cross-turn history).
            let mut direct = repl_ctx();
            direct.model = fc_model(CHAT_AGENT_MODEL);
            let app = direct.app.config.clone();
            direct
                .use_agent(&app, &chat_agent, Some("direct"), create_abort_signal())
                .await
                .unwrap();
            let direct_names = Input::from_str(&direct, "hello", None)
                .unwrap()
                .declared_function_names();
            assert!(direct_names.contains("local_hello"), "{direct_names:?}");
            assert!(
                direct_names
                    .iter()
                    .any(|n| n.starts_with(MEMORY_FUNCTION_PREFIX)),
                "{direct_names:?}"
            );

            let mut ctx =
                engage_delegating_graph_agent(repl_ctx(), &name, fc_model(SESSION_MODEL)).await;
            let seen: SeenInputs = Default::default();
            let mut runner = scripted_runner("chat", vec![("hi", vec![])], Arc::clone(&seen));
            turn(&mut ctx, "hello", None, &mut runner).await.unwrap();

            let seen = seen.lock().clone();
            let delegated_names = chat_inputs(&seen)[0].declared_function_names();
            assert_eq!(delegated_names, direct_names);
            assert_graph_agent_restored(&ctx, &name, &chat_agent);
        });
    }

    #[test]
    #[serial]
    fn compress_command_failure_fires_no_session_compressed() {
        let _sink = test_sink::install();
        let marker = "compress-fail-z9t";
        run_async(async {
            let mut ctx = ctx_with_hooks(&["session.compressed"], marker);
            ctx.session = Some(Session::default());
            let abort_signal = create_abort_signal();

            // Boxed: `run_repl_command`'s state machine is far larger than a
            // test thread's stack.
            let result = Box::pin(run_repl_command(
                &mut ctx,
                abort_signal,
                ".compress session",
            ))
            .await;

            assert!(result.is_err(), "compressing an empty session must fail");
        });
        assert_eq!(
            test_sink::snapshot()
                .iter()
                .filter(|capture| capture.hook_name.starts_with(marker))
                .count(),
            0,
            "a failed compression must fire no session.compressed hook"
        );
    }

    /// A ctx whose compression can actually SUCCEED offline: `dry_run`
    /// makes the summarization call echo instead of hitting the network,
    /// the default openai client config makes the session model
    /// resolvable, and the session already holds a user exchange so
    /// `compress_session` has something to summarize.
    fn compressible_ctx(marker: &str) -> RequestContext {
        compressible_ctx_with(marker, AppConfig::default())
    }

    /// `compressible_ctx` with caller-tuned compression settings, so tests
    /// can drive `needs_compression` and the safety valve; `dry_run`, the
    /// default client, and disabled memory/workspace instructions are
    /// enforced here to keep compression offline and hermetic.
    fn compressible_ctx_with(marker: &str, config: AppConfig) -> RequestContext {
        let mut hooks_map = HooksMap::default();
        hooks_map.insert(
            "session.compressed".to_string(),
            vec![HookDef {
                name: format!("{marker}-session.compressed"),
                command: "true".to_string(),
            }],
        );
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            hooks: hooks_map,
            dry_run: true,
            clients: vec![ClientConfig::default()],
            // The compression assertions measure the session's own text.
            // The host's memory indexes (global config dir and the CWD's
            // ancestor workspaces) and instruction files would otherwise ride
            // the system prompt.
            memory: Some(false),
            workspace_instructions: Some(false),
            ..config
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
        let mut session = Session::default();
        session.set_model(Model::new("openai", "test-compress-model"));
        ctx.session = Some(session);
        let input = Input::from_str(&ctx, "hello there", None).unwrap();
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&input, "hi")
            .unwrap();
        ctx.session.as_mut().unwrap().update_tokens();
        ctx
    }

    fn session_compressed_count(marker: &str) -> usize {
        test_sink::snapshot()
            .iter()
            .filter(|capture| capture.hook_name == format!("{marker}-session.compressed"))
            .count()
    }

    #[test]
    #[serial]
    fn compress_command_success_fires_session_compressed_once() {
        let _sink = test_sink::install();
        let marker = "compress-ok-w2p";
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            let abort_signal = create_abort_signal();

            // Boxed: `run_repl_command`'s state machine is far larger than a
            // test thread's stack.
            let result = Box::pin(run_repl_command(
                &mut ctx,
                abort_signal,
                ".compress session",
            ))
            .await;

            assert!(
                result.is_ok(),
                "dry-run compression must succeed: {result:?}"
            );
        });
        assert_eq!(
            session_compressed_count(marker),
            1,
            "a successful `.compress session` must fire session.compressed exactly once"
        );
    }

    #[test]
    #[serial]
    fn compress_command_fires_session_compressed_for_a_block_graph_agent() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "compress-graph-k4m";
        let name = unique("repl_compress_graph");
        write_graph_agent(&name, &format!("{SESSION_BLOCK}{ADMIT_GLOBAL_HOOKS}"));
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            // `use_agent` refuses to run over an attached session; the
            // engaged agent then gets the compressible one back.
            let session = ctx.session.take();
            let app = ctx.app.config.clone();
            ctx.use_agent(&app, &name, None, create_abort_signal())
                .await
                .unwrap();
            ctx.session = session;

            let result = repl_command(&mut ctx, ".compress session").await;

            assert!(
                result.is_ok(),
                "dry-run compression must succeed: {result:?}"
            );
        });
        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn auto_compress_success_fires_session_compressed_once() {
        // Covers the ask-loop call site through its extracted seam;
        // driving `needs_compression` itself requires a full LLM turn and
        // is exercised end-to-end by integration coverage, not here.
        let _sink = test_sink::install();
        let marker = "auto-compress-ok-f8r";
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            assert!(auto_compress_session(&mut ctx, app.as_ref()).await);
        });

        assert_eq!(
            session_compressed_count(marker),
            1,
            "a successful auto-compression must fire session.compressed exactly once"
        );
    }

    #[test]
    #[serial]
    fn session_compressed_carries_the_active_role_name() {
        let _sink = test_sink::install();
        let marker = "compress-role-t6b";
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            // A temp role (`--prompt`) is a real role for COYOTE_ROLE.
            ctx.role = Some(Role::new(TEMP_ROLE_NAME, "Session role prompt"));
            let app = Arc::clone(&ctx.app.config);
            auto_compress_session(&mut ctx, app.as_ref()).await;
        });

        let capture = test_sink::snapshot()
            .into_iter()
            .find(|capture| capture.hook_name == format!("{marker}-session.compressed"))
            .expect("session.compressed capture");
        assert_eq!(
            capture.envs.get("COYOTE_ROLE").map(String::as_str),
            Some(TEMP_ROLE_NAME)
        );
    }

    #[test]
    #[serial]
    fn session_compressed_omits_the_role_var_when_no_role_is_active() {
        let _sink = test_sink::install();
        let marker = "compress-norole-p9c";
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            auto_compress_session(&mut ctx, app.as_ref()).await;
        });

        let capture = test_sink::snapshot()
            .into_iter()
            .find(|capture| capture.hook_name == format!("{marker}-session.compressed"))
            .expect("session.compressed capture");
        assert!(
            !capture.envs.contains_key("COYOTE_ROLE"),
            "session.compressed must omit COYOTE_ROLE without an active role"
        );
    }

    #[test]
    #[serial]
    fn auto_compress_failure_fires_no_session_compressed_and_swallows_error() {
        let _sink = test_sink::install();
        let marker = "auto-compress-fail-d3k";
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            // An empty session makes compress_session fail; the seam must
            // swallow the error (no panic, no propagation) and fire nothing.
            ctx.session = Some(Session::default());
            let app = Arc::clone(&ctx.app.config);
            assert!(!auto_compress_session(&mut ctx, app.as_ref()).await);
        });

        assert_eq!(
            session_compressed_count(marker),
            0,
            "a failed auto-compression must fire no session.compressed hook"
        );
    }

    #[test]
    #[serial]
    fn maybe_auto_compress_skips_when_neither_threshold_nor_valve_fires() {
        let _sink = test_sink::install();
        let marker = "maybe-compress-skip-q7m";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            let app = Arc::clone(&ctx.app.config);

            assert!(!maybe_auto_compress(&mut ctx, app.as_ref()).await.fired);
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    #[serial]
    fn maybe_auto_compress_fires_on_threshold_each_loop_iteration() {
        // Exercises the shared seam the ask loop calls at the top of every
        // iteration (tool-loop, guardrail-inject, and auto-continue turns
        // included) as well as at turn end.
        let _sink = test_sink::install();
        let marker = "maybe-compress-threshold-n2x";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1,
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            let app = Arc::clone(&ctx.app.config);

            assert!(maybe_auto_compress(&mut ctx, app.as_ref()).await.succeeded);
            assert!(
                !ctx.is_compressing_session(),
                "the compressing flag must be released after compression"
            );
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn maybe_auto_compress_reports_no_success_when_compression_fails() {
        let _sink = test_sink::install();
        let marker = "maybe-compress-fail-g4z";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1,
                    compression_keep_last: 0,
                    ..Default::default()
                },
            );
            // The threshold fires, but the summarization call fails: the
            // session's model has no configured client.
            ctx.session
                .as_mut()
                .unwrap()
                .set_model(Model::new("unconfigured", "no-client"));
            let app = Arc::clone(&ctx.app.config);

            let outcome = maybe_auto_compress(&mut ctx, app.as_ref()).await;
            assert!(outcome.fired, "the compression condition must fire");
            assert!(
                !outcome.succeeded,
                "a failed compression must not report success"
            );
            assert!(
                !ctx.is_compressing_session(),
                "the compressing flag must be released after a failed compression"
            );
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    #[serial]
    fn failed_compression_still_reports_fired_with_kept_tail() {
        // Pins the turn-end auto-continue gate: with keep_last > 0, a
        // compression whose condition fired but whose summarization FAILED
        // must still report `fired`, so an unattended agent auto-continues
        // and retries the compression at the next turn end instead of
        // stalling at the pause banner.
        let _sink = test_sink::install();
        let marker = "maybe-compress-fired-fail-t8r";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1,
                    compression_keep_last: 2,
                    ..Default::default()
                },
            );
            // The foldable middle is a tool transcript and the kept tail is
            // the final exchange: the threshold fires, but the summarization
            // call fails because the session's model has no configured client.
            {
                let tool_input = Input::from_str(&ctx, "more context", None)
                    .unwrap()
                    .merge_tool_results(
                        "calling a tool to gather context".into(),
                        vec![ToolResult::new(ToolCall::default(), json!("ok"))],
                    );
                let last_input = Input::from_str(&ctx, "and then?", None).unwrap();
                let session = ctx.session.as_mut().unwrap();
                session.add_message(&tool_input, &"y".repeat(400)).unwrap();
                session.add_message(&last_input, "that's all").unwrap();
                session.set_model(Model::new("unconfigured", "no-client"));
            }
            let app = Arc::clone(&ctx.app.config);

            let outcome = maybe_auto_compress(&mut ctx, app.as_ref()).await;
            assert!(outcome.fired, "the compression condition must fire");
            assert!(
                !outcome.succeeded,
                "a failed compression must not report success"
            );
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    #[serial]
    fn loop_top_compression_refresh_rebuilds_from_compressed_session() {
        let _sink = test_sink::install();
        let marker = "loop-top-refresh-h5w";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1,
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            let app = Arc::clone(&ctx.app.config);
            let mut input = Input::from_str(&ctx, "next turn", None).unwrap();
            let stale = serde_json::to_string(&input.build_messages().unwrap()).unwrap();

            assert!(maybe_auto_compress(&mut ctx, app.as_ref()).await.succeeded);
            input.refresh_session(&ctx);

            let refreshed = serde_json::to_string(&input.build_messages().unwrap()).unwrap();
            assert_ne!(
                refreshed, stale,
                "the request built after a loop-top compression must reflect the compressed session"
            );
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn maybe_auto_compress_fires_on_safety_valve_despite_huge_threshold() {
        let _sink = test_sink::install();
        let marker = "maybe-compress-valve-v5c";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1_000_000,
                    compression_keep_last: 1,
                    compression_safety_valve: 0.8,
                    ..Default::default()
                },
            );
            // A distinct name: `set_model` is a no-op for an unchanged model
            // id, and the ctx's default session model declares no limit.
            let mut data = ModelData::new("test-compress-model-limited");
            data.max_input_tokens = Some(100_000);
            let model = Model::from_config("openai", &[data]).remove(0);
            ctx.session.as_mut().unwrap().set_model(model);
            // The last successful request measured a prompt near the limit
            // even though the local estimate is tiny.
            ctx.last_prompt_token_usage = Some(90_000);
            let app = Arc::clone(&ctx.app.config);

            assert!(maybe_auto_compress(&mut ctx, app.as_ref()).await.succeeded);
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    /// A ctx where only the safety valve can trigger compression: the last
    /// successful request measured a prompt near the session model's limit
    /// while the threshold is out of reach.
    fn valve_tripped_ctx(marker: &str) -> RequestContext {
        let mut ctx = compressible_ctx_with(
            marker,
            AppConfig {
                compression_threshold: 1_000_000,
                compression_keep_last: 1,
                compression_safety_valve: 0.8,
                ..Default::default()
            },
        );
        let mut data = ModelData::new("test-compress-model-limited");
        data.max_input_tokens = Some(100_000);
        let model = Model::from_config("openai", &[data]).remove(0);
        ctx.session.as_mut().unwrap().set_model(model);
        ctx.last_prompt_token_usage = Some(90_000);
        ctx
    }

    #[test]
    #[serial]
    fn successful_compression_clears_stale_prompt_measurement() {
        let _sink = test_sink::install();
        let marker = "maybe-compress-stale-p6b";
        run_async(async {
            let mut ctx = valve_tripped_ctx(marker);
            ctx.valve_checkpoint_floor = Some(123);
            let app = Arc::clone(&ctx.app.config);

            assert!(maybe_auto_compress(&mut ctx, app.as_ref()).await.succeeded);
            assert_eq!(
                ctx.last_prompt_token_usage, None,
                "a successful compression must drop the pre-compression prompt measurement"
            );
            assert_eq!(
                ctx.valve_checkpoint_floor, None,
                "a successful compression must release the checkpoint latch"
            );

            // A subsequent usage-less turn makes tokens reclaimable again;
            // the valve must not re-fire from the vanished prompt's
            // measurement.
            {
                let input = Input::from_str(&ctx, "next turn", None).unwrap();
                let session = ctx.session.as_mut().unwrap();
                session.add_message(&input, "reply").unwrap();
            }
            assert!(!maybe_auto_compress(&mut ctx, app.as_ref()).await.fired);
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    /// Counts `on_chat_completion_error` checkpoints in the session; the
    /// literal must match `INTERRUPTED_RESPONSE_TEXT`, whose presence is
    /// pinned by the give-up tests below.
    fn interrupted_checkpoint_count(ctx: &RequestContext) -> usize {
        ctx.session.as_ref().map_or(0, |session| {
            session
                .messages()
                .iter()
                .filter(|message| {
                    matches!(&message.content, MessageContent::Text(text)
                        if text.ends_with("[Response interrupted due to error]"))
                })
                .count()
        })
    }

    #[test]
    #[serial]
    fn error_retry_compresses_once_without_checkpointing() {
        let _sink = test_sink::install();
        let marker = "error-retry-valve-k1s";
        run_async(async {
            let mut ctx = valve_tripped_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();

            assert!(
                compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
            assert!(
                !ctx.is_compressing_session(),
                "the compressing flag must be released after compression"
            );
            assert_eq!(
                interrupted_checkpoint_count(&ctx),
                0,
                "the retry path must not checkpoint the in-flight turn"
            );
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn error_retry_gives_up_on_second_failure_with_single_checkpoint() {
        let _sink = test_sink::install();
        let marker = "error-retry-giveup-p4t";
        run_async(async {
            let mut ctx = valve_tripped_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();
            let input = Input::from_str(&ctx, "still too big", None).unwrap();

            assert!(
                compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
            // The retried request failed too: no second compression, and the
            // give-up path checkpoints the interrupted turn exactly once.
            assert!(
                !compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    true,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
            ctx.on_chat_completion_error(app.as_ref(), &input);
            assert_eq!(interrupted_checkpoint_count(&ctx), 1);
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn error_retry_declines_when_valve_is_quiet() {
        let _sink = test_sink::install();
        let marker = "error-retry-quiet-m8d";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();
            let input = Input::from_str(&ctx, "transient failure", None).unwrap();

            assert!(
                !compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
            // Valve-false errors keep today's behavior: checkpoint and
            // surface the error immediately, no compression, no retry.
            ctx.on_chat_completion_error(app.as_ref(), &input);
            assert_eq!(interrupted_checkpoint_count(&ctx), 1);
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    #[serial]
    fn error_retry_flag_blocks_repeat_and_rearms_on_fresh_input() {
        let _sink = test_sink::install();
        let marker = "error-retry-flag-c6y";
        run_async(async {
            let mut ctx = valve_tripped_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();

            // Tool-loop iterations carry the flag forward: a set flag wins
            // over a still-firing valve.
            assert!(
                !compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    true,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
            assert_eq!(session_compressed_count(marker), 0);
            // A fresh input rebuild clears the flag and re-arms the retry.
            assert!(
                compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn error_retry_refreshed_input_rebuilds_from_compressed_session() {
        let _sink = test_sink::install();
        let marker = "error-retry-refresh-j7q";
        run_async(async {
            let mut ctx = valve_tripped_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();
            let mut input = Input::from_str(&ctx, "still too big", None).unwrap();
            let stale = serde_json::to_string(&input.build_messages().unwrap()).unwrap();

            assert!(
                compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await
            );
            // The snapshot captured at construction is untouched by the
            // compression; retrying without a refresh would re-send the
            // failed prompt byte for byte.
            assert_eq!(
                serde_json::to_string(&input.build_messages().unwrap()).unwrap(),
                stale
            );

            input.refresh_session(&ctx);
            let refreshed = serde_json::to_string(&input.build_messages().unwrap()).unwrap();
            assert_ne!(refreshed, stale);
            let rebuilt = Input::from_str(&ctx, "still too big", None).unwrap();
            assert_eq!(
                refreshed,
                serde_json::to_string(&rebuilt.build_messages().unwrap()).unwrap(),
                "a refreshed input must build the same request as one created after compression"
            );
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn error_retry_declines_when_user_aborted() {
        let _sink = test_sink::install();
        let marker = "error-retry-abort-b2n";
        run_async(async {
            let mut ctx = valve_tripped_ctx(marker);
            let app = Arc::clone(&ctx.app.config);
            let input = Input::from_str(&ctx, "killed mid-request", None).unwrap();

            let ctrlc = create_abort_signal();
            ctrlc.set_ctrlc();
            assert!(
                !compress_for_error_retry(&mut ctx, app.as_ref(), false, &ctrlc, &anyhow!("boom"))
                    .await
            );
            let ctrld = create_abort_signal();
            ctrld.set_ctrld();
            assert!(
                !compress_for_error_retry(&mut ctx, app.as_ref(), false, &ctrld, &anyhow!("boom"))
                    .await
            );

            // A user-killed request keeps today's behavior: checkpoint and
            // surface the error, no compression, no re-send.
            ctx.on_chat_completion_error(app.as_ref(), &input);
            assert_eq!(interrupted_checkpoint_count(&ctx), 1);
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    /// Appends `[User, Tool(giant), Assistant]` to the session: one tool
    /// loop whose results alone dwarf any keep budget.
    fn push_giant_tool_exchange(ctx: &mut RequestContext, results: usize) {
        let input = Input::from_str(ctx, "do everything", None)
            .unwrap()
            .merge_tool_results(
                String::new(),
                (0..results)
                    .map(|i| {
                        ToolResult::new(
                            ToolCall::new("t".into(), json!({}), Some(format!("id-{i}"))),
                            json!("x".repeat(2000)),
                        )
                    })
                    .collect(),
            );
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&input, "done")
            .unwrap();
    }

    fn context_overflow_error() -> anyhow::Error {
        crate::client::catch_error(
            &json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": "prompt is too long: 250000 tokens > 200000 maximum"
                }
            }),
            400,
        )
        .unwrap_err()
    }

    #[test]
    #[serial]
    fn compress_for_error_retry_fires_with_keep_last_covering_window() {
        let _sink = test_sink::install();
        let marker = "error-retry-giant-tail-r3v";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1_000_000,
                    compression_keep_last: 10,
                    compression_safety_valve: 0.8,
                    ..Default::default()
                },
            );
            let mut data = ModelData::new("test-compress-model-limited");
            data.max_input_tokens = Some(100_000);
            let model = Model::from_config("openai", &[data]).remove(0);
            ctx.session.as_mut().unwrap().set_model(model);
            ctx.last_prompt_token_usage = Some(90_000);
            push_giant_tool_exchange(&mut ctx, 60);
            assert!(ctx.session.as_ref().unwrap().messages().len() <= 10);
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();

            assert!(
                compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await,
                "a keep_last that nominally covers the session must not shield a giant tail"
            );
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(
                session.messages().len(),
                1,
                "the giant exchange is folded away"
            );
            assert!(session.messages()[0].role.is_system());
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn compress_for_error_retry_fires_on_context_overflow_error_without_window() {
        let _sink = test_sink::install();
        let marker = "error-retry-overflow-n5w";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1_000_000,
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            assert!(
                ctx.session
                    .as_ref()
                    .unwrap()
                    .model()
                    .max_input_tokens()
                    .is_none()
            );
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();

            assert!(
                !compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &anyhow!("boom")
                )
                .await,
                "without a window the valve stays quiet for ordinary errors"
            );
            assert!(
                compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &context_overflow_error()
                )
                .await,
                "a context-overflow rejection is itself the signal to compress"
            );
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    #[serial]
    fn compress_for_error_retry_declines_overflow_without_session() {
        let _sink = test_sink::install();
        let marker = "error-retry-no-session-k2d";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1_000_000,
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            ctx.session = None;
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();

            assert!(
                !compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &context_overflow_error()
                )
                .await,
                "with nothing to fold, an overflow must not announce a compression"
            );
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    #[serial]
    fn compress_for_error_retry_declines_overflow_for_session_without_user_messages() {
        let _sink = test_sink::install();
        let marker = "error-retry-no-user-messages-p7q";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_threshold: 1_000_000,
                    compression_keep_last: 1,
                    ..Default::default()
                },
            );
            let mut session = Session::default();
            session.set_model(Model::new("openai", "test-compress-model"));
            ctx.session = Some(session);
            let app = Arc::clone(&ctx.app.config);
            let abort_signal = create_abort_signal();

            assert!(
                !compress_for_error_retry(
                    &mut ctx,
                    app.as_ref(),
                    false,
                    &abort_signal,
                    &context_overflow_error()
                )
                .await,
                "a session with no user turn has nothing to fold"
            );
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    fn compression_failure_notice_carries_the_cause_chain() {
        let app = AppConfig::default();
        let err = anyhow!("inner").context("outer");

        let notice = compression_failure_notice(&app, &err);

        assert!(notice.contains("outer") && notice.contains("inner"));
        assert_eq!(
            notice,
            dim_notice_color(&app)
                .italic()
                .paint(format!("⚠️ Compression failed: {err:#}"))
                .to_string()
        );
    }

    #[test]
    #[serial]
    fn compress_command_usage_rejects_non_numeric_keep_last() {
        let _sink = test_sink::install();
        let marker = "compress-usage-q8h";
        run_async(async {
            let mut ctx = compressible_ctx(marker);
            let before = ctx.session.as_ref().unwrap().messages().len();

            // Boxed: `run_repl_command`'s state machine is far larger than a
            // test thread's stack.
            let result = Box::pin(run_repl_command(
                &mut ctx,
                create_abort_signal(),
                ".compress session abc",
            ))
            .await;

            assert!(result.is_ok(), "usage is printed, not raised: {result:?}");
            assert_eq!(ctx.session.as_ref().unwrap().messages().len(), before);
        });

        assert_eq!(session_compressed_count(marker), 0);
    }

    #[test]
    #[serial]
    fn compress_command_keep_last_override_folds_a_covered_session() {
        let _sink = test_sink::install();
        let marker = "compress-override-d2k";
        run_async(async {
            let mut ctx = compressible_ctx_with(
                marker,
                AppConfig {
                    compression_keep_last: 10,
                    ..Default::default()
                },
            );

            let result = Box::pin(run_repl_command(
                &mut ctx,
                create_abort_signal(),
                ".compress session",
            ))
            .await;
            assert!(
                result.is_err(),
                "the configured tail covers the whole session"
            );

            let result = Box::pin(run_repl_command(
                &mut ctx,
                create_abort_signal(),
                ".compress session 0",
            ))
            .await;
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(ctx.session.as_ref().unwrap().messages().len(), 1);
        });

        assert_eq!(session_compressed_count(marker), 1);
    }

    #[test]
    fn test_process_command_line() {
        let parse = |line| parse_command(line, CommandPrefix::Dot);
        assert_eq!(parse(" ."), Some((Cow::Borrowed("."), None)));
        assert_eq!(parse(" .role"), Some((Cow::Borrowed(".role"), None)));
        assert_eq!(parse(" .role  "), Some((Cow::Borrowed(".role"), None)));
        assert_eq!(
            parse(" .set dry_run true"),
            Some((Cow::Borrowed(".set"), Some("dry_run true")))
        );
        assert_eq!(
            parse(" .set dry_run true  "),
            Some((Cow::Borrowed(".set"), Some("dry_run true")))
        );
        assert_eq!(
            parse(".prompt \nabc\n"),
            Some((Cow::Borrowed(".prompt"), Some("abc")))
        );
    }

    #[test]
    fn parse_command_slash_is_chat_under_dot() {
        assert!(parse_command("/usr/bin", CommandPrefix::Dot).is_none());
        assert!(parse_command("/info", CommandPrefix::Dot).is_none());
    }

    #[test]
    fn parse_command_dot_is_chat_under_slash() {
        assert!(parse_command(".info", CommandPrefix::Slash).is_none());
    }

    #[test]
    fn parse_command_normalizes_slash_to_dot_under_slash_and_both() {
        for prefix in [CommandPrefix::Slash, CommandPrefix::Both] {
            assert_eq!(
                parse_command("/info", prefix),
                Some((Cow::Owned(".info".to_string()), None)),
                "{prefix}"
            );
            assert_eq!(
                parse_command("/set dry_run true", prefix),
                Some((Cow::Owned(".set".to_string()), Some("dry_run true"))),
                "{prefix}"
            );
        }
        assert_eq!(
            parse_command(".info", CommandPrefix::Both),
            Some((Cow::Borrowed(".info"), None))
        );
    }

    #[test]
    fn repl_help_text_renders_slash_rows_and_prose() {
        let help = repl_help_text(CommandPrefix::Slash);
        assert!(
            help.lines().any(|line| line.starts_with("/help ")),
            "{help}"
        );
        assert!(
            help.lines().any(|line| line.starts_with("/info tools ")),
            "{help}"
        );
        assert!(!help.contains(".help"), "{help}");
        assert!(help.contains("/<name> [args...]"), "{help}");
        assert!(help.contains(r#""/macro <name>""#), "{help}");
        assert!(help.contains(r#""/list macros""#), "{help}");
        assert!(help.contains(r#""/redraw""#), "{help}");
    }

    fn slash_ctx() -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            command_prefix: CommandPrefix::Slash,
            ..Default::default()
        });
        RequestContext::new(Arc::new(app), WorkingMode::Repl)
    }

    #[test]
    fn unknown_command_hint_follows_command_prefix() {
        let err = unknown_command(&slash_ctx()).unwrap_err().to_string();
        assert!(err.contains(r#""/help""#), "{err}");
        assert!(!err.contains(".help"), "{err}");
    }

    #[test]
    fn slash_command_runs_and_reports_errors_in_slash_form() {
        run_async(async {
            let mut ctx = slash_ctx();
            let abort_signal = create_abort_signal();

            let result = Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "/redraw bogus",
            ))
            .await;
            let err = result.expect_err("unknown sub-command must be rejected");
            assert!(err.to_string().contains(r#""/help""#), "{err}");

            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal,
                "/set dry_run true",
            ))
            .await
            .unwrap();
            assert!(ctx.app.config.dry_run, "the slash-form step must run");
        });
    }

    /// A dry-run context whose chat path completes offline, so a line that
    /// is NOT a command under the configured prefix can be told apart from
    /// an unknown command: the former succeeds and leaves the flags alone,
    /// the latter is an error naming the help command.
    fn chat_capable_ctx(prefix: CommandPrefix) -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            command_prefix: prefix,
            dry_run: true,
            clients: vec![ClientConfig::default()],
            memory: Some(false),
            workspace_instructions: Some(false),
            ..Default::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
        ctx.model = Model::new("openai", "test-model");
        ctx
    }

    #[test]
    fn usage_probe_set_command_prefix_switches_the_live_sigil_mid_session() {
        run_async(async {
            let mut ctx = chat_capable_ctx(CommandPrefix::Dot);
            let abort_signal = create_abort_signal();

            // Under dot, a slash-led line is plain chat: it completes on the
            // dry-run path and flips nothing.
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "/set temperature 0.9",
            ))
            .await
            .expect("a slash-led line is chat under dot, never an unknown command");
            assert_eq!(ctx.app.config.temperature, None);

            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                ".set command_prefix slash",
            ))
            .await
            .unwrap();
            assert_eq!(ctx.command_prefix(), CommandPrefix::Slash);

            // The very next line already honours the new sigil...
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "/set temperature 0.5",
            ))
            .await
            .unwrap();
            assert_eq!(ctx.app.config.temperature, Some(0.5));

            // ...and the old one is chat now, so the flag does not move.
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                ".set temperature 0.1",
            ))
            .await
            .expect("a dot-led line is chat under slash, never an unknown command");
            assert_eq!(ctx.app.config.temperature, Some(0.5));

            // Switching back is reachable in the new sigil and restores the
            // original detection.
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "/set command_prefix dot",
            ))
            .await
            .unwrap();
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal,
                ".set temperature 0.2",
            ))
            .await
            .unwrap();
            assert_eq!(ctx.app.config.temperature, Some(0.2));
        });
    }

    #[test]
    fn usage_probe_both_accepts_either_sigil_and_leading_whitespace() {
        run_async(async {
            let mut ctx = chat_capable_ctx(CommandPrefix::Both);
            let abort_signal = create_abort_signal();

            for (line, expected) in [
                ("/set temperature 0.3", Some(0.3)),
                ("  .set temperature 0.4", Some(0.4)),
                ("\t/set temperature 0.6", Some(0.6)),
            ] {
                Box::pin(run_repl_command(&mut ctx, abort_signal.clone(), line))
                    .await
                    .unwrap_or_else(|err| panic!("{line:?}: {err}"));
                assert_eq!(ctx.app.config.temperature, expected, "{line:?}");
            }

            // A bare path-looking line is still chat when its first non-space
            // char is not a sigil at all.
            Box::pin(run_repl_command(&mut ctx, abort_signal, "usr/bin/env"))
                .await
                .expect("a sigil-less line is chat");
            assert_eq!(ctx.app.config.temperature, Some(0.6));
        });
    }

    #[test]
    fn usage_probe_set_command_prefix_refuses_bogus_values_and_the_marker_form_follows_the_sigil() {
        run_async(async {
            let mut ctx = chat_capable_ctx(CommandPrefix::Dot);
            let abort_signal = create_abort_signal();

            // A bogus value is refused naming the three accepted values and
            // the live prefix does not move, so the next dot-led line still
            // runs as a command.
            let err = Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                ".set command_prefix bogus",
            ))
            .await
            .expect_err("a bogus command_prefix value must be refused");
            let text = err.to_string();
            assert!(
                text.contains("dot") && text.contains("slash") && text.contains("both"),
                "{text}"
            );
            assert_eq!(ctx.command_prefix(), CommandPrefix::Dot);

            // The config-file aliases are honoured at the prompt as well.
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                ".set command_prefix either",
            ))
            .await
            .unwrap();
            assert_eq!(ctx.command_prefix(), CommandPrefix::Both);
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "/set command_prefix /",
            ))
            .await
            .unwrap();
            assert_eq!(ctx.command_prefix(), CommandPrefix::Slash);

            // The `.info` row reports the value that is live now, not the
            // one the session started with.
            let info = ctx.sysinfo(&ctx.app.config).unwrap();
            let row = info
                .lines()
                .find(|line| line.trim_start().starts_with("command_prefix"))
                .unwrap_or_else(|| panic!("no command_prefix row in:\n{info}"));
            assert!(row.trim_end().ends_with("slash"), "{row}");

            // The multi-line marker form is parsed with the configured sigil:
            // a slash command inside the markers runs...
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "::: /set temperature 0.3 :::",
            ))
            .await
            .unwrap();
            assert_eq!(ctx.app.config.temperature, Some(0.3));

            // ...and the retired dot form inside the markers is chat, so the
            // flag does not move and nothing is reported as unknown.
            Box::pin(run_repl_command(
                &mut ctx,
                abort_signal.clone(),
                "::: .set temperature 0.8 :::",
            ))
            .await
            .expect("a dot-led line is chat under slash, even inside ::: markers");
            assert_eq!(ctx.app.config.temperature, Some(0.3));

            // A slash-led line that is not a known command is still an
            // unknown command under slash, and the hint names `/help`.
            let err = Box::pin(run_repl_command(
                &mut ctx,
                abort_signal,
                "/definitely-not-a-command",
            ))
            .await
            .expect_err("an unknown slash command must be refused");
            let text = err.to_string();
            assert!(
                text.contains(r#""/help""#) && !text.contains(".help"),
                "{text}"
            );
        });
    }

    #[test]
    fn test_split_args_text() {
        assert_eq!(split_args_text("", false), (vec![], ""));
        assert_eq!(
            split_args_text("file.txt", false),
            (vec!["file.txt".into()], "")
        );
        assert_eq!(
            split_args_text("file.txt --", false),
            (vec!["file.txt".into()], "")
        );
        assert_eq!(
            split_args_text("file.txt -- hello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text("file.txt -- \thello", false),
            (vec!["file.txt".into()], "\thello")
        );
        assert_eq!(
            split_args_text("file.txt --\nhello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text("file.txt --\r\nhello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text("file.txt --\rhello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text(r#"file1.txt 'file2.txt' "file3.txt""#, false),
            (
                vec!["file1.txt".into(), "file2.txt".into(), "file3.txt".into()],
                ""
            )
        );
        assert_eq!(
            split_args_text(r#"./file1.txt 'file1 - Copy.txt' file\ 2.txt"#, false),
            (
                vec![
                    "./file1.txt".into(),
                    "file1 - Copy.txt".into(),
                    "file 2.txt".into()
                ],
                ""
            )
        );
        assert_eq!(
            split_args_text(r#".\file.txt C:\dir\file.txt"#, true),
            (vec![".\\file.txt".into(), "C:\\dir\\file.txt".into()], "")
        );
    }

    #[test]
    fn repl_commands_has_66_entries() {
        assert_eq!(REPL_COMMANDS.len(), 66);
    }

    #[test]
    fn run_command_is_listed_in_help_and_builtin_names() {
        let help = repl_help_text(CommandPrefix::Dot);
        assert!(help.lines().any(|line| line.starts_with(".run ")), "{help}");
        assert!(builtin_command_names().contains(&"run"));
    }

    #[test]
    fn redraw_without_history_fails_and_rejects_unknown_args() {
        run_async(async {
            let mut ctx = ctx_with_hooks(&[], "redraw");
            let abort_signal = create_abort_signal();

            let result =
                Box::pin(run_repl_command(&mut ctx, abort_signal.clone(), ".redraw")).await;
            let err = result.expect_err("nothing to redraw without a session or last message");
            assert!(err.to_string().contains("Nothing to redraw"), "{err}");

            let result = Box::pin(run_repl_command(&mut ctx, abort_signal, ".redraw bogus")).await;
            let err = result.expect_err("unknown sub-command must be rejected");
            assert!(err.to_string().contains("Unknown command"), "{err}");
        });
    }

    #[test]
    fn redraw_reprints_the_last_message_without_a_session() {
        run_async(async {
            let mut ctx = ctx_with_hooks(&[], "redraw");
            ctx.macro_flag = true;
            let input = Input::from_str(&ctx, "what time is it", None).unwrap();
            ctx.last_message = Some(LastMessage::new(input, "noon".to_string()));
            let abort_signal = create_abort_signal();

            let result = Box::pin(run_repl_command(&mut ctx, abort_signal, ".redraw")).await;

            assert!(result.is_ok(), "{result:?}");
        });
    }

    #[test]
    fn redraw_session_without_a_session_fails() {
        run_async(async {
            let mut ctx = ctx_with_hooks(&[], "redraw");
            let abort_signal = create_abort_signal();

            let result =
                Box::pin(run_repl_command(&mut ctx, abort_signal, ".redraw session")).await;

            let err = result.expect_err("no session to replay");
            assert!(err.to_string().contains(".redraw"), "{err}");
        });
    }

    #[test]
    fn redraw_session_with_an_empty_session_succeeds() {
        run_async(async {
            let mut ctx = ctx_with_hooks(&[], "redraw");
            ctx.session = Some(Session::default());
            let abort_signal = create_abort_signal();

            let result =
                Box::pin(run_repl_command(&mut ctx, abort_signal, ".redraw session")).await;

            assert!(result.is_ok(), "{result:?}");
        });
    }

    #[test]
    fn redraw_session_replays_a_populated_session() {
        run_async(async {
            let mut ctx = ctx_with_hooks(&[], "redraw");
            let input = Input::from_str(&ctx, "what time is it", None).unwrap();
            let mut session = Session::default();
            session.add_message(&input, "noon").unwrap();
            ctx.session = Some(session);
            let abort_signal = create_abort_signal();

            let result =
                Box::pin(run_repl_command(&mut ctx, abort_signal, ".redraw session")).await;

            assert!(result.is_ok(), "{result:?}");
        });
    }

    #[test]
    fn repl_help_mentions_ctrl_l_and_redraw_together() {
        let help = repl_help_text(CommandPrefix::Dot);

        let lines: Vec<&str> = help
            .lines()
            .filter(|line| line.contains("Ctrl+L") && line.contains("\".redraw\""))
            .collect();
        assert_eq!(lines.len(), 1, "{help}");
    }

    #[test]
    fn parse_prompt_call_args_splits_on_first_equals_and_unquotes() {
        let words = vec![
            "path=notes.txt".to_string(),
            r#"style="a b""#.to_string(),
            "expr=a=b".to_string(),
        ];

        let args = parse_prompt_call_args(&words).unwrap();

        assert_eq!(args["path"], "notes.txt");
        assert_eq!(args["style"], "a b");
        assert_eq!(args["expr"], "a=b");
    }

    #[test]
    fn parse_prompt_call_args_rejects_words_without_equals() {
        let err = parse_prompt_call_args(&["positional".to_string()])
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "Invalid prompt argument 'positional': arguments must be key=value pairs"
        );
    }

    #[test]
    fn prompt_arg_inquire_label_sanitizes_all_components() {
        assert_eq!(
            prompt_arg_inquire_label("srv", "summarize", "path"),
            "Prompt 'summarize' on 'srv' requires 'path':"
        );
        assert_eq!(
            prompt_arg_inquire_label("s\u{1b}[31mrv", "sum\u{1b}]0;x\u{7}marize", "pa\u{7}th"),
            "Prompt 'summarize' on 'srv' requires 'pa th':"
        );
    }

    #[test]
    fn parse_repl_install_routes_categories_to_builtins() {
        assert_eq!(
            parse_repl_install(Some("agents")),
            ReplInstallDispatch::Builtins(AssetCategory::Agents)
        );
    }

    #[test]
    fn parse_repl_install_routes_other_values_to_unified_dispatch() {
        assert_eq!(
            parse_repl_install(Some("https://github.com/x/y")),
            ReplInstallDispatch::Unified("https://github.com/x/y")
        );
        assert_eq!(
            parse_repl_install(Some("my-bundle")),
            ReplInstallDispatch::Unified("my-bundle")
        );
    }

    #[test]
    fn parse_repl_install_empty_args_ask_for_usage() {
        assert_eq!(parse_repl_install(None), ReplInstallDispatch::Usage);
        assert_eq!(parse_repl_install(Some("  ")), ReplInstallDispatch::Usage);
    }

    #[test]
    fn parse_repl_install_rejects_extra_tokens_after_a_category() {
        assert_eq!(
            parse_repl_install(Some("agents --force")),
            ReplInstallDispatch::Usage
        );
        assert_eq!(
            parse_repl_install(Some("agents extra")),
            ReplInstallDispatch::Usage
        );
    }

    #[test]
    fn parse_repl_install_routes_help_from_any_position() {
        assert_eq!(
            parse_repl_install(Some("--help")),
            ReplInstallDispatch::Help
        );
        assert_eq!(parse_repl_install(Some("-h")), ReplInstallDispatch::Help);
        assert_eq!(
            parse_repl_install(Some("agents --help")),
            ReplInstallDispatch::Help
        );
        assert_eq!(
            parse_repl_install(Some("owner/repo --help")),
            ReplInstallDispatch::Help
        );
    }

    #[test]
    fn parse_repl_uninstall_routes_run_help_and_usage() {
        assert_eq!(
            parse_repl_uninstall(Some("my-bundle --yes")),
            ReplUninstallDispatch::Run("my-bundle".to_string(), true)
        );
        assert_eq!(
            parse_repl_uninstall(Some("my-bundle")),
            ReplUninstallDispatch::Run("my-bundle".to_string(), false)
        );
        assert_eq!(
            parse_repl_uninstall(Some("--help")),
            ReplUninstallDispatch::Help
        );
        assert_eq!(
            parse_repl_uninstall(Some("my-bundle -h")),
            ReplUninstallDispatch::Help
        );
        assert_eq!(
            parse_repl_uninstall(Some("--force my-bundle")),
            ReplUninstallDispatch::Usage
        );
        assert_eq!(parse_repl_uninstall(None), ReplUninstallDispatch::Usage);
    }

    #[test]
    fn parse_repl_save_session_routes_name_and_scope_in_either_order() {
        assert_eq!(
            parse_repl_save_session(None),
            ReplSaveSessionDispatch::Run {
                name: None,
                scope: None
            }
        );
        assert_eq!(
            parse_repl_save_session(Some("notes")),
            ReplSaveSessionDispatch::Run {
                name: Some("notes".to_string()),
                scope: None
            }
        );
        assert_eq!(
            parse_repl_save_session(Some("notes --workspace")),
            ReplSaveSessionDispatch::Run {
                name: Some("notes".to_string()),
                scope: Some(SessionScope::Workspace)
            }
        );
        assert_eq!(
            parse_repl_save_session(Some("--global notes")),
            ReplSaveSessionDispatch::Run {
                name: Some("notes".to_string()),
                scope: Some(SessionScope::Global)
            }
        );
        assert_eq!(
            parse_repl_save_session(Some("--workspace")),
            ReplSaveSessionDispatch::Run {
                name: None,
                scope: Some(SessionScope::Workspace)
            }
        );
    }

    #[test]
    fn parse_repl_save_session_rejects_conflicts_and_unknown_flags_but_joins_spaced_names() {
        assert_eq!(
            parse_repl_save_session(Some("--workspace --global")),
            ReplSaveSessionDispatch::Conflict
        );
        assert_eq!(
            parse_repl_save_session(Some("notes --force")),
            ReplSaveSessionDispatch::Usage
        );
        assert_eq!(
            parse_repl_save_session(Some("one two")),
            ReplSaveSessionDispatch::Run {
                name: Some("one two".to_string()),
                scope: None
            }
        );
        assert_eq!(
            parse_repl_save_session(Some("--help")),
            ReplSaveSessionDispatch::Help
        );
        assert_eq!(
            parse_repl_save_session(Some("notes -h")),
            ReplSaveSessionDispatch::Help
        );
        assert_eq!(
            parse_repl_save_session(Some("--workspace --global -h")),
            ReplSaveSessionDispatch::Help
        );
        assert_eq!(
            parse_repl_save_session(Some("--global notes --global")),
            ReplSaveSessionDispatch::Run {
                name: Some("notes".to_string()),
                scope: Some(SessionScope::Global)
            }
        );
    }

    #[test]
    fn repl_save_session_help_text_covers_the_full_surface() {
        let help = repl_save_session_help();
        for needle in [
            "--workspace",
            "--global",
            "[name]",
            "never deletes the old file",
        ] {
            assert!(help.contains(needle), "save session help missing {needle}");
        }
    }

    // Usage probe (spec-first, criterion 5): `-h/--help` wins from ANY
    // position — including when it is preceded by a token that would
    // otherwise route to Usage (unknown `-` flag) or Conflict (both scope
    // flags) — while, absent help, an unknown `-` token still routes to
    // Usage even when a valid scope flag is present, and a repeated
    // identical flag stays idempotent for `--workspace` too.
    #[test]
    fn usage_probe_save_session_help_prescan_beats_usage_and_conflict() {
        for input in [
            "--bogus -h",
            "-h --bogus",
            "notes --bogus --help",
            "--workspace --global notes -h",
            "--help --workspace --global",
            "one -h two",
        ] {
            assert_eq!(
                parse_repl_save_session(Some(input)),
                ReplSaveSessionDispatch::Help,
                "help must win for {input:?}"
            );
        }

        assert_eq!(
            parse_repl_save_session(Some("--workspace --bogus notes")),
            ReplSaveSessionDispatch::Usage
        );
        assert_eq!(
            parse_repl_save_session(Some("-x")),
            ReplSaveSessionDispatch::Usage
        );
        assert_eq!(
            parse_repl_save_session(Some("--workspace notes --workspace")),
            ReplSaveSessionDispatch::Run {
                name: Some("notes".to_string()),
                scope: Some(SessionScope::Workspace)
            }
        );
        // Non-flag tokens join into ONE name even when a flag sits between them.
        assert_eq!(
            parse_repl_save_session(Some("one --global two")),
            ReplSaveSessionDispatch::Run {
                name: Some("one two".to_string()),
                scope: Some(SessionScope::Global)
            }
        );
    }

    #[test]
    fn repl_install_and_uninstall_help_text_cover_the_full_surface() {
        let install = repl_install_help();
        for needle in [
            "--git-host",
            "--filter",
            "--force",
            "#ref",
            "owner/repo",
            ".list bundles",
        ] {
            assert!(install.contains(needle), "install help missing {needle}");
        }
        let uninstall = repl_uninstall_help();
        for needle in ["--yes", ".list bundles", "<bundle-name>"] {
            assert!(
                uninstall.contains(needle),
                "uninstall help missing {needle}"
            );
        }
    }

    #[test]
    fn builtin_command_names_are_sorted_deduped_first_words_without_dots() {
        let names = builtin_command_names();
        assert!(!names.is_empty());
        for name in &names {
            assert!(!name.starts_with('.'), "'{name}' should not keep the dot");
            assert!(!name.contains(' '), "'{name}' should be a single word");
        }
        assert!(
            names.windows(2).all(|w| w[0] < w[1]),
            "names should be sorted and deduplicated: {names:?}"
        );
        assert!(names.contains(&"help"));
        assert!(names.contains(&"macro"));
    }

    #[test]
    fn repl_commands_all_start_with_dot() {
        for cmd in REPL_COMMANDS.iter() {
            assert!(
                cmd.name.starts_with('.'),
                "Command '{}' should start with '.'",
                cmd.name
            );
        }
    }

    #[test]
    fn repl_commands_no_empty_descriptions() {
        for cmd in REPL_COMMANDS.iter() {
            assert!(
                !cmd.description.is_empty(),
                "Command '{}' has empty description",
                cmd.name
            );
        }
    }

    #[test]
    fn repl_commands_help_is_always_available() {
        let help = REPL_COMMANDS.iter().find(|c| c.name == ".help").unwrap();
        assert!(help.is_valid(StateFlags::empty()));
        assert!(help.is_valid(StateFlags::ROLE));
        assert!(help.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn repl_commands_compress_session_describes_keep_last_as_upper_bound() {
        let compress = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".compress session")
            .unwrap();
        assert!(
            compress
                .description
                .contains("upper bound on trailing messages to keep")
        );
    }

    #[test]
    fn repl_commands_exit_is_always_available() {
        let exit = REPL_COMMANDS.iter().find(|c| c.name == ".exit").unwrap();
        assert!(exit.is_valid(StateFlags::empty()));
        assert!(exit.is_valid(StateFlags::all()));
    }

    #[test]
    fn repl_commands_info_role_requires_role() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".info role")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::ROLE));
        assert!(!cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::SESSION_EMPTY));
    }

    #[test]
    fn repl_commands_session_blocked_when_already_in_session() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".session").unwrap();
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::SESSION));
        assert!(!cmd.is_valid(StateFlags::SESSION_EMPTY));
    }

    #[test]
    fn repl_commands_exit_session_requires_session() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".exit session")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::SESSION));
        assert!(cmd.is_valid(StateFlags::SESSION_EMPTY));
        assert!(!cmd.is_valid(StateFlags::empty()));
    }

    #[test]
    fn repl_commands_exit_agent_requires_agent() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".exit agent")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::AGENT));
        assert!(!cmd.is_valid(StateFlags::empty()));
    }

    #[test]
    fn repl_commands_agent_only_when_bare() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".agent").unwrap();
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::ROLE));
        assert!(!cmd.is_valid(StateFlags::SESSION));
        assert!(!cmd.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn repl_commands_role_blocked_in_session_or_agent() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".role").unwrap();
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::SESSION));
        assert!(!cmd.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn repl_commands_prompt_always_available() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".prompt").unwrap();
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(cmd.is_valid(StateFlags::ROLE));
        assert!(cmd.is_valid(StateFlags::SESSION));
        assert!(cmd.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn repl_commands_temp_role_blocked_in_session_or_agent() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".temp-role")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(cmd.is_valid(StateFlags::ROLE));
        assert!(!cmd.is_valid(StateFlags::SESSION));
        assert!(!cmd.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn repl_commands_rag_blocked_in_agent() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".rag").unwrap();
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(cmd.is_valid(StateFlags::ROLE));
        assert!(!cmd.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn repl_commands_starter_requires_agent() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".starter").unwrap();
        assert!(cmd.is_valid(StateFlags::AGENT));
        assert!(!cmd.is_valid(StateFlags::empty()));
    }

    #[test]
    fn repl_commands_clear_todo_always_available() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".clear todo")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::AGENT));
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(cmd.is_valid(StateFlags::SESSION));
        assert!(cmd.is_valid(StateFlags::ROLE));
    }

    #[test]
    fn repl_commands_edit_role_requires_role_not_session() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".edit role")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::ROLE));
        assert!(!cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::ROLE | StateFlags::SESSION));
    }

    #[test]
    fn repl_commands_exit_rag_requires_rag_not_agent() {
        let cmd = REPL_COMMANDS
            .iter()
            .find(|c| c.name == ".exit rag")
            .unwrap();
        assert!(cmd.is_valid(StateFlags::RAG));
        assert!(!cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::RAG | StateFlags::AGENT));
    }

    #[test]
    fn parse_command_plain_text_returns_none() {
        assert!(parse_command("hello world", CommandPrefix::Dot).is_none());
    }

    #[test]
    fn parse_command_empty_returns_none() {
        assert!(parse_command("", CommandPrefix::Dot).is_none());
    }

    #[test]
    fn parse_command_whitespace_only_returns_none() {
        assert!(parse_command("   ", CommandPrefix::Dot).is_none());
    }

    #[test]
    fn parse_command_dot_only() {
        assert_eq!(
            parse_command(".", CommandPrefix::Dot),
            Some((Cow::Borrowed("."), None))
        );
    }

    #[test]
    fn try_extract_shell_command_strips_bang() {
        assert_eq!(try_extract_shell_command("!ls"), Some("ls"));
        assert_eq!(try_extract_shell_command("!ls -la"), Some("ls -la"));
    }

    #[test]
    fn try_extract_shell_command_trims_inner_whitespace() {
        assert_eq!(try_extract_shell_command("!   echo hi"), Some("echo hi"));
        assert_eq!(try_extract_shell_command("! ls"), Some("ls"));
    }

    #[test]
    fn try_extract_shell_command_only_bang_yields_empty() {
        assert_eq!(try_extract_shell_command("!"), Some(""));
        assert_eq!(try_extract_shell_command("!   "), Some(""));
    }

    #[test]
    fn try_extract_shell_command_rejects_leading_whitespace() {
        assert!(try_extract_shell_command(" !ls").is_none());
        assert!(try_extract_shell_command("\t!ls").is_none());
    }

    #[test]
    fn try_extract_shell_command_rejects_inline_bang() {
        assert!(try_extract_shell_command("echo !foo").is_none());
        assert!(try_extract_shell_command("hello world").is_none());
    }

    #[test]
    fn try_extract_shell_command_strips_one_leading_bang() {
        assert_eq!(try_extract_shell_command("!!ls"), Some("!ls"));
    }

    #[test]
    fn try_extract_shell_command_preserves_pipes_and_redirects() {
        assert_eq!(
            try_extract_shell_command("!ls -la | grep yaml"),
            Some("ls -la | grep yaml")
        );
        assert_eq!(
            try_extract_shell_command("!cat foo.txt > /tmp/out"),
            Some("cat foo.txt > /tmp/out")
        );
        assert_eq!(
            try_extract_shell_command(r#"!echo "$HOME""#),
            Some(r#"echo "$HOME""#)
        );
    }

    #[test]
    fn split_first_arg_none_input() {
        assert!(split_first_arg(None).is_none());
    }

    #[test]
    fn split_first_arg_single_word() {
        assert_eq!(split_first_arg(Some("role")), Some(("role", None)));
    }

    #[test]
    fn split_first_arg_two_words() {
        assert_eq!(
            split_first_arg(Some("role test-role")),
            Some(("role", Some("test-role")))
        );
    }

    #[test]
    fn split_first_arg_with_extra_spaces() {
        assert_eq!(
            split_first_arg(Some("session  my-session")),
            Some(("session", Some("my-session")))
        );
    }

    #[test]
    fn repl_command_is_valid_pass_always_true() {
        let cmd = ReplCommand::new(".test", "desc", AssertState::pass());
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(cmd.is_valid(StateFlags::all()));
    }

    #[test]
    fn repl_command_is_valid_respects_true() {
        let cmd = ReplCommand::new(".test", "desc", AssertState::True(StateFlags::ROLE));
        assert!(cmd.is_valid(StateFlags::ROLE));
        assert!(!cmd.is_valid(StateFlags::empty()));
    }

    #[test]
    fn repl_command_is_valid_respects_false() {
        let cmd = ReplCommand::new(".test", "desc", AssertState::False(StateFlags::AGENT));
        assert!(cmd.is_valid(StateFlags::empty()));
        assert!(!cmd.is_valid(StateFlags::AGENT));
    }

    #[test]
    fn multiline_regex_captures_content_between_markers() {
        let input = ":::\nhello world\n:::";
        let captures = MULTILINE_RE.captures(input).unwrap().unwrap();
        let content = captures.get(1).unwrap().as_str();
        assert_eq!(content.trim(), "hello world");
    }

    #[test]
    fn multiline_regex_does_not_match_single_marker() {
        let input = ":::\nhello world";
        let result = MULTILINE_RE.captures(input).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn multiline_regex_does_not_match_plain_text() {
        let input = "hello world";
        let result = MULTILINE_RE.captures(input).unwrap();
        assert!(result.is_none());
    }

    fn banner_ctx() -> RequestContext {
        use crate::config::{AppState, WorkingMode};
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    #[test]
    fn pause_banner_text_paused_with_incomplete_todos() {
        let mut ctx = banner_ctx();
        ctx.add_todo("write code");
        ctx.add_todo("run tests");
        ctx.pause_auto_continue("Which database?");

        let text = pause_banner_text(&ctx).unwrap();
        assert!(text.contains("Which database?"), "{text}");
        assert!(text.contains("2 incomplete todo(s) preserved"), "{text}");
    }

    #[test]
    fn pause_banner_text_none_when_all_todos_complete() {
        let mut ctx = banner_ctx();
        let id = ctx.add_todo("write code");
        ctx.mark_todo_done(id);
        ctx.pause_auto_continue("Which database?");

        assert_eq!(pause_banner_text(&ctx), None);
    }

    #[test]
    fn pause_banner_text_none_when_unpaused() {
        let mut ctx = banner_ctx();
        ctx.add_todo("write code");

        assert_eq!(pause_banner_text(&ctx), None);
    }

    #[test]
    fn latch_prompt_interrupt_stays_clear_at_idle_prompt() {
        let abort = create_abort_signal();
        let supervisor = RwLock::new(Supervisor::new(4, 3));

        let cancelled = latch_prompt_interrupt(&abort, &supervisor);

        assert!(!cancelled, "nothing was running, so nothing was cancelled");
        assert!(
            !abort.aborted_ctrlc(),
            "ctrl-c with nothing to cancel must not latch as an interruption"
        );
    }

    #[test]
    fn latch_prompt_interrupt_latches_when_cancelling_active_children() {
        use crate::supervisor::mailbox::Inbox;
        use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};

        // Keep the runtime alive so the spawned task is never polled and the
        // child counts as running.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(AgentResult {
                id: "done".into(),
                agent_name: "test".into(),
                output: "result".into(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        let child_signal = create_abort_signal();
        let mut sup = Supervisor::new(4, 3);
        sup.register(AgentHandle {
            id: "a1".to_string(),
            agent_name: "explore".to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: child_signal.clone(),
            join_handle,
            child_supervisor: None,
        })
        .unwrap();
        let supervisor = RwLock::new(sup);
        let abort = create_abort_signal();

        let cancelled = latch_prompt_interrupt(&abort, &supervisor);

        assert!(cancelled, "a live child was cancelled");
        assert!(
            abort.aborted_ctrlc(),
            "cancelling live work is an interruption"
        );
        assert!(
            child_signal.aborted_ctrlc(),
            "the child must still be cancelled"
        );
    }

    /// Polls `done` until it holds, failing if it never does.
    async fn wait_until(done: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the bridge task must settle promptly");
    }

    /// Only the signal half is testable here: SIGINT is process-global, so
    /// the ctrl-c arm cannot be exercised hermetically.
    #[tokio::test]
    async fn turn_interrupt_bridge_retires_once_signal_latched() {
        let abort = create_abort_signal();
        let guard = spawn_turn_interrupt_bridge(&abort);
        tokio::task::yield_now().await;
        assert!(!guard.is_finished(), "the bridge waits while the turn runs");

        abort.set_ctrlc();

        wait_until(|| guard.is_finished()).await;
    }

    /// The spawned task holds the only other clone of the signal, so the
    /// strong count falling back to one shows the aborted future was dropped.
    #[tokio::test]
    async fn turn_interrupt_bridge_aborts_pending_task_on_drop() {
        let abort = create_abort_signal();
        let guard = spawn_turn_interrupt_bridge(&abort);
        tokio::task::yield_now().await;
        assert_eq!(Arc::strong_count(&abort), 2);

        drop(guard);

        wait_until(|| Arc::strong_count(&abort) == 1).await;
        assert!(
            !abort.aborted_ctrlc(),
            "dropping the bridge is not an interrupt"
        );
    }

    /// Registers a child whose task sleeps `for_secs`; returns its abort
    /// signal. With zero seconds the child is already finished on return.
    async fn register_agent(ctx: &mut RequestContext, id: &str, for_secs: u64) -> AbortSignal {
        use crate::supervisor::mailbox::Inbox;
        use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};

        let abort = create_abort_signal();
        let id_owned = id.to_string();
        let join_handle = tokio::spawn(async move {
            if for_secs > 0 {
                tokio::time::sleep(Duration::from_secs(for_secs)).await;
            }
            Ok(AgentResult {
                id: id_owned,
                agent_name: "explore".to_string(),
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        if for_secs == 0 {
            while !join_handle.is_finished() {
                tokio::task::yield_now().await;
            }
        }
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(AgentHandle {
                id: id.to_string(),
                agent_name: "explore".to_string(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: abort.clone(),
                join_handle,
                child_supervisor: None,
            })
            .unwrap();
        abort
    }

    async fn register_running_agent(ctx: &mut RequestContext, id: &str) -> AbortSignal {
        register_agent(ctx, id, 60).await
    }

    fn register_running_job(ctx: &mut RequestContext, id: &str) {
        use crate::function::jobs::RingBuf;
        use crate::supervisor::{JobHandle, JobResult, JobState, JobStatus};

        let join_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(JobResult {
                output: json!(null),
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(JobHandle {
                id: id.to_string(),
                tool: "execute_command".to_string(),
                started_at: std::time::Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::new(parking_lot::Mutex::new(JobState {
                    status: JobStatus::Running,
                    pgid: None,
                })),
                output_buf: Arc::new(parking_lot::Mutex::new(RingBuf::default())),
                no_change_checks: 0,
                last_check_state: None,
            })
            .unwrap();
    }

    /// A session-bearing REPL ctx with a supervisor that allows jobs, so
    /// both task kinds can be registered on it.
    fn interruptible_ctx(marker: &str) -> RequestContext {
        let mut ctx = ctx_with_hooks(&["turn.interrupted", "agent.interrupted"], marker);
        ctx.session = Some(Session::default());
        ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(4),
        )));
        ctx
    }

    /// Latches Ctrl-C on the turn's abort signal during the first completion
    /// and answers it with `first`; every later call answers with no text
    /// and no tool results.
    fn ctrl_c_on_first_call_runner(
        first: (&'static str, Vec<ToolResult>),
        seen: SeenInputs,
    ) -> impl for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a> + Send
    {
        let mut first = Some(first);
        completion_runner(move |input, _ctx, abort| {
            seen.lock().push(input.clone());
            let reply = match first.take() {
                Some((text, results)) => {
                    abort.set_ctrlc();
                    Ok((text.to_string(), results))
                }
                None => Ok((String::new(), vec![])),
            };
            boxed_completion(async move { reply })
        })
    }

    /// Answers every completion with no text and no tool results, never
    /// touching the abort signal: a turn that finishes normally.
    fn scripted_plain_runner()
    -> impl for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a> + Send
    {
        completion_runner(move |_input, _ctx, _abort| {
            boxed_completion(async move { Ok((String::new(), vec![])) })
        })
    }

    fn hook_count(marker: &str, event: &str) -> usize {
        test_sink::snapshot()
            .iter()
            .filter(|capture| capture.hook_name == format!("{marker}-{event}"))
            .count()
    }

    #[test]
    #[serial]
    fn ctrl_c_during_a_tool_round_ends_the_turn_without_guardrail_phantoms() {
        let _sink = test_sink::install();
        let marker = "soft-ctrlc-tool-round-k4m";
        run_async(async {
            let mut ctx = interruptible_ctx(marker);
            let child_abort = register_running_agent(&mut ctx, "a1").await;
            let round = vec![ToolResult::new(
                ToolCall::new(
                    "agent__collect".into(),
                    json!({"id": "a1"}),
                    Some("id-1".into()),
                ),
                json!({"status": "interrupted", "id": "a1"}),
            )];
            let seen: SeenInputs = Default::default();
            let mut runner = ctrl_c_on_first_call_runner(("", round), Arc::clone(&seen));
            let abort_signal = create_abort_signal();
            let input = Input::from_str(&ctx, "collect a1", None).unwrap();

            ask_with_branch_with_runner(&mut ctx, abort_signal, input, false, None, &mut runner)
                .await
                .unwrap();

            assert_eq!(
                seen.lock().len(),
                1,
                "the tool loop must stop after the interrupted round"
            );
            let exchange = exchange(&ctx);
            let roles: Vec<MessageRole> = exchange.iter().map(|(role, _)| *role).collect();
            assert_eq!(
                roles,
                [MessageRole::User, MessageRole::Tool, MessageRole::Assistant]
            );
            assert_eq!(exchange[0].1, "collect a1");
            assert_eq!(exchange[2].1, TURN_INTERRUPTED_TEXT);
            assert!(
                exchange
                    .iter()
                    .all(|(_, text)| !text.contains("[SYSTEM GUARDRAIL]")),
                "the guardrail must not inject reminders into an interrupted turn"
            );
            assert!(
                !child_abort.aborted_ctrlc(),
                "the child must be left running"
            );
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                Some(false)
            );
            assert!(
                ctx.last_message
                    .as_ref()
                    .is_some_and(|v| v.continuous && v.output.is_empty()),
                "the marker is bookkeeping, not a reply to continue"
            );
            assert!(
                ctx.has_recoverable_interruption(),
                ".recover stays available after a Ctrl-C"
            );
        });
        assert_eq!(hook_count(marker, "turn.interrupted"), 1);
        // Shape pin: the fixture child is a bare sleep task, so no agent
        // loop could have fired agent.interrupted in the first place.
        assert_eq!(hook_count(marker, "agent.interrupted"), 0);
    }

    #[test]
    #[serial]
    fn ctrl_c_during_the_stream_leaves_background_tasks_alive() {
        let _sink = test_sink::install();
        let marker = "soft-ctrlc-stream-p9z";
        run_async(async {
            let mut ctx = interruptible_ctx(marker);
            let child_abort = register_running_agent(&mut ctx, "a1").await;
            let seen: SeenInputs = Default::default();
            let mut runner = ctrl_c_on_first_call_runner(("partial", vec![]), Arc::clone(&seen));
            let abort_signal = create_abort_signal();
            let input = Input::from_str(&ctx, "tell me", None).unwrap();

            ask_with_branch_with_runner(&mut ctx, abort_signal, input, false, None, &mut runner)
                .await
                .unwrap();

            assert_eq!(
                seen.lock().len(),
                1,
                "no guardrail prompt may follow the interrupt"
            );
            assert_eq!(
                exchange(&ctx),
                [
                    (MessageRole::User, "tell me".to_string()),
                    (MessageRole::Assistant, "partial".to_string())
                ]
            );
            assert!(
                !child_abort.aborted_ctrlc(),
                "the child must be left running"
            );
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                Some(false)
            );
        });
        assert_eq!(hook_count(marker, "turn.interrupted"), 1);
    }

    #[test]
    fn interrupted_turn_notice_is_silent_without_unfinished_tasks() {
        let ctx = banner_ctx();
        assert_eq!(interrupted_turn_notice(&ctx), None);

        run_async(async {
            let mut ctx = interruptible_ctx("notice-none");
            register_agent(&mut ctx, "a1", 0).await;
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                Some(true)
            );
            assert_eq!(
                interrupted_turn_notice(&ctx),
                None,
                "a finished task is not running"
            );
        });
    }

    #[test]
    fn interrupted_turn_notice_lists_running_agents_and_jobs() {
        run_async(async {
            let mut ctx = interruptible_ctx("notice-list");
            // Registered out of id order so the sorted listing is a claim.
            register_running_job(&mut ctx, "j2");
            register_running_agent(&mut ctx, "a1").await;

            assert_eq!(
                interrupted_turn_notice(&ctx).as_deref(),
                Some(
                    "Turn interrupted; 2 background task(s) still running: a1 (agent), j2 (job). Press Ctrl-C again to cancel them, or type to continue."
                )
            );
        });
    }

    /// Live work one supervisor level down has no top-level id to list, but
    /// `latch_prompt_interrupt` would cancel it on the next Ctrl-C, so the
    /// notice must still say so.
    #[test]
    fn interrupted_turn_notice_names_grandchild_only_activity() {
        use crate::supervisor::mailbox::Inbox;
        use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};

        run_async(async {
            let ctx = interruptible_ctx("notice-grandchild");
            let grandchild = tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(AgentResult {
                    id: "g1".to_string(),
                    agent_name: "worker".to_string(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                })
            });
            let mut child_sup = Supervisor::new(4, 3);
            child_sup
                .register(AgentHandle {
                    id: "g1".to_string(),
                    agent_name: "worker".to_string(),
                    depth: 2,
                    inbox: Arc::new(Inbox::new()),
                    abort_signal: create_abort_signal(),
                    join_handle: grandchild,
                    child_supervisor: None,
                })
                .unwrap();
            // The parent's own task is already finished, so the only live
            // work sits in its child supervisor.
            let parent_task = tokio::spawn(async {
                Ok(AgentResult {
                    id: "a1".to_string(),
                    agent_name: "explore".to_string(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                })
            });
            while !parent_task.is_finished() {
                tokio::task::yield_now().await;
            }
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(AgentHandle {
                    id: "a1".to_string(),
                    agent_name: "explore".to_string(),
                    depth: 1,
                    inbox: Arc::new(Inbox::new()),
                    abort_signal: create_abort_signal(),
                    join_handle: parent_task,
                    child_supervisor: Some(Arc::new(RwLock::new(child_sup))),
                })
                .unwrap();
            assert_eq!(
                pending_tasks(&ctx).iter().filter(|t| !t.finished).count(),
                0
            );

            assert_eq!(
                interrupted_turn_notice(&ctx).as_deref(),
                Some(
                    "Turn interrupted; background work is still running. Press Ctrl-C again to cancel it, or type to continue."
                )
            );
        });
    }

    /// The guardrail's force-terminate arm must stay as dark as its inject
    /// arm on an interrupted turn: with the reminder budget already spent,
    /// a turn that ran the guardrail would `cancel_recursive` the very
    /// tasks the user chose to leave running and reset the counter.
    #[test]
    #[serial]
    fn usage_probe_ctrl_c_in_a_tool_round_skips_the_guardrail_force_terminate_arm() {
        use crate::function::agents::PENDING_TASKS_GUARDRAIL_MAX;

        let _sink = test_sink::install();
        let marker = "soft-ctrlc-force-terminate-q2w";
        run_async(async {
            let mut ctx = interruptible_ctx(marker);
            let child_abort = register_running_agent(&mut ctx, "a1").await;
            register_running_job(&mut ctx, "j1");
            ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;
            let round = vec![ToolResult::new(
                ToolCall::new(
                    "agent__collect".into(),
                    json!({"id": "a1"}),
                    Some("id-1".into()),
                ),
                json!({"status": "interrupted", "id": "a1"}),
            )];
            let seen: SeenInputs = Default::default();
            let mut runner = ctrl_c_on_first_call_runner(("", round), Arc::clone(&seen));
            let input = Input::from_str(&ctx, "collect a1", None).unwrap();

            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            assert_eq!(seen.lock().len(), 1);
            assert!(
                !child_abort.aborted(),
                "the force-terminate arm must not cancel the child"
            );
            let sup = ctx.supervisor.as_ref().unwrap().read();
            assert_eq!(sup.is_finished("a1"), Some(false));
            assert!(sup.has_job("j1"), "the job handle must stay registered");
            assert!(
                sup.list_tasks().iter().all(|(_, _, finished)| !finished),
                "both tasks must still be running"
            );
            drop(sup);
            assert_eq!(
                ctx.pending_tasks_guardrail_count, PENDING_TASKS_GUARDRAIL_MAX,
                "the guardrail did not run, so its counter is untouched"
            );
            assert_eq!(
                interrupted_turn_notice(&ctx).as_deref(),
                Some(
                    "Turn interrupted; 2 background task(s) still running: a1 (agent), j1 (job). Press Ctrl-C again to cancel them, or type to continue."
                )
            );
        });
        assert_eq!(hook_count(marker, "turn.interrupted"), 1);
        assert_eq!(hook_count(marker, "agent.interrupted"), 0);
    }

    /// An interrupted tool round is committed to the autosaving session file
    /// in exactly one write, every tool_use id keeps its tool_result, and the
    /// closing marker is the deliberate-interrupt text: the persisted session
    /// never reads as an error checkpoint, so a resumed copy does not offer
    /// `.recover`, and the live ctx offers it no more than any completed
    /// turn already does (`record_chat_completion` marks every finished
    /// exchange continuous).
    #[test]
    #[serial]
    fn usage_probe_ctrl_c_in_a_tool_round_flushes_once_and_keeps_recover_dark() {
        let _guard = crate::testing::TestConfigDirGuard::new("repl");
        let _sink = test_sink::install();
        let marker = "soft-ctrlc-flush-h7t";
        run_async(async {
            let mut ctx = interruptible_ctx(marker);
            register_running_agent(&mut ctx, "a1").await;
            let path = ctx.session_file_for("soft-ctrlc", SessionScope::Workspace);
            let session = ctx.session.as_mut().unwrap();
            session.save("soft-ctrlc", &path, false).unwrap();
            session.set_save_session(Some(true));
            let writes_before = Session::save_temp_seq();
            let round = vec![
                ToolResult::new(
                    ToolCall::new(
                        "agent__collect".into(),
                        json!({"id": "a1"}),
                        Some("id-1".into()),
                    ),
                    json!({"status": "interrupted", "id": "a1"}),
                ),
                ToolResult::new(
                    ToolCall::new("fs_ls".into(), json!({"path": "."}), Some("id-2".into())),
                    json!("src"),
                ),
            ];
            let seen: SeenInputs = Default::default();
            let mut runner = ctrl_c_on_first_call_runner(("", round), Arc::clone(&seen));
            let input = Input::from_str(&ctx, "collect a1", None).unwrap();

            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            assert_eq!(seen.lock().len(), 1);
            assert_eq!(
                Session::save_temp_seq() - writes_before,
                1,
                "the interrupted turn lands on disk in one write"
            );
            assert!(!ctx.session.as_ref().unwrap().dirty());
            let mut plain = interruptible_ctx("soft-ctrlc-flush-plain");
            let mut plain_runner = scripted_plain_runner();
            let plain_input = Input::from_str(&plain, "collect a1", None).unwrap();
            ask_with_branch_with_runner(
                &mut plain,
                create_abort_signal(),
                plain_input,
                false,
                None,
                &mut plain_runner,
            )
            .await
            .unwrap();
            assert_eq!(
                ctx.has_recoverable_interruption(),
                plain.has_recoverable_interruption(),
                "a Ctrl-C turn offers .recover exactly as much as a completed turn"
            );

            let on_disk: Session =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let exchange: Vec<&Message> = on_disk
                .messages()
                .iter()
                .skip_while(|m| m.role.is_system())
                .collect();
            assert_eq!(exchange.len(), 3, "User + Tool + Assistant(marker)");
            assert_eq!(exchange[0].role, MessageRole::User);
            let MessageContent::ToolCalls(rounds) = &exchange[1].content else {
                panic!("the second message must carry the tool rounds");
            };
            let mut ids: Vec<Option<&str>> = rounds
                .tool_results
                .iter()
                .map(|result| result.call.id.as_deref())
                .collect();
            ids.sort();
            assert_eq!(
                ids,
                [Some("id-1"), Some("id-2")],
                "every tool_use id keeps its tool_result"
            );
            assert_eq!(exchange[2].role, MessageRole::Assistant);
            assert_eq!(exchange[2].content.to_text(), TURN_INTERRUPTED_TEXT);
            assert!(
                !on_disk.has_interrupted_error_checkpoint(),
                "the persisted marker must not read as an error checkpoint"
            );
            assert!(
                !ctx.session
                    .as_ref()
                    .unwrap()
                    .has_interrupted_error_checkpoint(),
                "the live session must not read as an error checkpoint either"
            );
            let mut resumed = repl_ctx();
            resumed.session = Some(on_disk);
            assert!(
                !resumed.has_recoverable_interruption(),
                ".recover must stay dark on a session resumed after a Ctrl-C"
            );
        });
        assert_eq!(hook_count(marker, "turn.interrupted"), 1);
    }

    /// A stream interrupt with no partial text still closes the turn with an
    /// empty Assistant text, leaves a running job alone, lists it in the
    /// notice in id order regardless of registration order, and does not
    /// light `.recover`.
    #[test]
    #[serial]
    fn usage_probe_ctrl_c_during_an_empty_stream_keeps_a_job_alive_and_recover_dark() {
        let _sink = test_sink::install();
        let marker = "soft-ctrlc-empty-stream-m3v";
        run_async(async {
            let mut ctx = interruptible_ctx(marker);
            register_running_job(&mut ctx, "j9");
            let agent_abort = register_running_agent(&mut ctx, "a5").await;
            let seen: SeenInputs = Default::default();
            let mut runner = ctrl_c_on_first_call_runner(("", vec![]), Arc::clone(&seen));
            let input = Input::from_str(&ctx, "tell me", None).unwrap();

            ask_with_branch_with_runner(
                &mut ctx,
                create_abort_signal(),
                input,
                false,
                None,
                &mut runner,
            )
            .await
            .unwrap();

            assert_eq!(seen.lock().len(), 1);
            assert_eq!(
                exchange(&ctx),
                [
                    (MessageRole::User, "tell me".to_string()),
                    (MessageRole::Assistant, String::new())
                ]
            );
            assert!(
                !ctx.session
                    .as_ref()
                    .unwrap()
                    .has_interrupted_error_checkpoint(),
                "a stream interrupt must not write the error-checkpoint marker"
            );
            assert!(!agent_abort.aborted());
            let sup = ctx.supervisor.as_ref().unwrap().read();
            assert!(sup.has_job("j9"));
            assert!(
                sup.list_tasks().iter().all(|(_, _, finished)| !finished),
                "both tasks must still be running"
            );
            drop(sup);
            assert_eq!(
                interrupted_turn_notice(&ctx).as_deref(),
                Some(
                    "Turn interrupted; 2 background task(s) still running: a5 (agent), j9 (job). Press Ctrl-C again to cancel them, or type to continue."
                ),
                "ids are listed sorted, not in registration order"
            );
        });
        assert_eq!(hook_count(marker, "turn.interrupted"), 1);
        assert_eq!(hook_count(marker, "agent.interrupted"), 0);
    }
}
