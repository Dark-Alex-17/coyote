use crate::cli::{Cli, McpScopeArg, McpTransportArg};
use crate::config::conflict::{menu_can_use_terminal, refuse_non_interactive};
use crate::config::{ensure_parent_exists, paths};
use crate::mcp::{JsonField, McpOAuthConfig, McpServer, McpServersConfig, McpTransportType};
use crate::vault::{SECRET_RE, Vault};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::{IndexMap, IndexSet};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

impl From<McpTransportArg> for McpTransportType {
    fn from(value: McpTransportArg) -> Self {
        match value {
            McpTransportArg::Stdio => McpTransportType::Stdio,
            McpTransportArg::Http => McpTransportType::Http,
            McpTransportArg::Sse => McpTransportType::Sse,
        }
    }
}

pub fn handle(cli: &Cli, vault: &Vault) -> Result<()> {
    if cli.mcp_list {
        return handle_list(cli.scope);
    }
    if let Some(name) = &cli.mcp_get {
        return handle_get(name, cli.scope);
    }
    if let Some(name) = &cli.mcp_remove {
        return handle_remove(name, cli.scope, cli.mcp_force);
    }
    if let Some(name) = &cli.mcp_add {
        return handle_add(cli, name, vault);
    }

    Ok(())
}

fn handle_list(scope: Option<McpScopeArg>) -> Result<()> {
    let show_user = scope != Some(McpScopeArg::Workspace);
    let show_workspace = scope != Some(McpScopeArg::User);

    if show_user {
        let user_path = paths::mcp_config_file();
        let user_cfg = load_config_raw(&user_path)?;
        println!("User ({})", user_path.display());
        print_server_list(&user_cfg);
    }

    if show_workspace {
        match paths::workspace_mcp_config_file() {
            Some(ws_path) => {
                let ws_cfg = load_config_raw(&ws_path)?;
                if show_user {
                    println!();
                }
                println!("Workspace ({})", ws_path.display());
                print_server_list(&ws_cfg);
            }
            None if scope == Some(McpScopeArg::Workspace) => {
                println!("Workspace: no mcp.json found in current directory");
            }
            None => {}
        }
    }

    Ok(())
}

fn print_server_list(cfg: &McpServersConfig) {
    if cfg.mcp_servers.is_empty() {
        println!("  (none)");
        return;
    }
    let name_width = cfg.mcp_servers.keys().map(String::len).max().unwrap_or(0);
    for (name, spec) in &cfg.mcp_servers {
        let transport = match spec.transport_type {
            McpTransportType::Stdio => "stdio",
            McpTransportType::Http => "http",
            McpTransportType::Sse => "sse",
        };
        let target = spec.url.clone().unwrap_or_else(|| {
            let cmd = spec.command.clone().unwrap_or_default();
            let args = spec.args.as_ref().map(|a| a.join(" ")).unwrap_or_default();
            if args.is_empty() {
                cmd
            } else {
                format!("{cmd} {args}")
            }
        });
        println!(
            "  {name:<name_width$}  {transport:<5}  {target}",
            name_width = name_width
        );
    }
}

fn handle_get(name: &str, scope: Option<McpScopeArg>) -> Result<()> {
    let (path, cfg) = load_for_scope_or_search(name, scope)?;
    let spec = cfg
        .mcp_servers
        .get(name)
        .ok_or_else(|| anyhow!("MCP server '{name}' not found"))?;
    let pretty =
        serde_json::to_string_pretty(spec).context("failed to serialize MCP server config")?;
    println!("# {}", path.display());
    println!("{pretty}");

    Ok(())
}

fn handle_remove(name: &str, scope: Option<McpScopeArg>, force: bool) -> Result<()> {
    let (path, mut cfg) = load_for_scope_or_search(name, scope)?;
    if !force {
        let ok = confirm_on_the_terminal(
            format_args!("remove MCP server '{name}'"),
            format!("Remove MCP server '{name}' from {}?", path.display()),
        )?;
        if !ok {
            println!("Aborted.");
            return Ok(());
        }
    }

    cfg.mcp_servers.shift_remove(name);
    save_config(&path, &cfg)?;
    println!("✓ Removed MCP server '{name}' from {}", path.display());

    Ok(())
}

fn handle_add(cli: &Cli, name: &str, vault: &Vault) -> Result<()> {
    validate_name(name)?;
    let server = build_server(cli)?;
    server.validate(name)?;

    let scope = cli.scope.unwrap_or_default();
    let path = write_path_for_scope(scope);
    let mut cfg = load_config_raw(&path)?;

    if cfg.mcp_servers.contains_key(name) && !cli.mcp_force {
        let ok = confirm_on_the_terminal(
            format_args!("overwrite MCP server '{name}'"),
            format!(
                "MCP server '{name}' already exists in {}. Overwrite?",
                path.display()
            ),
        )?;
        if !ok {
            println!("Aborted. Use --mcp-force to overwrite without prompting.");
            return Ok(());
        }
    }

    provision_secrets(cli, name, vault)?;

    cfg.mcp_servers.insert(name.to_string(), server);
    save_config(&path, &cfg)?;
    println!("✓ Added MCP server '{name}' to {}", path.display());

    Ok(())
}

/// A yes/no question on the terminal, defaulting to no. A run with
/// nowhere to ask is refused in the shared shape rather than left on a
/// widget nothing can answer.
fn confirm_on_the_terminal(action: fmt::Arguments<'_>, question: String) -> Result<bool> {
    if !menu_can_use_terminal() {
        return Err(refuse_non_interactive(action, "."));
    }
    let prompter = crate::prompt::InquirePrompter::default();
    crate::prompt::confirm(&prompter, question, false)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("MCP server name cannot be empty");
    }

    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Invalid MCP server name '{name}': only letters, digits, '-', and '_' are allowed");
    }

    Ok(())
}

fn build_server(cli: &Cli) -> Result<McpServer> {
    let has_command = !cli.mcp_command.is_empty();
    let has_url = cli.url.is_some();

    let transport = cli
        .transport
        .map(McpTransportType::from)
        .unwrap_or_else(|| {
            if has_command {
                McpTransportType::Stdio
            } else {
                McpTransportType::Http
            }
        });

    match transport {
        McpTransportType::Stdio => build_stdio(cli, has_url),
        McpTransportType::Http | McpTransportType::Sse => build_remote(cli, transport, has_command),
    }
}

fn build_stdio(cli: &Cli, has_url: bool) -> Result<McpServer> {
    if cli.mcp_command.is_empty() {
        bail!(
            "stdio MCP server requires a command. Pass it after `--`, e.g. \
             `--mcp-add NAME -- npx some-server --flag`"
        );
    }
    if has_url {
        bail!("stdio MCP server does not accept --url");
    }
    if !cli.header.is_empty() {
        bail!("stdio MCP server does not accept --header");
    }
    if cli.client_id.is_some()
        || cli.client_secret.is_some()
        || cli.callback_port.is_some()
        || cli.redirect_host.is_some()
    {
        bail!("stdio MCP server does not accept OAuth flags");
    }

    let (cmd, args) = cli.mcp_command.split_first().unwrap();

    let mut env: IndexMap<String, JsonField> = IndexMap::new();
    for kv in &cli.env {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --env value '{kv}': expected KEY=VALUE"))?;
        if k.is_empty() {
            bail!("invalid --env value '{kv}': KEY cannot be empty");
        }
        env.insert(k.to_string(), JsonField::Str(v.to_string()));
    }

    Ok(McpServer {
        transport_type: McpTransportType::Stdio,
        command: Some(cmd.clone()),
        args: (!args.is_empty()).then(|| args.to_vec()),
        env: (!env.is_empty()).then_some(env),
        cwd: cli.cwd.clone(),
        url: None,
        headers: None,
        oauth: None,
        allowed_tools: None,
        initialize_timeout: None,
    })
}

fn build_remote(cli: &Cli, transport: McpTransportType, has_command: bool) -> Result<McpServer> {
    if has_command {
        bail!(
            "http/sse MCP server does not accept a trailing `-- <cmd>`. Use `--url` \
             to specify the endpoint."
        );
    }
    let url = cli
        .url
        .clone()
        .ok_or_else(|| anyhow!("http/sse MCP server requires --url <URL>"))?;
    if !cli.env.is_empty() {
        bail!("http/sse MCP server does not accept --env; use --header instead");
    }
    if cli.cwd.is_some() {
        bail!("http/sse MCP server does not accept --cwd");
    }

    let mut headers: IndexMap<String, String> = IndexMap::new();
    for h in &cli.header {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid --header value '{h}': expected 'Name: Value'"))?;
        let name = name.trim();
        let value = value.trim_start_matches(' ');
        if name.is_empty() {
            bail!("invalid --header value '{h}': header name cannot be empty");
        }
        headers.insert(name.to_string(), value.to_string());
    }

    let oauth = if cli.client_id.is_some()
        || cli.client_secret.is_some()
        || cli.callback_port.is_some()
        || cli.redirect_host.is_some()
    {
        Some(McpOAuthConfig {
            client_id: cli.client_id.clone(),
            client_secret: cli.client_secret.clone(),
            callback_port: cli.callback_port,
            redirect_host: cli.redirect_host.clone(),
        })
    } else {
        None
    };

    Ok(McpServer {
        transport_type: transport,
        command: None,
        args: None,
        env: None,
        cwd: None,
        url: Some(url),
        headers: (!headers.is_empty()).then_some(headers),
        oauth,
        allowed_tools: None,
        initialize_timeout: None,
    })
}

fn provision_secrets(cli: &Cli, server: &str, vault: &Vault) -> Result<()> {
    let mut sources: Vec<&str> = Vec::new();
    if let Some(s) = cli.url.as_deref() {
        sources.push(s);
    }
    if let Some(s) = cli.client_secret.as_deref() {
        sources.push(s);
    }
    if let Some(s) = cli.client_id.as_deref() {
        sources.push(s);
    }
    if let Some(s) = cli.redirect_host.as_deref() {
        sources.push(s);
    }
    if let Some(s) = cli.cwd.as_deref() {
        sources.push(s);
    }
    sources.extend(cli.env.iter().map(String::as_str));
    sources.extend(cli.header.iter().map(String::as_str));

    let mut needed: IndexSet<String> = IndexSet::new();
    for value in sources {
        for caps in SECRET_RE.captures_iter(value).filter_map(Result::ok) {
            if let Some(m) = caps.get(1) {
                needed.insert(m.as_str().trim().to_string());
            }
        }
    }

    if needed.is_empty() {
        return Ok(());
    }

    let existing: HashSet<String> = vault.list_secrets(false)?.into_iter().collect();
    let missing: Vec<&str> = needed
        .iter()
        .filter(|name| !existing.contains(*name))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    refuse_missing_secrets_without_a_terminal(server, &missing)?;
    let prompter = crate::prompt::InquirePrompter::default();
    for name in missing {
        eprintln!("Value references vault secret {{{{ {name} }}}} which is not stored yet.");
        let ok =
            crate::prompt::confirm(&prompter, format!("Add '{name}' to the vault now?"), true)?;
        if !ok {
            bail!(
                "Vault secret '{name}' is required by the config; aborting. \
                 Add it later with `coyote --add-secret {name}`."
            );
        }
        vault.add_secret(name, &prompter)?;
    }

    Ok(())
}

/// Each missing secret is asked for and then entered on the terminal, so
/// a run with nowhere to ask is refused before the first question rather
/// than part way through the list.
fn refuse_missing_secrets_without_a_terminal(server: &str, missing: &[&str]) -> Result<()> {
    if menu_can_use_terminal() {
        return Ok(());
    }
    let commands: Vec<String> = missing
        .iter()
        .map(|name| format!("`coyote --add-secret {name}`"))
        .collect();
    Err(refuse_non_interactive(
        format_args!("provision the MCP secrets for '{server}'"),
        format_args!(
            " or add them to the vault first with {}.",
            commands.join(", ")
        ),
    ))
}

fn load_for_scope_or_search(
    name: &str,
    scope: Option<McpScopeArg>,
) -> Result<(PathBuf, McpServersConfig)> {
    if let Some(s) = scope {
        let path = match s {
            McpScopeArg::User => paths::mcp_config_file(),
            McpScopeArg::Workspace => paths::workspace_mcp_config_file()
                .ok_or_else(|| anyhow!("no workspace mcp.json found in the current directory"))?,
        };
        let cfg = load_config_raw(&path)?;
        if !cfg.mcp_servers.contains_key(name) {
            bail!(
                "MCP server '{name}' not found in {} scope ({})",
                scope_label(s),
                path.display()
            );
        }

        return Ok((path, cfg));
    }

    let user_path = paths::mcp_config_file();
    let user_cfg = load_config_raw(&user_path)?;
    if user_cfg.mcp_servers.contains_key(name) {
        return Ok((user_path, user_cfg));
    }

    if let Some(ws_path) = paths::workspace_mcp_config_file() {
        let ws_cfg = load_config_raw(&ws_path)?;
        if ws_cfg.mcp_servers.contains_key(name) {
            return Ok((ws_path, ws_cfg));
        }
    }

    bail!("MCP server '{name}' not found in any scope");
}

fn write_path_for_scope(scope: McpScopeArg) -> PathBuf {
    match scope {
        McpScopeArg::User => paths::mcp_config_file(),
        McpScopeArg::Workspace => paths::workspace_mcp_config_file()
            .unwrap_or_else(|| paths::workspace_config_dir().join("mcp.json")),
    }
}

fn scope_label(scope: McpScopeArg) -> &'static str {
    match scope {
        McpScopeArg::User => "user",
        McpScopeArg::Workspace => "workspace",
    }
}

fn load_config_raw(path: &Path) -> Result<McpServersConfig> {
    if !path.exists() {
        return Ok(McpServersConfig {
            mcp_servers: IndexMap::new(),
        });
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read MCP config at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(McpServersConfig {
            mcp_servers: IndexMap::new(),
        });
    }

    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse MCP config at {}", path.display()))
}

fn save_config(path: &Path, config: &McpServersConfig) -> Result<()> {
    ensure_parent_exists(path)?;
    let serialized =
        serde_json::to_string_pretty(config).context("failed to serialize MCP config")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &serialized)
        .with_context(|| format!("failed to write temporary MCP config at {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to finalize MCP config at {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::conflict::prompt_script;
    use clap::Parser;
    use serial_test::serial;

    #[test]
    #[serial]
    fn without_a_terminal_missing_mcp_secrets_are_refused_before_asking() {
        let _tui_off = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, false);
        let _script = prompt_script::install_non_interactive();

        let err = refuse_missing_secrets_without_a_terminal("github", &["GH_TOKEN", "GH_USER"])
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Refusing to provision the MCP secrets for 'github' non-interactively. \
             Re-run in a terminal or add them to the vault first with \
             `coyote --add-secret GH_TOKEN`, `coyote --add-secret GH_USER`."
        );
        assert_eq!(prompt_script::prompts_asked(), 0);
    }

    #[test]
    #[serial]
    fn with_a_terminal_missing_mcp_secrets_go_on_to_be_asked_for() {
        let _tui_off = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, false);
        let _script = prompt_script::install(&[]);

        refuse_missing_secrets_without_a_terminal("github", &["GH_TOKEN"]).unwrap();
    }

    /// A config dir holding a user mcp.json with one server, plus the
    /// config.yaml the vault needs to open.
    struct McpConfigDir {
        path: PathBuf,
        _config_dir: crate::testing::EnvVarGuard,
    }

    impl McpConfigDir {
        fn with_server(label: &str, name: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("coyote-mcp-{label}-{unique}"));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("config.yaml"), "vault_password_file: /dev/null\n").unwrap();
            fs::write(
                path.join("mcp.json"),
                format!(
                    r#"{{"mcpServers":{{"{name}":{{"type":"http","url":"https://example.test/mcp"}}}}}}"#
                ),
            )
            .unwrap();
            let _config_dir =
                crate::testing::EnvVarGuard::set(crate::utils::get_env_name("config_dir"), &path);
            Self { path, _config_dir }
        }

        fn mcp_json(&self) -> String {
            fs::read_to_string(self.path.join("mcp.json")).unwrap()
        }
    }

    impl Drop for McpConfigDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    #[serial]
    fn without_a_terminal_removing_an_mcp_server_is_refused_before_asking() {
        let _tui_off = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, false);
        let _script = prompt_script::install_non_interactive();
        let dir = McpConfigDir::with_server("remove", "github");
        let before = dir.mcp_json();

        let err = handle_remove("github", Some(McpScopeArg::User), false).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Refusing to remove MCP server 'github' non-interactively. Re-run in a terminal."
        );
        assert_eq!(dir.mcp_json(), before);
    }

    #[test]
    #[serial]
    fn without_a_terminal_overwriting_an_mcp_server_is_refused_before_asking() {
        let _tui_off = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, false);
        let _script = prompt_script::install_non_interactive();
        let dir = McpConfigDir::with_server("overwrite", "github");
        let before = dir.mcp_json();
        let cli = Cli::try_parse_from([
            "coyote",
            "--mcp-add",
            "github",
            "--url",
            "https://example.test/other",
        ])
        .unwrap();
        let vault = Vault::init_bare().unwrap();

        let err = handle_add(&cli, "github", &vault).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Refusing to overwrite MCP server 'github' non-interactively. Re-run in a terminal."
        );
        assert_eq!(dir.mcp_json(), before);
    }
}
