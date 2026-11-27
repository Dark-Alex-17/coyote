use crate::config::CommandPrefix;
use crate::repl::unfence;

/// When a command typed during a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Timing {
    /// Runs at once on the context the turn started with: it only reads
    /// that context or edits a file the next turn picks up.
    Now,
    /// Changes a setting the next turn uses: it runs at once on the
    /// context the turn started with, which checks it, and the live
    /// context takes it when the turn ends.
    Deferred,
    /// Waits for the turn to end and runs as a turn of its own, for the
    /// reason given.
    Queue(&'static str),
}

const CHANGES_AGENT: Timing = Timing::Queue("changes the agent");
const CHANGES_CONVERSATION: Timing = Timing::Queue("changes the conversation");
const CHANGES_RAG: Timing = Timing::Queue("changes the RAG");
const CHANGES_ROLE: Timing = Timing::Queue("changes the role");
const CHANGES_SESSION: Timing = Timing::Queue("changes the session");
const CHANGES_SKILLS: Timing = Timing::Queue("changes the loaded skills");
const CHANGES_TOOLS: Timing = Timing::Queue("changes the tools the model is offered");
const PROMPTS_MODEL: Timing = Timing::Queue("sends a prompt to the model");
const PROMPTS_USER: Timing = Timing::Queue("asks questions while it runs");
const SAVES_RAG: Timing = Timing::Queue("saves the RAG");
const STARTS_MCP_SERVERS: Timing = Timing::Queue("starts or stops MCP servers");

/// Every command the TUI knows by name, with its timing. A `.set <key>`
/// entry queues a key whose update does I/O, starts processes, saves
/// files, prints or reads the network, or one that changes how the lines
/// held behind the turn are read. Every other key only changes memory and
/// is deferred, the in-memory switches for the tools the next turn offers
/// among them, so replaying a deferred `.set` on the loop thread stays
/// cheap.
pub(super) const COMMAND_TIMINGS: &[(&str, Timing)] = &[
    (".help", Timing::Now),
    (".keys", Timing::Now),
    (".threads", Timing::Now),
    (".info", Timing::Now),
    (".info role", Timing::Now),
    (".info session", Timing::Now),
    (".info rag", Timing::Now),
    (".info agent", Timing::Now),
    (".info tools", Timing::Now),
    (".info todo", Timing::Now),
    (".info mcp-server", Timing::Now),
    (".edit config", Timing::Now),
    (".edit mcp-config", Timing::Now),
    (".edit agent-config", Timing::Now),
    (".edit skill", Timing::Now),
    (".list", Timing::Now),
    (".list roles", Timing::Now),
    (".list sessions", Timing::Now),
    (".list agents", Timing::Now),
    (".list rags", Timing::Now),
    (".list macros", Timing::Now),
    (".list skills", Timing::Now),
    (".list prompts", Timing::Now),
    (".list tools", Timing::Now),
    (".list mcp-servers", Timing::Now),
    (".list bundles", Timing::Now),
    (".sources rag", Timing::Now),
    (".skill loaded", Timing::Now),
    (".set", Timing::Deferred),
    (".set skills_enabled", STARTS_MCP_SERVERS),
    (".set enabled_mcp_servers", STARTS_MCP_SERVERS),
    (".set mcp_server_support", STARTS_MCP_SERVERS),
    (
        ".set mcp_tools.",
        Timing::Queue("asks the MCP server for its tools"),
    ),
    (".set rag_top_k", SAVES_RAG),
    (".set rag_reranker_model", SAVES_RAG),
    (
        ".set command_prefix",
        Timing::Queue("changes how lines are read"),
    ),
    (".reasoning", Timing::Deferred),
    (".authenticate", PROMPTS_USER),
    (".mcp auth", PROMPTS_USER),
    (".mcp enable", CHANGES_TOOLS),
    (".mcp disable", CHANGES_TOOLS),
    (".tool enable", CHANGES_TOOLS),
    (".tool disable", CHANGES_TOOLS),
    (".model", Timing::Queue("changes the model")),
    (".prompt", PROMPTS_MODEL),
    (".temp-role", CHANGES_ROLE),
    (".role", CHANGES_ROLE),
    (
        ".edit role",
        Timing::Queue("reloads the role into the conversation"),
    ),
    (
        ".save role",
        Timing::Queue("saves the role the turn is using"),
    ),
    (".exit role", CHANGES_ROLE),
    (".session", CHANGES_SESSION),
    (".empty session", CHANGES_SESSION),
    (".undo", CHANGES_CONVERSATION),
    (".compress session", CHANGES_CONVERSATION),
    (".edit session", CHANGES_SESSION),
    (
        ".save session",
        Timing::Queue("saves the session the turn is writing"),
    ),
    (".exit session", CHANGES_SESSION),
    (".fork", CHANGES_SESSION),
    (".agent", CHANGES_AGENT),
    (".starter", PROMPTS_MODEL),
    (".exit agent", CHANGES_AGENT),
    (".clear todo", Timing::Queue("changes the todo list")),
    (
        ".clear images",
        Timing::Queue("changes the images the next prompt carries"),
    ),
    (".clear messages", CHANGES_CONVERSATION),
    (".rag", CHANGES_RAG),
    (".rag attach", CHANGES_RAG),
    (".edit rag-docs", Timing::Queue("rebuilds the RAG")),
    (".rebuild rag", Timing::Queue("rebuilds the RAG")),
    (".exit rag", CHANGES_RAG),
    (".macro", Timing::Queue("runs commands on the live context")),
    (".macro enable", Timing::Queue("changes the macros")),
    (".macro disable", Timing::Queue("changes the macros")),
    (
        ".skill",
        Timing::Queue("creates the skill when it is missing"),
    ),
    (".skill load", CHANGES_SKILLS),
    (".skill unload", CHANGES_SKILLS),
    (".file", PROMPTS_MODEL),
    (".continue", PROMPTS_MODEL),
    (".recover", CHANGES_CONVERSATION),
    (".regenerate", PROMPTS_MODEL),
    (".run", PROMPTS_MODEL),
    (
        ".copy",
        Timing::Queue("copies the reply the turn is still writing"),
    ),
    (
        ".redraw",
        Timing::Queue("redraws the reply the turn is still writing"),
    ),
    (
        ".redraw session",
        Timing::Queue("redraws the session the turn is still writing"),
    ),
    (".delete", PROMPTS_USER),
    (".vault", PROMPTS_USER),
    (".vault add", PROMPTS_USER),
    (".vault get", PROMPTS_USER),
    (".vault update", PROMPTS_USER),
    (".vault delete", PROMPTS_USER),
    (".vault list", PROMPTS_USER),
    (".install", PROMPTS_USER),
    (".uninstall", PROMPTS_USER),
    (".update", PROMPTS_USER),
    (".exit", Timing::Queue("ends the session")),
];

/// The entry whose words begin the line under the active prefix, the
/// longest such entry when several do: `.info mcp-server foo` is
/// `.info mcp-server`, `.info foo` is `.info`. A line no entry names (a
/// macro, a `!shell` line) has none and waits like `Queue`. An entry
/// word ending in `.` matches any word it begins, so `.set mcp_tools.`
/// covers `.set mcp_tools.github`.
pub(super) fn lookup(line: &str, prefix: CommandPrefix) -> Option<&'static (&'static str, Timing)> {
    let line = prefix.normalize(unfence(line).trim())?;
    let words: Vec<&str> = line.split_whitespace().collect();
    COMMAND_TIMINGS
        .iter()
        .filter(|(name, _)| {
            let name_words: Vec<&str> = name.split_whitespace().collect();
            name_words.len() <= words.len()
                && name_words.iter().zip(&words).all(|(name, word)| {
                    name == word || (name.ends_with('.') && word.starts_with(name))
                })
        })
        .max_by_key(|(name, _)| name.split_whitespace().count())
}
