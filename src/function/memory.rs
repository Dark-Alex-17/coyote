use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{env, fs};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Days, Local};
use indexmap::IndexMap;
use log::warn;
use serde_json::{Value, json};

use super::{FunctionDeclaration, JsonSchema};
use crate::config::RequestContext;
use crate::config::memory::{
    DEFAULT_MEMORY_RUN_STATE_TTL_DAYS, KindField, MemoryFile, MemoryFrontmatter, MemoryKind,
    MemoryStore, Retention, WorkspaceMemory, bootstrap_workspace_memory, catalogue_slug,
    find_git_root, today_string,
};
use crate::config::paths;

pub const MEMORY_FUNCTION_PREFIX: &str = "memory__";

const PER_FILE_SOFT_CAP: usize = 2_000;

pub fn memory_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}read"),
            description: "Read the full content of a specific memory file by its name slug."
                .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "name".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some(
                            "The `name:` slug of the memory file to read (from MEMORY.md index)"
                                .into(),
                        ),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["name".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}write"),
            description:
                "Create or replace a memory file. REQUIRED arguments: name, description, content, \
                 scope (all four must be passed as function arguments; the call fails without them). \
                 Coyote writes the YAML front matter and the MEMORY.md index entry itself from these \
                 arguments, so never put `name:` / `description:` / `scope:` / `type:` lines inside \
                 `content`."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "name".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "REQUIRED. Short kebab-case slug for the file (no extension). \
                                 A function argument, not a front-matter line."
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "description".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "REQUIRED. One plain-text line for the MEMORY.md index entry. \
                                 A function argument, not a front-matter line inside `content`."
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "type".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(format!(
                                "Optional memory type: {} (run-state defaults `expires` to \
                                 today + memory_run_state_ttl_days)",
                                MemoryKind::NAMES.join(" | ")
                            )),
                            ..Default::default()
                        },
                    ),
                    (
                        "content".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "REQUIRED. The markdown BODY of the memory file only. Do NOT start it \
                                 with a `---` YAML front-matter block; Coyote generates the front matter \
                                 (name, description, type, created, updated) from the other arguments."
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "scope".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "REQUIRED. Where to write: 'global' (user-level) or 'workspace' \
                                 (project-level). A function argument, not a front-matter line."
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "superseded_by".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Optional `name:` slug of the memory that replaces this one. \
                                 `memory__lint` flags superseded files for cleanup. Omitting this \
                                 on overwrite clears any previous value."
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "expires".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Optional ISO date (YYYY-MM-DD) after which this memory is stale. \
                                 `memory__lint` flags expired files. Omitting this on overwrite \
                                 clears any previous value."
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec![
                    "name".to_string(),
                    "description".to_string(),
                    "content".to_string(),
                    "scope".to_string(),
                ]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}list"),
            description: "List all live drill files with metadata (size, type, scope). Superseded \
                          and expired files are hidden unless include_stale is true."
                .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "include_stale".to_string(),
                    JsonSchema {
                        type_value: Some("boolean".to_string()),
                        description: Some(
                            "Also list superseded and expired files, marked `stale: true` (default false)"
                                .into(),
                        ),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}lint"),
            description: "Health-check memory: orphan files, broken [[wikilinks]], oversized files."
                .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}edit_index"),
            description:
                "Replace the entire MEMORY.md index at the given scope. Use to add always-on facts, \
                 reorganize, prune stale entries, or fix descriptions. Coyote manages the path; \
                 NEVER use fs_write or any other generic file tool on MEMORY.md."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "scope".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Where to edit: 'global' (user-level) or 'workspace' (project-level)"
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "content".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Full new contents of MEMORY.md".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["scope".to_string(), "content".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}rename"),
            description:
                "Rename a memory file. Its MEMORY.md index entry and every [[wikilink]] to it in \
                 other memory files are rewritten automatically."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "name".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Current `name:` slug of the memory file".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "new_name".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "New kebab-case slug for the file (no extension)".into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "scope".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Scope of the file: 'global' (user-level) or 'workspace' (project-level)"
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec![
                    "name".to_string(),
                    "new_name".to_string(),
                    "scope".to_string(),
                ]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{MEMORY_FUNCTION_PREFIX}delete"),
            description:
                "Delete a memory file and remove its MEMORY.md index entry. Reports any \
                 [[wikilinks]] in other memory files left dangling by the deletion."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "name".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "The `name:` slug of the memory file to delete".into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "scope".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Scope of the file: 'global' (user-level) or 'workspace' (project-level)"
                                    .into(),
                            ),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["name".to_string(), "scope".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub fn handle_memory_tool(ctx: &mut RequestContext, cmd_name: &str, args: &Value) -> Result<Value> {
    if !ctx.should_register_memory_tools() {
        bail!("Memory tools are disabled (memory off or function calling unavailable).");
    }

    let action = cmd_name
        .strip_prefix(MEMORY_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);
    let cwd = env::current_dir().context("get cwd")?;
    let store = MemoryStore::new(&cwd);
    let run_state_ttl_days = ctx
        .app
        .config
        .memory_run_state_ttl_days
        .unwrap_or(DEFAULT_MEMORY_RUN_STATE_TTL_DAYS);

    match action {
        "read" => {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("name is required"))?;
            let file = find_file(&store, name)?
                .ok_or_else(|| anyhow!("memory file '{}' not found", name))?;

            Ok(json!({
                "name": file.frontmatter.name,
                "type": file.frontmatter.kind.kind().name(),
                "content": file.body,
            }))
        }
        "list" => {
            let include_stale = args
                .get("include_stale")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            Ok(json!({
                "files": list_entries(&store, include_stale)?,
                "global_index_exists": paths::global_memory_index_file().exists(),
                "workspace": store.workspace.as_ref().map(workspace_label),
            }))
        }
        "write" => write_memory(&store, &cwd, args, run_state_ttl_days),
        "rename" => rename_memory(&store, &cwd, args),
        "delete" => delete_memory(&store, &cwd, args),
        "edit_index" => {
            let scope = arg_str(args, "scope")?;
            let content = arg_str(args, "content")?;
            let target_dir = scope_dir(&store, &cwd, &scope)?;
            let index_path = write_memory_index(&target_dir, &content)?;

            Ok(json!({
                "status": "ok",
                "path": index_path.display().to_string(),
            }))
        }
        "lint" => lint_memory(&store),
        _ => bail!("unknown memory action: {action}"),
    }
}

fn list_entries(store: &MemoryStore, include_stale: bool) -> Result<Vec<Value>> {
    let retention = if include_stale {
        Retention::all()
    } else {
        Retention::live()
    };
    let today = today_string();
    store
        .list_files(retention)?
        .iter()
        .map(|f| {
            let kind = &f.frontmatter.kind;
            let mut entry = json!({
                "name": f.frontmatter.name,
                "description": f.frontmatter.description,
                "type": kind.kind().name(),
                "char_len": f.char_len(),
                "path": f.path.display().to_string(),
            });
            if !kind.raw_kind().is_null() && !kind.is_known() {
                entry["raw_type"] = serde_json::to_value(kind.raw_kind())?;
            }
            if include_stale && f.is_stale(&today) {
                entry["stale"] = Value::Bool(true);
            }
            Ok(entry)
        })
        .collect()
}

fn write_memory(
    store: &MemoryStore,
    cwd: &Path,
    args: &Value,
    run_state_ttl_days: u32,
) -> Result<Value> {
    if let Some(report) = missing_write_args(args) {
        warn!("memory__write refused: {report}");
        bail!("{report}");
    }
    let name = arg_str(args, "name")?;
    let description = arg_str(args, "description")?;
    let content = arg_str(args, "content")?;
    let scope = arg_str(args, "scope")?;
    if content.trim_start().starts_with("---") {
        bail!(
            "`content` starts with a YAML front-matter block. {WRITE_ARGUMENT_CONTRACT} \
             Remove the block from `content` and pass those values as arguments instead."
        );
    }
    let kind = match args.get("type").and_then(Value::as_str) {
        Some(value) => MemoryKind::from_str(value)?.into(),
        None => KindField::default(),
    };
    let superseded_by = args
        .get("superseded_by")
        .and_then(Value::as_str)
        .map(String::from);
    let expires = args
        .get("expires")
        .and_then(Value::as_str)
        .map(String::from);
    let today = today_string();
    let expires = match (kind.kind(), expires) {
        (MemoryKind::RunState, None) => Some(date_after_days(run_state_ttl_days)?),
        (_, expires) => expires,
    };

    let target_dir = scope_dir(store, cwd, &scope)?;
    let path = target_dir.join(format!("{name}.md"));
    let previous = if path.exists() {
        MemoryFile::load(&path).ok()
    } else {
        None
    };
    let created = previous
        .as_ref()
        .and_then(|p| p.frontmatter.created.clone())
        .unwrap_or_else(|| today.clone());

    let file = MemoryFile {
        path,
        frontmatter: MemoryFrontmatter {
            name: name.clone(),
            description: Some(description.clone()),
            kind,
            created: Some(created),
            updated: Some(today),
            superseded_by,
            expires,
        },
        body: content,
    };
    file.save()?;

    let index_path = target_dir.join("MEMORY.md");
    let index_updated = ensure_index_entry(&index_path, &name, &description)?;

    Ok(json!({
        "status": "ok",
        "path": file.path.display().to_string(),
        "index_path": index_path.display().to_string(),
        "index_updated": index_updated,
        "replaced": previous.is_some(),
        "previous_description": previous.and_then(|p| p.frontmatter.description),
    }))
}

fn rename_memory(store: &MemoryStore, cwd: &Path, args: &Value) -> Result<Value> {
    let name = arg_str(args, "name")?;
    let new_name = arg_str(args, "new_name")?;
    let scope = arg_str(args, "scope")?;
    if new_name.is_empty()
        || !new_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "invalid new_name '{}': use a kebab-case slug (alphanumeric, hyphens, underscores)",
            new_name
        );
    }

    if name == new_name {
        bail!("new_name matches the current name");
    }

    let target_dir = scope_dir(store, cwd, &scope)?;
    let files = store.list_files(Retention::all())?;
    let file = files
        .iter()
        .find(|f| f.path.starts_with(&target_dir) && f.frontmatter.name == name)
        .ok_or_else(|| anyhow!("memory file '{}' not found in scope '{}'", name, scope))?
        .clone();

    if target_dir.join(format!("{new_name}.md")).exists()
        || files
            .iter()
            .any(|f| f.path.starts_with(&target_dir) && f.frontmatter.name == new_name)
    {
        bail!(
            "memory file '{}' already exists in scope '{}'",
            new_name,
            scope
        );
    }

    let needle = format!("[[{name}]]");
    let replacement = format!("[[{new_name}]]");

    let mut renamed = file.clone();
    renamed.path = target_dir.join(format!("{new_name}.md"));
    renamed.frontmatter.name = new_name.clone();
    renamed.frontmatter.updated = Some(today_string());
    renamed.body = renamed.body.replace(&needle, &replacement);
    renamed.save()?;
    fs::remove_file(&file.path).with_context(|| format!("remove {}", file.path.display()))?;

    let mut rewritten = Vec::new();
    for f in &files {
        if f.path == file.path || !f.body.contains(&needle) {
            continue;
        }
        let mut updated = f.clone();
        updated.body = updated.body.replace(&needle, &replacement);
        updated.save()?;
        rewritten.push(f.frontmatter.name.clone());
    }

    // Own-scope index: rewrite the wikilink, drop any leftover references to the
    // old name, and guarantee the new name is present.
    let index_path = target_dir.join("MEMORY.md");
    if let Ok(existing) = fs::read_to_string(&index_path)
        && existing.contains(&needle)
    {
        fs::write(&index_path, existing.replace(&needle, &replacement))?;
    }

    remove_index_entry(&index_path, &name)?;
    let description = renamed.frontmatter.description.clone().unwrap_or_default();
    ensure_index_entry(&index_path, &new_name, &description)?;

    // Other indexes (other scope's MEMORY.md): rewrite wikilinks only.
    for other_index in other_index_paths(store, &target_dir) {
        if let Ok(existing) = fs::read_to_string(&other_index)
            && existing.contains(&needle)
        {
            fs::write(&other_index, existing.replace(&needle, &replacement))?;
        }
    }

    Ok(json!({
        "status": "ok",
        "old_path": file.path.display().to_string(),
        "new_path": renamed.path.display().to_string(),
        "rewritten_references": rewritten,
    }))
}

fn delete_memory(store: &MemoryStore, cwd: &Path, args: &Value) -> Result<Value> {
    let name = arg_str(args, "name")?;
    let scope = arg_str(args, "scope")?;
    let target_dir = scope_dir(store, cwd, &scope)?;
    let files = store.list_files(Retention::all())?;
    let file = files
        .iter()
        .find(|f| f.path.starts_with(&target_dir) && f.frontmatter.name == name)
        .ok_or_else(|| anyhow!("memory file '{}' not found in scope '{}'", name, scope))?;
    let deleted_path = file.path.clone();
    fs::remove_file(&deleted_path).with_context(|| format!("delete {}", deleted_path.display()))?;

    let index_path = target_dir.join("MEMORY.md");
    let index_updated = remove_index_entry(&index_path, &name)?;

    let dangling: Vec<String> = files
        .iter()
        .filter(|f| f.path != deleted_path && extract_wikilinks(&f.body).iter().any(|l| l == &name))
        .map(|f| f.frontmatter.name.clone())
        .collect();

    Ok(json!({
        "status": "ok",
        "deleted_path": deleted_path.display().to_string(),
        "index_updated": index_updated,
        "dangling_references": dangling,
    }))
}

fn scope_dir(store: &MemoryStore, cwd: &Path, scope: &str) -> Result<PathBuf> {
    match scope {
        "global" => Ok(paths::global_memory_dir()),
        "workspace" => workspace_write_dir(store, cwd),
        other => bail!("unknown scope '{}': use 'global' or 'workspace'", other),
    }
}

fn date_after_days(days: u32) -> Result<String> {
    Local::now()
        .date_naive()
        .checked_add_days(Days::new(u64::from(days)))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .ok_or_else(|| {
            anyhow!(
                "memory_run_state_ttl_days {days} puts the expiry out of the supported date range"
            )
        })
}

fn other_index_paths(store: &MemoryStore, own_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let global_index = store.global_dir.join("MEMORY.md");
    if store.global_dir.as_path() != own_dir && global_index.exists() {
        out.push(global_index);
    }

    if let Some(ws) = &store.workspace {
        let index = ws.dir.join("MEMORY.md");
        if ws.dir.as_path() != own_dir && index.exists() {
            out.push(index);
        }
    }

    out
}

fn write_memory_index(target_dir: &Path, content: &str) -> Result<PathBuf> {
    fs::create_dir_all(target_dir)?;
    let index_path = target_dir.join("MEMORY.md");

    fs::write(&index_path, content)?;

    Ok(index_path)
}

fn ensure_index_entry(index_path: &Path, name: &str, description: &str) -> Result<bool> {
    let existing = fs::read_to_string(index_path).unwrap_or_default();
    if existing.lines().any(|l| catalogue_slug(l) == Some(name)) {
        return Ok(false);
    }

    let entry = format!("- [[{name}]]: {description}\n");
    let new_content = if existing.is_empty() {
        format!("# Memory Index\n\n{entry}")
    } else if existing.ends_with('\n') {
        format!("{existing}{entry}")
    } else {
        format!("{existing}\n{entry}")
    };

    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(index_path, new_content)?;

    Ok(true)
}

fn line_references(line: &str, name: &str) -> bool {
    let file_name = format!("{name}.md");
    line.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .any(|token| token == file_name || token.trim_matches('.') == name)
}

fn remove_index_entry(index_path: &Path, name: &str) -> Result<bool> {
    let Ok(existing) = fs::read_to_string(index_path) else {
        return Ok(false);
    };
    let kept: Vec<&str> = existing
        .lines()
        .filter(|line| !line_references(line, name))
        .collect();
    let mut new_content = kept.join("\n");

    if existing.ends_with('\n') && !new_content.is_empty() {
        new_content.push('\n');
    }

    if new_content == existing {
        return Ok(false);
    }

    fs::write(index_path, new_content)?;

    Ok(true)
}

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("{} is required", key))
}

const WRITE_ARGUMENT_CONTRACT: &str = "memory__write takes name, description, content and scope \
    as FUNCTION ARGUMENTS (plus optional type, superseded_by, expires); `content` is the markdown \
    body only. Coyote writes the YAML front matter itself, so front-matter lines inside `content` \
    do not count as arguments.";

const WRITE_REQUIRED_ARGS: [&str; 4] = ["name", "description", "content", "scope"];

fn missing_write_args(args: &Value) -> Option<String> {
    let missing: Vec<&str> = WRITE_REQUIRED_ARGS
        .iter()
        .copied()
        .filter(|key| args.get(key).and_then(Value::as_str).is_none())
        .collect();
    if missing.is_empty() {
        return None;
    }
    let received: Vec<String> = args
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| format!("{key} ({})", json_kind(value)))
                .collect()
        })
        .unwrap_or_default();
    let received = if received.is_empty() {
        "none".to_string()
    } else {
        received.join(", ")
    };
    let buried: Vec<String> = args
        .get("content")
        .and_then(Value::as_str)
        .map(|content| {
            missing
                .iter()
                .filter(|key| {
                    content
                        .lines()
                        .any(|line| line.trim_start().starts_with(&format!("{key}:")))
                })
                .map(|key| format!("`{key}:`"))
                .collect()
        })
        .unwrap_or_default();
    let buried = if buried.is_empty() {
        String::new()
    } else {
        format!(
            " `content` contains {} as a line of text; that does not count, move the value \
             into the function argument.",
            buried.join(" and ")
        )
    };
    Some(format!(
        "missing required argument(s): {}. Received arguments: {received}.{buried} \
         {WRITE_ARGUMENT_CONTRACT}",
        missing.join(", ")
    ))
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn find_file(store: &MemoryStore, name: &str) -> Result<Option<MemoryFile>> {
    Ok(store
        .list_files(Retention::all())?
        .into_iter()
        .find(|f| f.frontmatter.name == name))
}

fn workspace_write_dir(store: &MemoryStore, cwd: &Path) -> Result<PathBuf> {
    match &store.workspace {
        Some(ws) => Ok(ws.dir.clone()),
        None => match find_git_root(cwd) {
            Some(git_root) => bootstrap_workspace_memory(&git_root),
            None => bail!(
                "no workspace memory discoverable and not inside a git repository for auto-bootstrap. \
                 If you want workspace memory, run `coyote --init-memory workspace`."
            ),
        },
    }
}

fn workspace_label(w: &WorkspaceMemory) -> Value {
    json!({
        "root": w.workspace_root.display().to_string(),
        "dir": w.dir.display().to_string(),
    })
}

fn lint_memory(store: &MemoryStore) -> Result<Value> {
    let files = store.list_files(Retention::all())?;
    let names: HashSet<&str> = files.iter().map(|f| f.frontmatter.name.as_str()).collect();
    let today = today_string();

    let mut oversized = Vec::new();
    let mut broken_links = Vec::new();
    let mut stale = Vec::new();
    let mut unknown_type = Vec::new();
    let mut missing_expires_on_run_state = Vec::new();
    for f in &files {
        if f.char_len() > PER_FILE_SOFT_CAP {
            oversized.push(json!({"name": &f.frontmatter.name, "chars": f.char_len()}));
        }
        for link in extract_wikilinks(&f.body) {
            if !names.contains(link.as_str()) {
                broken_links.push(json!({"from": &f.frontmatter.name, "to": link}));
            }
        }
        if let Some(target) = &f.frontmatter.superseded_by {
            stale.push(json!({
                "name": &f.frontmatter.name,
                "reason": "superseded",
                "superseded_by": target,
                "target_exists": names.contains(target.as_str()),
            }));
        }

        if let Some(expires) = &f.frontmatter.expires
            && expires.as_str() < today.as_str()
        {
            stale.push(json!({
                "name": &f.frontmatter.name,
                "reason": "expired",
                "expires": expires,
            }));
        }

        let kind = &f.frontmatter.kind;
        if !kind.raw_kind().is_null() && !kind.is_known() {
            unknown_type.push(json!({
                "name": &f.frontmatter.name,
                "value": serde_json::to_value(kind.raw_kind())?,
            }));
        }

        if kind.kind() == MemoryKind::RunState && f.frontmatter.expires.is_none() {
            missing_expires_on_run_state.push(f.frontmatter.name.clone());
        }
    }

    let global_index = store.load_global_index()?.unwrap_or_default();
    let workspace_index = store
        .load_workspace_index()
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut orphans = Vec::new();
    let mut description_drift = Vec::new();

    for f in &files {
        let index = if f.path.starts_with(&store.global_dir) {
            &global_index
        } else {
            &workspace_index
        };

        let catalogued = index
            .lines()
            .any(|line| catalogue_slug(line) == Some(f.frontmatter.name.as_str()));
        if !catalogued {
            orphans.push(f.frontmatter.name.clone());
        } else if let (Some(index_desc), Some(file_desc)) = (
            index_description(index, &f.frontmatter.name),
            f.frontmatter.description.as_deref(),
        ) && index_desc != file_desc
        {
            description_drift.push(json!({
                "name": &f.frontmatter.name,
                "index_description": index_desc,
                "file_description": file_desc,
            }));
        }
    }

    Ok(json!({
        "total_files": files.len(),
        "oversized": oversized,
        "broken_wikilinks": broken_links,
        "orphans": orphans,
        "stale": stale,
        "description_drift": description_drift,
        "unknown_type": unknown_type,
        "missing_expires_on_run_state": missing_expires_on_run_state,
    }))
}

fn index_description(index: &str, name: &str) -> Option<String> {
    let marker = format!("[[{name}]]");
    index.lines().find_map(|line| {
        let pos = line.find(&marker)?;
        let rest = line[pos + marker.len()..].trim_start();
        let desc = rest.strip_prefix(':')?.trim();
        (!desc.is_empty()).then(|| desc.to_string())
    })
}

fn extract_wikilinks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;

    while i + 1 < bytes.len() {
        if bytes[i] == b'['
            && bytes[i + 1] == b'['
            && let Some(end_rel) = body[i + 2..].find("]]")
        {
            out.push(body[i + 2..i + 2 + end_rel].to_string());
            i = i + 2 + end_rel + 2;
            continue;
        }
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::memory::discover_workspace_memory;
    use std::fs;
    use std::time;

    fn temp_root(label: &str) -> PathBuf {
        let unique = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-function-memory-{label}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn extract_wikilinks_finds_all_pairs() {
        let body = "see [[alpha]] and [[bravo]] but not [single] or [[unclosed";

        assert_eq!(
            extract_wikilinks(body),
            vec!["alpha".to_string(), "bravo".to_string()]
        );
    }

    #[test]
    fn extract_wikilinks_handles_empty_and_no_links() {
        assert!(extract_wikilinks("").is_empty());
        assert!(extract_wikilinks("nothing here").is_empty());
    }

    #[test]
    fn ensure_index_entry_appends_when_missing() {
        let root = temp_root("index_append");
        let index = root.join("MEMORY.md");
        fs::write(&index, "# Memory Index\n\n- [[existing]]: already here\n").unwrap();

        let updated = ensure_index_entry(&index, "new_one", "newly added").unwrap();
        assert!(updated);
        let content = fs::read_to_string(&index).unwrap();
        assert!(content.contains("- [[existing]]: already here"));
        assert!(content.contains("- [[new_one]]: newly added"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_index_entry_skips_when_referenced() {
        let root = temp_root("index_skip");
        let index = root.join("MEMORY.md");
        let original = "# Memory Index\n\n- [[existing]]: already here\n";
        fs::write(&index, original).unwrap();

        let updated = ensure_index_entry(&index, "existing", "different description").unwrap();
        assert!(!updated);
        assert_eq!(fs::read_to_string(&index).unwrap(), original);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_index_entry_creates_index_when_absent() {
        let root = temp_root("index_create");
        let index = root.join("memory").join("MEMORY.md");

        let updated = ensure_index_entry(&index, "first", "first ever").unwrap();
        assert!(updated);
        let content = fs::read_to_string(&index).unwrap();
        assert!(content.starts_with("# Memory Index"));
        assert!(content.contains("- [[first]]: first ever"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_write_dir_returns_structured_dir_directly() {
        let root = temp_root("ws_structured");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), "idx").unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let dir = workspace_write_dir(&store, &workspace).unwrap();
        assert_eq!(dir, structured);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_write_dir_treats_root_instructions_file_as_no_memory() {
        let root = temp_root("ws_instructions_only");
        let workspace = root.join("ws");
        fs::create_dir_all(workspace.join(".git")).unwrap();
        fs::write(workspace.join("COYOTE.md"), "instructions, not memory").unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };
        assert!(store.workspace.is_none(), "COYOTE.md must not be memory");

        let dir = workspace_write_dir(&store, &workspace).unwrap();
        assert_eq!(dir, workspace.join(".coyote").join("memory"));
        assert!(
            dir.join("MEMORY.md").exists(),
            "bootstrap must create index"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_write_dir_errors_when_no_workspace_and_no_git() {
        let root = temp_root("ws_none");
        let bare = root.join("nowhere");
        fs::create_dir_all(&bare).unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&bare),
        };

        let err = workspace_write_dir(&store, &bare).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no workspace memory discoverable"));
        assert!(msg.contains("coyote --init-memory workspace"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_write_dir_auto_bootstraps_inside_git_repo() {
        let root = temp_root("ws_bootstrap");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&nested),
        };
        assert!(store.workspace.is_none());

        let dir = workspace_write_dir(&store, &nested).unwrap();
        assert_eq!(dir, repo.join(".coyote").join("memory"));
        assert!(dir.join("MEMORY.md").exists());
        let gi = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert!(gi.contains(".coyote/memory/"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_file_returns_matching_file() {
        let root = temp_root("find_file");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), "idx").unwrap();
        fs::write(
            structured.join("target.md"),
            "---\nname: target\n---\nfound me\n",
        )
        .unwrap();
        fs::write(
            structured.join("other.md"),
            "---\nname: other\n---\nignored\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let hit = find_file(&store, "target").unwrap();
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().body.trim(), "found me");

        let miss = find_file(&store, "nope").unwrap();
        assert!(miss.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_index_creates_dir_and_writes_content() {
        let root = temp_root("write_index_create");
        let target = root.join("nested").join(".coyote").join("memory");

        let path =
            write_memory_index(&target, "# Workspace Memory Index\n\n- [[foo]]: hello\n").unwrap();

        assert_eq!(path, target.join("MEMORY.md"));
        assert!(path.exists());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Workspace Memory Index\n\n- [[foo]]: hello\n"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_index_replaces_existing_content() {
        let root = temp_root("write_index_replace");
        fs::create_dir_all(&root).unwrap();
        let index = root.join("MEMORY.md");
        fs::write(&index, "# Old\n\n- [[stale]]: gone\n").unwrap();

        let path = write_memory_index(&root, "# New\n").unwrap();

        assert_eq!(path, index);
        assert_eq!(fs::read_to_string(&path).unwrap(), "# New\n");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lint_flags_orphans_broken_links_and_oversized() {
        let root = temp_root("lint");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();

        fs::write(structured.join("MEMORY.md"), "- [[referenced]]: r\n").unwrap();
        fs::write(
            structured.join("referenced.md"),
            "---\nname: referenced\n---\nlinks to [[missing]] and [[also_missing]]\n",
        )
        .unwrap();
        fs::write(
            structured.join("orphan.md"),
            "---\nname: orphan\n---\nnot in the index\n",
        )
        .unwrap();
        let huge_body = "x".repeat(PER_FILE_SOFT_CAP + 100);
        fs::write(
            structured.join("huge.md"),
            format!("---\nname: huge\n---\n{huge_body}\n"),
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("nonexistent_global"),
            workspace: discover_workspace_memory(&workspace),
        };

        let report = lint_memory(&store).unwrap();
        assert_eq!(report["total_files"], 3);

        let orphans = report["orphans"].as_array().unwrap();
        let orphan_names: Vec<&str> = orphans.iter().filter_map(|v| v.as_str()).collect();
        assert!(orphan_names.contains(&"orphan"));
        assert!(orphan_names.contains(&"huge"));
        assert!(!orphan_names.contains(&"referenced"));

        let broken = report["broken_wikilinks"].as_array().unwrap();
        let broken_targets: Vec<&str> = broken.iter().filter_map(|v| v["to"].as_str()).collect();
        assert!(broken_targets.contains(&"missing"));
        assert!(broken_targets.contains(&"also_missing"));

        let oversized = report["oversized"].as_array().unwrap();
        let oversized_names: Vec<&str> = oversized
            .iter()
            .filter_map(|v| v["name"].as_str())
            .collect();
        assert_eq!(oversized_names, vec!["huge"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn line_references_requires_exact_token_match() {
        assert!(line_references("- [[auth]]: description", "auth"));
        assert!(line_references("- auth.md is here", "auth"));
        assert!(line_references("- referenced", "referenced"));
        assert!(line_references("see auth.", "auth"));
        assert!(!line_references("- [[auth-flow]]: description", "auth"));
        assert!(!line_references("- oauth.md legacy", "auth"));
        assert!(!line_references("- preauth notes", "auth"));
    }

    #[test]
    fn remove_index_entry_drops_only_matching_lines() {
        let root = temp_root("index_remove");
        let index = root.join("MEMORY.md");
        fs::write(
            &index,
            "# Memory Index\n\n- [[keep]]: stays\n- [[gone]]: removed\n",
        )
        .unwrap();

        assert!(remove_index_entry(&index, "gone").unwrap());
        let content = fs::read_to_string(&index).unwrap();
        assert!(content.contains("[[keep]]"));
        assert!(!content.contains("[[gone]]"));

        assert!(!remove_index_entry(&index, "gone").unwrap());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lint_checks_orphans_against_own_scope_index() {
        let root = temp_root("lint_scopes");
        let global = root.join("global");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join("MEMORY.md"), "- [[global-note]]: g\n").unwrap();
        fs::write(
            global.join("global-note.md"),
            "---\nname: global-note\n---\ng\n",
        )
        .unwrap();

        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), "- [[ws-note]]: w\n").unwrap();
        fs::write(
            structured.join("ws-note.md"),
            "---\nname: ws-note\n---\nw\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: global,
            workspace: discover_workspace_memory(&workspace),
        };

        let report = lint_memory(&store).unwrap();
        assert!(
            report["orphans"].as_array().unwrap().is_empty(),
            "expected no orphans, got: {report}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lint_flags_stale_and_description_drift() {
        let root = temp_root("lint_stale");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(
            structured.join("MEMORY.md"),
            "- [[old-plan]]: old\n- [[bygone]]: e\n- [[drifted]]: index says this\n",
        )
        .unwrap();
        fs::write(
            structured.join("old-plan.md"),
            "---\nname: old-plan\nsuperseded_by: new-plan\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("bygone.md"),
            "---\nname: bygone\nexpires: 2000-01-01\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("drifted.md"),
            "---\nname: drifted\ndescription: file says that\n---\nx\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("nonexistent_global"),
            workspace: discover_workspace_memory(&workspace),
        };

        let report = lint_memory(&store).unwrap();
        let stale = report["stale"].as_array().unwrap();
        let reasons: Vec<(&str, &str)> = stale
            .iter()
            .map(|v| (v["name"].as_str().unwrap(), v["reason"].as_str().unwrap()))
            .collect();
        assert!(reasons.contains(&("old-plan", "superseded")));
        assert!(reasons.contains(&("bygone", "expired")));
        let superseded = stale.iter().find(|v| v["name"] == "old-plan").unwrap();
        assert_eq!(superseded["target_exists"], false);

        let drift = report["description_drift"].as_array().unwrap();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0]["name"], "drifted");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_memory_removes_file_index_entry_and_reports_dangling() {
        let root = temp_root("delete");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(
            structured.join("MEMORY.md"),
            "# Memory Index\n\n- [[doomed]]: bye\n- [[linker]]: links\n",
        )
        .unwrap();
        fs::write(
            structured.join("doomed.md"),
            "---\nname: doomed\n---\nbye\n",
        )
        .unwrap();
        fs::write(
            structured.join("linker.md"),
            "---\nname: linker\n---\nsee [[doomed]]\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let args = json!({"name": "doomed", "scope": "workspace"});
        let result = delete_memory(&store, &workspace, &args).unwrap();

        assert_eq!(result["status"], "ok");
        assert_eq!(result["index_updated"], true);
        assert!(!structured.join("doomed.md").exists());
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(!index.contains("doomed"));
        assert!(index.contains("[[linker]]"));
        assert_eq!(
            result["dangling_references"].as_array().unwrap(),
            &vec![json!("linker")]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_memory_moves_file_and_rewrites_references() {
        let root = temp_root("rename");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(
            structured.join("MEMORY.md"),
            "# Memory Index\n\n- [[old-name]]: the plan\n- [[linker]]: links\n",
        )
        .unwrap();
        fs::write(
            structured.join("old-name.md"),
            "---\nname: old-name\ndescription: the plan\n---\nself link [[old-name]]\n",
        )
        .unwrap();
        fs::write(
            structured.join("linker.md"),
            "---\nname: linker\n---\nsee [[old-name]] and [[old-name-extended]]\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let args = json!({"name": "old-name", "new_name": "new-name", "scope": "workspace"});
        let result = rename_memory(&store, &workspace, &args).unwrap();

        assert_eq!(result["status"], "ok");
        assert!(!structured.join("old-name.md").exists());
        let renamed = MemoryFile::load(&structured.join("new-name.md")).unwrap();
        assert_eq!(renamed.frontmatter.name, "new-name");
        assert!(renamed.body.contains("[[new-name]]"));

        let linker = fs::read_to_string(structured.join("linker.md")).unwrap();
        assert!(linker.contains("[[new-name]]"));
        assert!(
            linker.contains("[[old-name-extended]]"),
            "unrelated links must be untouched: {linker}"
        );

        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(index.contains("- [[new-name]]: the plan"));
        assert!(!index.contains("[[old-name]]"));
        assert!(index.contains("[[linker]]"));

        assert_eq!(
            result["rewritten_references"].as_array().unwrap(),
            &vec![json!("linker")]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_memory_rejects_collisions_and_bad_slugs() {
        let root = temp_root("rename_guard");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), "- [[a]]: a\n- [[b]]: b\n").unwrap();
        fs::write(structured.join("a.md"), "---\nname: a\n---\nx\n").unwrap();
        fs::write(structured.join("b.md"), "---\nname: b\n---\nx\n").unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let collision = json!({"name": "a", "new_name": "b", "scope": "workspace"});
        let err = rename_memory(&store, &workspace, &collision).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        let bad_slug = json!({"name": "a", "new_name": "bad name!", "scope": "workspace"});
        let err = rename_memory(&store, &workspace, &bad_slug).unwrap_err();
        assert!(err.to_string().contains("invalid new_name"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_stamps_timestamps_and_reports_replacement() {
        let root = temp_root("write_stamps");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), "# Memory Index\n").unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let first = json!({
            "name": "fact",
            "description": "first version",
            "content": "body v1",
            "scope": "workspace",
            "expires": "2099-01-01",
        });
        let before = today_string();
        let result = write_memory(
            &store,
            &workspace,
            &first,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap();
        let after = today_string();
        assert_eq!(result["replaced"], false);
        assert_eq!(result["previous_description"], Value::Null);

        let saved = MemoryFile::load(&structured.join("fact.md")).unwrap();
        let created = saved.frontmatter.created.clone().expect("created stamped");
        assert!(
            created == before || created == after,
            "created '{created}' should be stamped with today's date"
        );
        assert_eq!(saved.frontmatter.updated, Some(created.clone()));
        assert_eq!(saved.frontmatter.expires.as_deref(), Some("2099-01-01"));
        assert_eq!(saved.frontmatter.superseded_by, None);

        let second = json!({
            "name": "fact",
            "description": "second version",
            "content": "body v2",
            "scope": "workspace",
        });
        let result = write_memory(
            &store,
            &workspace,
            &second,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap();
        assert_eq!(result["replaced"], true);
        assert_eq!(result["previous_description"], "first version");

        let saved = MemoryFile::load(&structured.join("fact.md")).unwrap();
        assert_eq!(
            saved.frontmatter.created,
            Some(created),
            "creation date must be preserved across overwrites"
        );
        assert!(saved.frontmatter.updated.is_some());
        assert_eq!(saved.frontmatter.expires, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_teaches_the_argument_contract_when_metadata_is_missing_or_in_front_matter() {
        let root = temp_root("write_contract");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), "# Memory Index\n").unwrap();
        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let missing_description = json!({
            "name": "fact",
            "content": "body",
            "scope": "workspace",
        });
        let err = write_memory(
            &store,
            &workspace,
            &missing_description,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("missing required argument(s): description."),
            "{text}"
        );
        assert!(
            text.contains("Received arguments: name (string), content (string), scope (string)."),
            "{text}"
        );
        assert!(!text.contains("as a line of text"), "{text}");
        assert!(text.contains("front matter"), "{text}");
        assert!(
            text.contains("name, description, content and scope"),
            "{text}"
        );

        let scope_buried_in_content = json!({
            "name": "fact",
            "description": "a fact",
            "content": "# Fact\n\nscope: workspace\ntype: reference\n\nbody",
            "expires": 20991231,
        });
        let err = write_memory(
            &store,
            &workspace,
            &scope_buried_in_content,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("missing required argument(s): scope."),
            "{text}"
        );
        assert!(text.contains("expires (number)"), "{text}");
        assert!(
            text.contains("`content` contains `scope:` as a line of text"),
            "{text}"
        );
        assert!(!text.contains("`type:`"), "type is optional: {text}");

        let everything_missing = json!("not even an object");
        let err = write_memory(
            &store,
            &workspace,
            &everything_missing,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with(
                "missing required argument(s): name, description, content, scope. \
                 Received arguments: none."
            ),
            "{text}"
        );

        let front_matter_in_content = json!({
            "name": "fact",
            "description": "a fact",
            "content": "---\nname: fact\ndescription: a fact\nscope: workspace\n---\n\nbody",
            "scope": "workspace",
        });
        let err = write_memory(
            &store,
            &workspace,
            &front_matter_in_content,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("`content` starts with a YAML front-matter block."),
            "{text}"
        );
        assert!(text.contains("pass those values as arguments"), "{text}");
        assert!(
            !structured.join("fact.md").exists(),
            "refusal must write nothing"
        );

        let body_only = json!({
            "name": "fact",
            "description": "a fact",
            "content": "# Fact\n\nA line with --- inside it is fine.",
            "scope": "workspace",
        });
        write_memory(
            &store,
            &workspace,
            &body_only,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap();
        let saved = MemoryFile::load(&structured.join("fact.md")).unwrap();
        assert_eq!(saved.body, "# Fact\n\nA line with --- inside it is fine.");

        let _ = fs::remove_dir_all(&root);
    }

    fn workspace_store(root: &Path, index: &str) -> (MemoryStore, PathBuf, PathBuf) {
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join("MEMORY.md"), index).unwrap();
        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };
        (store, workspace, structured)
    }

    #[test]
    fn lint_orphan_check_only_counts_catalogue_lines() {
        let root = temp_root("lint_strict_orphans");
        let (store, _, structured) = workspace_store(
            &root,
            "# Index\n\n## FACTS\n- Always run tests before [[cited]] lands\n\n## FILES\n- [[catalogued]]: proper entry\n",
        );
        fs::write(structured.join("cited.md"), "---\nname: cited\n---\nx\n").unwrap();
        fs::write(
            structured.join("catalogued.md"),
            "---\nname: catalogued\n---\nx\n",
        )
        .unwrap();

        let report = lint_memory(&store).unwrap();
        let orphans: Vec<&str> = report["orphans"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(orphans, vec!["cited"], "{report}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lint_reports_unknown_types_and_run_state_without_expires() {
        let root = temp_root("lint_types");
        let (store, _, structured) = workspace_store(
            &root,
            "- [[untyped]]: u\n- [[gotcha]]: g\n- [[scratch]]: s\n- [[scratch-dated]]: d\n",
        );
        fs::write(
            structured.join("untyped.md"),
            "---\nname: untyped\ntype: null\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("gotcha.md"),
            "---\nname: gotcha\ntype: gotcha\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("scratch.md"),
            "---\nname: scratch\ntype: run-state\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("scratch-dated.md"),
            "---\nname: scratch-dated\ntype: run-state\nexpires: 2999-01-01\n---\nx\n",
        )
        .unwrap();

        let report = lint_memory(&store).unwrap();
        assert_eq!(
            report["unknown_type"],
            json!([{"name": "gotcha", "value": "gotcha"}]),
            "{report}"
        );
        assert_eq!(
            report["missing_expires_on_run_state"],
            json!(["scratch"]),
            "{report}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_files_stay_readable_renamable_and_deletable() {
        let root = temp_root("stale_ops");
        let (store, workspace, structured) =
            workspace_store(&root, "- [[bygone]]: b\n- [[replaced]]: r\n");
        fs::write(
            structured.join("bygone.md"),
            "---\nname: bygone\nexpires: 2000-01-01\n---\nold\n",
        )
        .unwrap();
        fs::write(
            structured.join("replaced.md"),
            "---\nname: replaced\nsuperseded_by: bygone\n---\nolder\n",
        )
        .unwrap();

        assert!(store.list_files(Retention::live()).unwrap().is_empty());
        assert_eq!(
            find_file(&store, "bygone").unwrap().unwrap().body.trim(),
            "old"
        );

        let rename = json!({"name": "bygone", "new_name": "bygone-v1", "scope": "workspace"});
        rename_memory(&store, &workspace, &rename).unwrap();
        assert!(structured.join("bygone-v1.md").exists());

        let delete = json!({"name": "replaced", "scope": "workspace"});
        delete_memory(&store, &workspace, &delete).unwrap();
        assert!(!structured.join("replaced.md").exists());

        let report = lint_memory(&store).unwrap();
        let stale: Vec<&str> = report["stale"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["name"].as_str())
            .collect();
        assert_eq!(stale, vec!["bygone-v1"], "{report}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_defaults_expires_for_run_state_only() {
        let root = temp_root("write_run_state");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");

        let scratch = json!({
            "name": "scratch",
            "description": "in-flight state",
            "content": "step 3 of 5",
            "scope": "workspace",
            "type": "run-state",
        });
        write_memory(&store, &workspace, &scratch, 3).unwrap();
        let saved = MemoryFile::load(&structured.join("scratch.md")).unwrap();
        assert_eq!(saved.frontmatter.kind.kind(), MemoryKind::RunState);
        assert_eq!(
            saved.frontmatter.expires.as_deref(),
            Some(date_after_days(3).unwrap().as_str())
        );
        assert!(saved.frontmatter.expires.as_deref() > Some(today_string().as_str()));
        let raw = fs::read_to_string(structured.join("scratch.md")).unwrap();
        assert!(raw.contains("\ntype: run-state\n"), "{raw}");

        let pinned = json!({
            "name": "pinned",
            "description": "explicit expiry wins",
            "content": "x",
            "scope": "workspace",
            "type": "run-state",
            "expires": "2999-01-01",
        });
        write_memory(&store, &workspace, &pinned, 3).unwrap();
        let saved = MemoryFile::load(&structured.join("pinned.md")).unwrap();
        assert_eq!(saved.frontmatter.expires.as_deref(), Some("2999-01-01"));

        let durable = json!({
            "name": "durable",
            "description": "no type",
            "content": "x",
            "scope": "workspace",
        });
        write_memory(&store, &workspace, &durable, 3).unwrap();
        let saved = MemoryFile::load(&structured.join("durable.md")).unwrap();
        assert_eq!(saved.frontmatter.expires, None);
        assert!(saved.frontmatter.kind.raw_kind().is_null());
        let raw = fs::read_to_string(structured.join("durable.md")).unwrap();
        assert!(raw.contains("\ntype: null\n"), "{raw}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_rejects_unknown_type_naming_the_valid_ones() {
        let root = temp_root("write_bad_type");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");

        let bogus = json!({
            "name": "fact",
            "description": "a fact",
            "content": "x",
            "scope": "workspace",
            "type": "bogus",
        });
        let err = write_memory(
            &store,
            &workspace,
            &bogus,
            DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Invalid value 'bogus'"), "{text}");
        for name in MemoryKind::NAMES {
            assert!(text.contains(name), "{text} should list {name}");
        }
        assert!(!structured.join("fact.md").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn date_after_days_rejects_ttl_past_the_date_range() {
        assert!(date_after_days(3).unwrap().as_str() > today_string().as_str());

        let err = date_after_days(u32::MAX).unwrap_err().to_string();
        assert!(
            err.contains("memory_run_state_ttl_days 4294967295")
                && err.contains("out of the supported date range"),
            "{err}"
        );
    }

    #[test]
    fn write_declaration_lists_every_memory_kind() {
        let declarations = memory_function_declarations();
        let write = declarations
            .iter()
            .find(|d| d.name == "memory__write")
            .unwrap();
        let description = write.parameters.properties.as_ref().unwrap()["type"]
            .description
            .as_deref()
            .unwrap();

        for name in MemoryKind::NAMES {
            assert!(description.contains(name), "{description}");
        }
        assert!(
            description.contains("user | feedback | project | reference | run-state"),
            "{description}"
        );
    }

    #[test]
    fn write_memory_catalogues_a_slug_only_cited_mid_sentence() {
        let root = temp_root("write_cited_slug");
        let (store, workspace, structured) = workspace_store(
            &root,
            "# Index\n\n## FACTS\n- Always run tests before [[cited]] lands\n",
        );

        let args = json!({
            "name": "cited",
            "description": "what cited means",
            "content": "x",
            "scope": "workspace",
        });
        let result = write_memory(&store, &workspace, &args, 3).unwrap();
        assert_eq!(result["index_updated"], true);
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(index.contains("- Always run tests before [[cited]] lands\n"));
        assert!(index.contains("- [[cited]]: what cited means\n"), "{index}");

        let report = lint_memory(&store).unwrap();
        assert!(report["orphans"].as_array().unwrap().is_empty(), "{report}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_entries_reports_types_and_hides_stale_by_default() {
        let root = temp_root("list_entries");
        let (store, _, structured) = workspace_store(
            &root,
            "- [[known]]: k\n- [[odd]]: o\n- [[untyped]]: u\n- [[bygone]]: b\n",
        );
        fs::write(
            structured.join("known.md"),
            "---\nname: known\ntype: user\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("odd.md"),
            "---\nname: odd\ntype: gotcha\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("untyped.md"),
            "---\nname: untyped\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("bygone.md"),
            "---\nname: bygone\nexpires: 2000-01-01\n---\nx\n",
        )
        .unwrap();

        let by_name = |entries: Vec<Value>| -> IndexMap<String, Value> {
            entries
                .into_iter()
                .map(|e| (e["name"].as_str().unwrap().to_string(), e))
                .collect()
        };

        let live = by_name(list_entries(&store, false).unwrap());
        assert_eq!(live.len(), 3, "{live:?}");
        assert!(!live.contains_key("bygone"));
        assert_eq!(live["known"]["type"], "user");
        assert!(live["known"].get("raw_type").is_none());
        assert_eq!(live["odd"]["type"], "reference");
        assert_eq!(live["odd"]["raw_type"], "gotcha");
        assert_eq!(live["untyped"]["type"], "reference");
        assert!(live["untyped"].get("raw_type").is_none());
        assert!(live.values().all(|e| e.get("stale").is_none()));

        let all = by_name(list_entries(&store, true).unwrap());
        assert_eq!(all.len(), 4, "{all:?}");
        assert_eq!(all["bygone"]["stale"], true);
        assert!(all["known"].get("stale").is_none());

        let _ = fs::remove_dir_all(&root);
    }
}
