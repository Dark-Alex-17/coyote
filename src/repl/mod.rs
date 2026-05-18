mod completer;
mod highlighter;
mod prompt;

use self::completer::ReplCompleter;
use self::highlighter::ReplHighlighter;
use self::prompt::ReplPrompt;

use crate::client::{call_chat_completions, call_chat_completions_streaming, init_client, oauth};
use crate::config::paths;
use crate::config::{
    AgentVariables, AppConfig, AssertState, Input, LastMessage, RequestContext, StateFlags,
    macro_execute,
};
use crate::render::render_error;
use crate::utils::{
    AbortSignal, abortable_run_with_spinner, create_abort_signal, dimmed_text, set_text, temp_file,
};

use crate::{graph, resolve_oauth_client};
use anyhow::{Context, Result, bail};
use crossterm::cursor::SetCursorStyle;
use fancy_regex::Regex;
use indoc::indoc;
use parking_lot::RwLock;
use reedline::CursorConfig;
use reedline::{
    ColumnarMenu, EditCommand, EditMode, Emacs, KeyCode, KeyModifiers, Keybindings, Reedline,
    ReedlineEvent, ReedlineMenu, ValidationResult, Validator, Vi, default_emacs_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings,
};
use reedline::{MenuBuilder, Signal};
use std::sync::LazyLock;
use std::{env, process, sync::Arc};
use log::warn;

const MENU_NAME: &str = "completion_menu";

pub const DEFAULT_CONTINUATION_PROMPT: &str = indoc! {"
    [SYSTEM REMINDER - TODO CONTINUATION]
    You have incomplete tasks. Rules:
    1. BEFORE marking a todo done: verify the work compiles/works. No premature completion.
    2. If a todo is broad (e.g. \"implement X and implement Y\"): break it into specific subtasks FIRST using todo__add, then work on those.\n\
    3. Each todo should be atomic and be \"single responsibility\" - completable in one focused action.
    4. Continue with the next pending item now. Call tools immediately."
};

static REPL_COMMANDS: LazyLock<[ReplCommand; 39]> = LazyLock::new(|| {
    [
        ReplCommand::new(".help", "Show this help guide", AssertState::pass()),
        ReplCommand::new(".info", "Show system info", AssertState::pass()),
        ReplCommand::new(
            ".authenticate",
            "Authenticate the current model client via OAuth (if configured)",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".edit config",
            "Modify configuration file",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(".model", "Switch LLM model", AssertState::pass()),
        ReplCommand::new(
            ".prompt",
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
            ".compress session",
            "Compress session messages",
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
            ".rag",
            "Initialize or access RAG",
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
            ".regenerate",
            "Regenerate last response",
            AssertState::pass(),
        ),
        ReplCommand::new(".copy", "Copy last response", AssertState::pass()),
        ReplCommand::new(".set", "Modify runtime settings", AssertState::pass()),
        ReplCommand::new(
            ".delete",
            "Delete roles, sessions, RAGs, or agents",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".vault",
            "View or modify the Loki vault",
            AssertState::pass(),
        ),
        ReplCommand::new(".exit", "Exit REPL", AssertState::pass()),
    ]
});
static COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\.\S*)\s*").unwrap());
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
                r#"Welcome to {} {}
Type ".help" for additional help.
"#,
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION"),
            )
        }

        loop {
            if self.abort_signal.aborted_ctrld() {
                break;
            }
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
                        }
                        Err(err) => {
                            render_error(err);
                            println!()
                        }
                    }
                }
                Ok(Signal::CtrlC) => {
                    self.abort_signal.set_ctrlc();
                    println!("(To exit, press Ctrl+D or enter \".exit\")\n");
                }
                Ok(Signal::CtrlD) => {
                    self.abort_signal.set_ctrld();
                    break;
                }
                _ => {}
            }
        }
        self.ctx.write().exit_session()?;
        Ok(())
    }

    fn create_editor(ctx: Arc<RwLock<RequestContext>>, app: &AppConfig) -> Result<Reedline> {
        let completer = ReplCompleter::new(Arc::clone(&ctx));
        let highlighter = ReplHighlighter::new();
        let menu = Self::create_menu();
        let edit_mode = Self::create_edit_mode(app);
        let cursor_config = CursorConfig {
            vi_insert: Some(SetCursorStyle::BlinkingBar),
            vi_normal: Some(SetCursorStyle::SteadyBlock),
            emacs: None,
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
            Box::new(Vi::new(insert_keybindings, default_vi_normal_keybindings()))
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
    mut line: &str,
) -> Result<bool> {
    if let Ok(Some(captures)) = MULTILINE_RE.captures(line)
        && let Some(text_match) = captures.get(1)
    {
        line = text_match.as_str();
    }
    match parse_command(line) {
        Some((cmd, args)) => match cmd {
            ".help" => {
                dump_repl_help();
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
                Some(_) => unknown_command()?,
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
                None => println!("Usage: .model <name>"),
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
            ".prompt" => match args {
                Some(text) => {
                    let app = Arc::clone(&ctx.app.config);
                    ctx.use_prompt(app.as_ref(), text)?;
                }
                None => println!("Usage: .prompt <text>..."),
            },
            ".role" => match args {
                Some(args) => match args.split_once(['\n', ' ']) {
                    Some((name, text)) => {
                        let app = Arc::clone(&ctx.app.config);
                        let role = ctx.retrieve_role(app.as_ref(), name.trim())?;
                        let input = Input::from_str(ctx, text, Some(role));
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
                    r#"Usage:
    .role <name>                    # If the role exists, switch to it; otherwise, create a new role
    .role <name> [text]...          # Temporarily switch to the role, send the text, and switch back"#
                ),
            },
            ".session" => {
                if let Some(name) = graph::active_agent_graph_name(ctx) {
                    bail!(
                        "Graph-based agent '{name}' does not support sessions. \
                         The graph manages its own state."
                    );
                }
                let app = Arc::clone(&ctx.app.config);
                ctx.use_session(app.as_ref(), args, abort_signal.clone())
                    .await?;
                if ctx.maybe_autoname_session() {
                    let color = if app.light_theme() {
                        nu_ansi_term::Color::LightGray
                    } else {
                        nu_ansi_term::Color::DarkGray
                    };
                    eprintln!("\n📢 {}", color.italic().paint("Autonaming the session."),);
                    if let Err(err) = ctx.autoname_session(app.as_ref()).await {
                        warn!("Failed to autonaming the session: {err}");
                    }
                    if let Some(session) = ctx.session.as_mut() {
                        session.set_autonaming(false);
                    }
                }
            }
            ".rag" => {
                ctx.use_rag(args, abort_signal.clone()).await?;
            }
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
                }
                None => {
                    println!(r#"Usage: .agent <agent-name> [session-name] [key=value]..."#)
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
                            let input = Input::from_str(ctx, &text, None);
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
            ".save" => match split_first_arg(args) {
                Some(("role", name)) => {
                    ctx.save_role(name)?;
                }
                Some(("session", name)) => {
                    ctx.save_session(name)?;
                }
                _ => {
                    println!(r#"Usage: .save <role|session> [name]"#)
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
                    _ => {
                        println!(r#"Usage: .edit <config|role|session|rag-docs|agent-config>"#)
                    }
                }
            }
            ".compress" => match args {
                Some("session") => {
                    abortable_run_with_spinner(
                        ctx.compress_session(),
                        "Compressing",
                        abort_signal.clone(),
                    )
                    .await?;
                    println!("✓ Successfully compressed the session.");
                }
                _ => {
                    println!(r#"Usage: .compress session"#)
                }
            },
            ".empty" => match args {
                Some("session") => {
                    ctx.empty_session()?;
                }
                _ => {
                    println!(r#"Usage: .empty session"#)
                }
            },
            ".rebuild" => match args {
                Some("rag") => {
                    ctx.rebuild_rag(abort_signal.clone()).await?;
                }
                _ => {
                    println!(r#"Usage: .rebuild rag"#)
                }
            },
            ".sources" => match args {
                Some("rag") => {
                    let output = ctx.rag_sources()?;
                    println!("{output}");
                }
                _ => {
                    println!(r#"Usage: .sources rag"#)
                }
            },
            ".macro" => match split_first_arg(args) {
                Some((name, extra)) => {
                    let app = Arc::clone(&ctx.app.config);
                    if !paths::has_macro(name) && extra.is_none() {
                        ctx.new_macro(app.as_ref(), name)?;
                    } else {
                        macro_execute(ctx, name, extra, abort_signal.clone()).await?;
                    }
                }
                None => println!("Usage: .macro <name> <text>..."),
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
                    r#"Usage: .file <file|dir|url|cmd|loader:resource|%%>... [-- <text>...]

.file /tmp/file.txt
.file src/ Cargo.toml -- analyze
.file https://example.com/file.txt -- summarize
.file https://example.com/image.png -- recognize text
.file `git diff` -- Generate git commit message
.file jina:https://example.com
.file %% -- translate last reply to english"#
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
            ".regenerate" => {
                let LastMessage { mut input, .. } =
                    match ctx.last_message.as_ref().filter(|v| v.continuous).cloned() {
                        Some(v) => v,
                        None => bail!("Unable to regenerate the response"),
                    };
                let app = Arc::clone(&ctx.app.config);
                input.set_regenerate(ctx.extract_role(&app));
                ask(ctx, abort_signal.clone(), input, true).await?;
            }
            ".set" => match args {
                Some(args) => {
                    ctx.update(args, abort_signal).await?;
                }
                _ => {
                    println!("Usage: .set <key> <value>...")
                }
            },
            ".delete" => match args {
                Some(args) => {
                    ctx.delete(args)?;
                }
                _ => {
                    println!("Usage: .delete <role|session|rag|macro|agent-data>")
                }
            },
            ".copy" => {
                let output = match ctx
                    .last_message
                    .as_ref()
                    .filter(|v| !v.output.is_empty())
                    .map(|v| v.output.clone())
                {
                    Some(v) => v,
                    None => bail!("No chat response to copy"),
                };
                set_text(&output).context("Failed to copy the last chat response")?;
            }
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
                Some(_) => unknown_command()?,
                None => {
                    return Ok(true);
                }
            },
            ".clear" => match args {
                Some("messages") => {
                    bail!("Use '.empty session' instead");
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
                _ => unknown_command()?,
            },
            ".vault" => match split_first_arg(args) {
                Some(("add", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.add_secret(name)?;
                    } else {
                        println!("Usage: .vault add <name>");
                    }
                }
                Some(("get", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.get_secret(name, true)?;
                    } else {
                        println!("Usage: .vault get <name>");
                    }
                }
                Some(("update", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.update_secret(name)?;
                    } else {
                        println!("Usage: .vault update <name>");
                    }
                }
                Some(("delete", name)) => {
                    if let Some(name) = name {
                        ctx.app.vault.delete_secret(name)?;
                    } else {
                        println!("Usage: .vault delete <name>");
                    }
                }
                Some(("list", _)) => {
                    ctx.app.vault.list_secrets(true)?;
                }
                None | Some(_) => {
                    println!("Usage: .vault <add|get|update|delete|list> [name]")
                }
            },
            _ => unknown_command()?,
        },
        None => {
            reset_continuation(ctx);
            let input = Input::from_str(ctx, line, None);
            ask(ctx, abort_signal.clone(), input, true).await?;
        }
    }

    if !ctx.macro_flag {
        println!();
    }

    Ok(false)
}

#[async_recursion::async_recursion]
async fn ask(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    mut input: Input,
    with_embeddings: bool,
) -> Result<()> {
    if input.is_empty() {
        return Ok(());
    }
    if with_embeddings {
        input.use_embeddings(abort_signal.clone()).await?;
    }
    while ctx.is_compressing_session() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let app = Arc::clone(&ctx.app.config);

    if graph::active_agent_graph_name(ctx).is_some() {
        ctx.before_chat_completion(&input)?;
        let output =
            graph::run_active_agent_graph(ctx, &input.text(), abort_signal.clone()).await?;
        app.print_markdown(&output)?;
        ctx.after_chat_completion(app.as_ref(), &input, &output, &[])?;
        return Ok(());
    }

    let client = input.create_client()?;
    ctx.before_chat_completion(&input)?;
    let (output, tool_results) = if input.stream() {
        call_chat_completions_streaming(&input, client.as_ref(), ctx, abort_signal.clone()).await?
    } else {
        call_chat_completions(
            &input,
            true,
            false,
            client.as_ref(),
            ctx,
            abort_signal.clone(),
        )
        .await?
    };
    ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;
    if !tool_results.is_empty() {
        ask(
            ctx,
            abort_signal,
            input.merge_tool_results(output, tool_results),
            false,
        )
        .await
    } else {
        let do_continue = should_continue(ctx);

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

                let color = if app.light_theme() {
                    nu_ansi_term::Color::LightGray
                } else {
                    nu_ansi_term::Color::DarkGray
                };
                eprintln!(
                    "\n📋 {}",
                    color.italic().paint(format!(
                        "Auto-continuing ({count}/{max}): {remaining} incomplete todo(s) remain"
                    ))
                );

                format!("{prompt}\n\n{todo_state}")
            };
            let continuation_input = Input::from_str(ctx, &full_prompt, None);
            ask(ctx, abort_signal, continuation_input, false).await
        } else {
            reset_continuation(ctx);
            if ctx.maybe_autoname_session() {
                let color = if app.light_theme() {
                    nu_ansi_term::Color::LightGray
                } else {
                    nu_ansi_term::Color::DarkGray
                };
                eprintln!("\n📢 {}", color.italic().paint("Autonaming the session."),);
                if let Err(err) = ctx.autoname_session(app.as_ref()).await {
                    warn!("Failed to autonaming the session: {err}");
                }
                if let Some(session) = ctx.session.as_mut() {
                    session.set_autonaming(false);
                }
            }

            let needs_compression = ctx
                .session
                .as_ref()
                .is_some_and(|s| s.needs_compression(app.compression_threshold));

            if needs_compression {
                let agent_can_continue_after_compress = should_continue(ctx);

                if let Some(session) = ctx.session.as_mut() {
                    session.set_compressing(true);
                }

                let color = if app.light_theme() {
                    nu_ansi_term::Color::LightGray
                } else {
                    nu_ansi_term::Color::DarkGray
                };
                eprintln!("\n📢 {}", color.italic().paint("Compressing the session."),);

                if let Err(err) = ctx.compress_session().await {
                    warn!("Failed to compress the session: {err}");
                }
                if let Some(session) = ctx.session.as_mut() {
                    session.set_compressing(false);
                }

                if agent_can_continue_after_compress {
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

                        let color = if app.light_theme() {
                            nu_ansi_term::Color::LightGray
                        } else {
                            nu_ansi_term::Color::DarkGray
                        };
                        eprintln!(
                            "\n📋 {}",
                            color.italic().paint(format!(
                                "Auto-continuing after compression ({count}/{max}): {remaining} incomplete todo(s) remain"
                            ))
                        );

                        format!("{prompt}\n\n{todo_state}")
                    };
                    let continuation_input = Input::from_str(ctx, &full_prompt, None);
                    return ask(ctx, abort_signal, continuation_input, false).await;
                }
            }

            Ok(())
        }
    }
}

fn should_continue(ctx: &RequestContext) -> bool {
    let config = ctx.auto_continue_config();
    ctx.app.config.function_calling_support
        && config.enabled
        && ctx.auto_continue_count < config.max_continues
        && ctx.todo_list.has_incomplete()
}

fn reset_continuation(ctx: &mut RequestContext) {
    ctx.reset_continuation_count();
}

fn unknown_command() -> Result<()> {
    bail!(r#"Unknown command. Type ".help" for additional help."#);
}

fn dump_repl_help() {
    let head = REPL_COMMANDS
        .iter()
        .map(|cmd| format!("{:<24} {}", cmd.name, cmd.description))
        .collect::<Vec<String>>()
        .join("\n");
    println!(
        r###"{head}

Type ::: to start multi-line editing, type ::: to finish it.
Press Ctrl+O to open an editor for editing the input buffer.
Press Ctrl+C to cancel the response, Ctrl+D to exit the REPL."###,
    );
}

fn parse_command(line: &str) -> Option<(&str, Option<&str>)> {
    match COMMAND_RE.captures(line) {
        Ok(Some(captures)) => {
            let cmd = captures.get(1)?.as_str();
            let args = line[captures[0].len()..].trim();
            let args = if args.is_empty() { None } else { Some(args) };
            Some((cmd, args))
        }
        _ => None,
    }
}

fn split_first_arg(args: Option<&str>) -> Option<(&str, Option<&str>)> {
    args.map(|v| match v.split_once(' ') {
        Some((subcmd, args)) => (subcmd, Some(args.trim())),
        None => (v, None),
    })
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

    #[test]
    fn test_process_command_line() {
        assert_eq!(parse_command(" ."), Some((".", None)));
        assert_eq!(parse_command(" .role"), Some((".role", None)));
        assert_eq!(parse_command(" .role  "), Some((".role", None)));
        assert_eq!(
            parse_command(" .set dry_run true"),
            Some((".set", Some("dry_run true")))
        );
        assert_eq!(
            parse_command(" .set dry_run true  "),
            Some((".set", Some("dry_run true")))
        );
        assert_eq!(
            parse_command(".prompt \nabc\n"),
            Some((".prompt", Some("abc")))
        );
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
    fn repl_commands_has_39_entries() {
        assert_eq!(REPL_COMMANDS.len(), 39);
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
    fn repl_commands_prompt_blocked_in_session_or_agent() {
        let cmd = REPL_COMMANDS.iter().find(|c| c.name == ".prompt").unwrap();
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
        assert!(parse_command("hello world").is_none());
    }

    #[test]
    fn parse_command_empty_returns_none() {
        assert!(parse_command("").is_none());
    }

    #[test]
    fn parse_command_whitespace_only_returns_none() {
        assert!(parse_command("   ").is_none());
    }

    #[test]
    fn parse_command_dot_only() {
        assert_eq!(parse_command("."), Some((".", None)));
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
}
