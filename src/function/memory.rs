use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{env, fs};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Days, Local, NaiveDate};
use indexmap::IndexMap;
use log::warn;
use serde_json::{Value, json};

use super::{FunctionDeclaration, JsonSchema};
use crate::config::memory::{
    DEFAULT_MEMORY_RUN_STATE_TTL_DAYS, KindField, MEMORY_DESCRIPTION_CAP, MEMORY_FILE_HARD_CAP,
    MemoryFile, MemoryFrontmatter, MemoryKind, MemoryStore, Retention, WorkspaceMemory,
    bootstrap_workspace_memory, catalogue_entry, catalogue_slug, find_git_root, today_string,
};
use crate::config::paths;
use crate::config::{RequestContext, RoundChains};

pub const MEMORY_FUNCTION_PREFIX: &str = "memory__";

const PER_FILE_SOFT_CAP: usize = 2_000;
const MEMORY_SLUG_MAX_LEN: usize = 64;

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
                 Prefer updating an existing slug over minting a new one; a `-rN` name supersedes the \
                 previous round under the default policy. \
                 Coyote writes the YAML front matter and the MEMORY.md index entry itself from these \
                 arguments, so never put `name:` / `description:` / `scope:` / `type:` lines inside \
                 `content`. Writing `<base>-r<N>` applies the `memory_round_chains` config key to the \
                 older `<base>-r<M>` files in the same scope (supersede marks them superseded_by, keep \
                 leaves them alone). Either way, a round written below an existing one keeps \
                 superseded_by pointing at the round above."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "name".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(format!(
                                "REQUIRED. Short kebab-case slug for the file (no extension): {}. \
                                 A function argument, not a front-matter line.",
                                slug_grammar()
                            )),
                            ..Default::default()
                        },
                    ),
                    (
                        "description".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(format!(
                                "REQUIRED. One plain-text line (no newline, at most \
                                 {MEMORY_DESCRIPTION_CAP} chars) for the MEMORY.md index entry. \
                                 A function argument, not a front-matter line inside `content`."
                            )),
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
                                 A superseded memory drops out of prompt injection and the default \
                                 `memory__list` (it stays readable, and `memory__list` with \
                                 `include_stale` still shows it); `memory__lint` flags it for \
                                 cleanup. Omitting this on overwrite clears any previous value."
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
                                 An expired memory drops out of prompt injection and the default \
                                 `memory__list` (it stays readable, and `memory__list` with \
                                 `include_stale` still shows it); `memory__lint` flags it for \
                                 cleanup. Omitting this on overwrite clears any previous value \
                                 (a `run-state` memory then gets today + memory_run_state_ttl_days)."
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
                            description: Some(format!(
                                "New kebab-case slug for the file (no extension): {}.",
                                slug_grammar()
                            )),
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
    let round_chains = ctx.app.config.memory_round_chains;

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
        "write" => write_memory(&store, &cwd, args, run_state_ttl_days, round_chains),
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
    round_chains: RoundChains,
) -> Result<Value> {
    if let Some(report) = missing_write_args(args) {
        warn!("memory__write refused: {report}");
        bail!("{report}");
    }
    let name = arg_str(args, "name")?;
    let description = arg_str(args, "description")?.trim().to_string();
    let content = arg_str(args, "content")?;
    let scope = arg_str(args, "scope")?;
    if content.trim_start().starts_with("---") {
        bail!(
            "`content` starts with a YAML front-matter block. {WRITE_ARGUMENT_CONTRACT} \
             Remove the block from `content` and pass those values as arguments instead."
        );
    }
    validate_slug("name", &name)?;
    let kind = match args.get("type").and_then(Value::as_str) {
        Some(value) => MemoryKind::from_str(value)?.into(),
        None => KindField::default(),
    };
    if description.contains(['\r', '\n']) {
        bail!("description must be one line (no newline or carriage return)");
    }
    let description_chars = description.chars().count();
    if description_chars > MEMORY_DESCRIPTION_CAP {
        bail!("description is {description_chars} chars, the cap is {MEMORY_DESCRIPTION_CAP}");
    }
    let content_chars = content.chars().count();
    if content_chars > MEMORY_FILE_HARD_CAP {
        bail!(
            "content is {content_chars} chars, the cap is {MEMORY_FILE_HARD_CAP} \
             (split the topic across linked files)"
        );
    }
    let expires = args
        .get("expires")
        .and_then(Value::as_str)
        .map(String::from);
    if let Some(value) = expires.as_deref()
        && !is_iso_date(value)
    {
        bail!("Invalid `expires` '{value}', expected an ISO date (YYYY-MM-DD)");
    }
    let today = today_string();
    let expires = match (kind.kind(), expires) {
        (MemoryKind::RunState, None) => Some(date_after_days(run_state_ttl_days)?),
        (_, expires) => expires,
    };

    let target_dir = scope_dir(store, cwd, &scope)?;
    let chain = round_chain_name(&name);
    let (older, newer): (Vec<RoundSibling>, Vec<RoundSibling>) = match chain {
        Some((base, round)) => scan_round_siblings(&target_dir, base)?
            .into_iter()
            .filter(|s| s.round != round)
            .partition(|s| s.round < round),
        None => (Vec::new(), Vec::new()),
    };
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
    // Rewriting an older round must keep it superseded: without an argument,
    // reuse the mark it had, else point at the next round up.
    let superseded_by = match args.get("superseded_by").and_then(Value::as_str) {
        Some(value) => Some(value.to_string()),
        None => newer.first().map(|next| {
            previous
                .as_ref()
                .and_then(|p| p.frontmatter.superseded_by.clone())
                .unwrap_or_else(|| next.name.clone())
        }),
    };

    let file = MemoryFile {
        path,
        frontmatter: MemoryFrontmatter {
            name: name.clone(),
            description: Some(description.clone()),
            kind,
            created: Some(created),
            updated: Some(today.clone()),
            superseded_by,
            expires,
        },
        body: content,
    };
    file.save()?;

    let index_path = target_dir.join("MEMORY.md");
    let index_updated = ensure_index_entry(&index_path, &name, &description)?;

    let mut result = json!({
        "status": "ok",
        "path": file.path.display().to_string(),
        "index_path": index_path.display().to_string(),
        "index_updated": index_updated,
        "replaced": previous.is_some(),
        "previous_description": previous.and_then(|p| p.frontmatter.description),
    });
    if let Some((base, round)) = chain {
        result["siblings"] =
            Value::Array(settle_round_siblings(older, &name, &today, round_chains));
        let newer_rounds: Vec<&str> = newer.iter().map(|s| s.name.as_str()).collect();
        result["round_chain"] = json!({"base": base, "round": round, "newer_rounds": newer_rounds});
    }

    Ok(result)
}

fn validate_slug(what: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let well_formed = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.len() <= MEMORY_SLUG_MAX_LEN;
    if !well_formed {
        bail!(
            "invalid {what} '{name}': use a kebab-case slug of lowercase letters, digits, \
             hyphens and underscores (max {MEMORY_SLUG_MAX_LEN} chars)"
        );
    }
    Ok(())
}

/// The slug rule as the tool schema states it, shared by every slug argument.
fn slug_grammar() -> String {
    format!(
        "lowercase letters, digits, hyphens and underscores, starting with a letter or digit, \
         at most {MEMORY_SLUG_MAX_LEN} chars"
    )
}

/// Splits `<base>-r<N>` into `(base, N)`; any other shape is not a round name.
pub(crate) fn round_chain_name(name: &str) -> Option<(&str, u32)> {
    let (base, digits) = name.rsplit_once("-r")?;
    if base.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((base, digits.parse().ok()?))
}

struct RoundSibling {
    round: u32,
    name: String,
    path: PathBuf,
}

/// Every `<base>-r<M>` file in `dir`, in round order. Only this directory is
/// scanned: a chain never crosses scopes, and a symlink is never followed, so
/// settling a round cannot rewrite a file outside the scope. A directory that
/// does not exist yet simply has no rounds.
fn scan_round_siblings(dir: &Path, base: &str) -> Result<Vec<RoundSibling>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("read {}", dir.display()))?
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match path.symlink_metadata() {
            Ok(meta) if meta.is_symlink() => continue,
            Ok(_) => {}
            // The entry vanished between read_dir and stat.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some((sibling_base, round)) = round_chain_name(stem)
            && sibling_base == base
        {
            found.push(RoundSibling {
                round,
                name: stem.to_string(),
                path,
            });
        }
    }
    found.sort_by(|a, b| a.round.cmp(&b.round).then_with(|| a.name.cmp(&b.name)));
    Ok(found)
}

/// One row per older round, after any supersede update. A sibling that cannot
/// be read or saved is reported with its on-disk mark and an `error` instead
/// of failing the write that already landed.
fn settle_round_siblings(
    older: Vec<RoundSibling>,
    successor: &str,
    today: &str,
    round_chains: RoundChains,
) -> Vec<Value> {
    older
        .into_iter()
        .map(|sibling| {
            let mut mark = None;
            let outcome = settle_round_sibling(&sibling, successor, today, round_chains, &mut mark);
            let mut row = json!({"name": sibling.name, "superseded_by": mark});
            if let Err(e) = outcome {
                warn!("skip memory round {}: {e:#}", sibling.path.display());
                row["error"] = json!(format!("{e:#}"));
            }
            row
        })
        .collect()
}

/// `mark` ends as the sibling's `superseded_by` on disk, whether or not this
/// write changed it.
fn settle_round_sibling(
    sibling: &RoundSibling,
    successor: &str,
    today: &str,
    round_chains: RoundChains,
    mark: &mut Option<String>,
) -> Result<()> {
    let mut file = MemoryFile::load(&sibling.path)?;
    *mark = file.frontmatter.superseded_by.clone();
    if file.frontmatter.name != sibling.name {
        bail!(
            "front matter name '{}' does not match the file name",
            file.frontmatter.name
        );
    }
    if round_chains == RoundChains::Keep || mark.is_some() {
        return Ok(());
    }
    file.frontmatter.superseded_by = Some(successor.to_string());
    file.frontmatter.updated = Some(today.to_string());
    if !file.frontmatter.kind.is_known() {
        file.frontmatter.kind = MemoryKind::RunState.into();
    }
    file.save()
        .with_context(|| format!("save memory file {}", sibling.path.display()))?;
    *mark = file.frontmatter.superseded_by;
    Ok(())
}

fn rename_memory(store: &MemoryStore, cwd: &Path, args: &Value) -> Result<Value> {
    let name = arg_str(args, "name")?;
    let new_name = arg_str(args, "new_name")?;
    let scope = arg_str(args, "scope")?;
    validate_slug("new_name", &new_name)?;

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
    ensure_index_entry_present(&index_path, &new_name, &description)?;

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

/// `expires` is compared to today as text, so only the zero-padded `YYYY-MM-DD`
/// spelling sorts correctly; an unpadded or non-ISO value is refused at write.
fn is_iso_date(value: &str) -> bool {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .is_ok_and(|date| date.format("%Y-%m-%d").to_string() == value)
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

pub(crate) fn write_memory_index(target_dir: &Path, content: &str) -> Result<PathBuf> {
    fs::create_dir_all(target_dir)?;
    let index_path = target_dir.join("MEMORY.md");

    fs::write(&index_path, content)?;

    Ok(index_path)
}

fn ensure_index_entry(index_path: &Path, name: &str, description: &str) -> Result<bool> {
    let existing = fs::read_to_string(index_path).unwrap_or_default();
    let entry = format!("- [[{name}]]: {description}");
    if let Some((offset, line)) = catalogue_line_of(&existing, name) {
        if catalogue_entry(line).is_some_and(|(_, desc)| desc == description) {
            return Ok(false);
        }
        let rewritten = format!(
            "{}{entry}{}",
            &existing[..offset],
            &existing[offset + line.len()..]
        );
        fs::write(index_path, rewritten)?;
        return Ok(true);
    }
    append_index_entry(index_path, &existing, &entry)?;
    Ok(true)
}

/// Guarantees `name` has a catalogue line but never rewrites one that is
/// already there, whatever description it carries: a rename moves the entry,
/// only a write refreshes it.
fn ensure_index_entry_present(index_path: &Path, name: &str, description: &str) -> Result<()> {
    let existing = fs::read_to_string(index_path).unwrap_or_default();
    if catalogue_line_of(&existing, name).is_some() {
        return Ok(());
    }
    append_index_entry(
        index_path,
        &existing,
        &format!("- [[{name}]]: {description}"),
    )
}

/// Byte offset of the catalogue line for `name` and that line without its
/// line ending.
fn catalogue_line_of<'a>(index: &'a str, name: &str) -> Option<(usize, &'a str)> {
    let mut offset = 0;
    for raw in index.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\r', '\n']);
        if catalogue_slug(line) == Some(name) {
            return Some((offset, line));
        }
        offset += raw.len();
    }
    None
}

fn append_index_entry(index_path: &Path, existing: &str, entry: &str) -> Result<()> {
    let new_content = if existing.is_empty() {
        format!("# Memory Index\n\n{entry}\n")
    } else if existing.ends_with('\n') {
        format!("{existing}{entry}\n")
    } else {
        format!("{existing}\n{entry}\n")
    };

    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(index_path, new_content)?;
    Ok(())
}

fn remove_index_entry(index_path: &Path, name: &str) -> Result<bool> {
    let Ok(existing) = fs::read_to_string(index_path) else {
        return Ok(false);
    };
    let kept: Vec<&str> = existing
        .lines()
        .filter(|line| catalogue_slug(line) != Some(name))
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
    index.lines().find_map(|line| {
        let (slug, desc) = catalogue_entry(line)?;
        (slug == name && !desc.is_empty()).then(|| desc.to_string())
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
    fn ensure_index_entry_rewrites_the_line_in_place_when_the_description_differs() {
        let root = temp_root("index_rewrite");
        let index = root.join("MEMORY.md");
        let original =
            "# Memory Index\n\n- [[existing]]: already here\n- [[other]]: untouched\n\nfooter\n";
        fs::write(&index, original).unwrap();

        let updated = ensure_index_entry(&index, "existing", "different description").unwrap();
        assert!(updated);
        let rewritten = "# Memory Index\n\n- [[existing]]: different description\n- [[other]]: untouched\n\nfooter\n";
        assert_eq!(fs::read_to_string(&index).unwrap(), rewritten);

        let updated = ensure_index_entry(&index, "existing", "different description").unwrap();
        assert!(!updated, "an identical description is not a rewrite");
        assert_eq!(fs::read_to_string(&index).unwrap(), rewritten);

        let crlf =
            "# Memory Index\r\n\r\n- [[existing]]: already here\r\n- [[other]]: untouched\r\n";
        fs::write(&index, crlf).unwrap();
        let updated = ensure_index_entry(&index, "existing", "different description").unwrap();
        assert!(updated);
        assert_eq!(
            fs::read_to_string(&index).unwrap(),
            "# Memory Index\r\n\r\n- [[existing]]: different description\r\n- [[other]]: untouched\r\n",
            "a CRLF index keeps its line endings"
        );

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
    fn remove_index_entry_leaves_lines_that_only_cite_the_slug() {
        let root = temp_root("index_remove_strict");
        let index = root.join("MEMORY.md");
        fs::write(
            &index,
            "# Memory Index\n\n- Run auth.md checks before [[auth]] lands\n- [[auth-flow]]: sibling\n- [[auth]]: the file\n",
        )
        .unwrap();

        assert!(remove_index_entry(&index, "auth").unwrap());
        let content = fs::read_to_string(&index).unwrap();
        assert!(
            content.contains("- Run auth.md checks before [[auth]] lands\n"),
            "{content}"
        );
        assert!(content.contains("- [[auth-flow]]: sibling\n"));
        assert!(!content.contains("- [[auth]]: the file"));

        let _ = fs::remove_dir_all(&root);
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
    fn rename_memory_moves_the_catalogue_line_without_rewriting_it() {
        let root = temp_root("rename_index_text");
        let workspace = root.join("ws");
        let structured = workspace.join(".coyote").join("memory");
        fs::create_dir_all(&structured).unwrap();
        fs::write(
            structured.join("MEMORY.md"),
            "# Memory Index\n\n- [[linker]]: links\n- [[drift]]: hand-curated wording\n",
        )
        .unwrap();
        fs::write(
            structured.join("linker.md"),
            "---\nname: linker\n---\nno description here\n",
        )
        .unwrap();
        fs::write(
            structured.join("drift.md"),
            "---\nname: drift\ndescription: what the file says\n---\nx\n",
        )
        .unwrap();
        fs::write(
            structured.join("uncatalogued.md"),
            "---\nname: uncatalogued\ndescription: never indexed\n---\nx\n",
        )
        .unwrap();
        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let args = json!({"name": "linker", "new_name": "new-linker", "scope": "workspace"});
        rename_memory(&store, &workspace, &args).unwrap();
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(
            index.contains("- [[new-linker]]: links\n"),
            "a file without a description keeps the catalogue text: {index}"
        );

        let args = json!({"name": "drift", "new_name": "drifted", "scope": "workspace"});
        rename_memory(&store, &workspace, &args).unwrap();
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(
            index.contains("- [[drifted]]: hand-curated wording\n"),
            "a rename moves the line, only a write refreshes it: {index}"
        );
        assert!(!index.contains("what the file says"), "{index}");

        let args = json!({"name": "uncatalogued", "new_name": "catalogued", "scope": "workspace"});
        rename_memory(&store, &workspace, &args).unwrap();
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(
            index.ends_with("- [[catalogued]]: never indexed\n"),
            "a missing line is still appended: {index}"
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
            RoundChains::default(),
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
            RoundChains::default(),
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
            RoundChains::default(),
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
            RoundChains::default(),
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
            RoundChains::default(),
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
            RoundChains::default(),
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
            RoundChains::default(),
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
        write_memory(&store, &workspace, &scratch, 3, RoundChains::default()).unwrap();
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
        write_memory(&store, &workspace, &pinned, 3, RoundChains::default()).unwrap();
        let saved = MemoryFile::load(&structured.join("pinned.md")).unwrap();
        assert_eq!(saved.frontmatter.expires.as_deref(), Some("2999-01-01"));

        let durable = json!({
            "name": "durable",
            "description": "no type",
            "content": "x",
            "scope": "workspace",
        });
        write_memory(&store, &workspace, &durable, 3, RoundChains::default()).unwrap();
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
            RoundChains::default(),
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
    fn write_memory_rejects_an_expires_that_is_not_a_padded_iso_date() {
        let root = temp_root("write_bad_expires");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        let write = |expires: &str| {
            let args = json!({
                "name": "dated",
                "description": "a dated fact",
                "content": "x",
                "scope": "workspace",
                "expires": expires,
            });
            write_memory(
                &store,
                &workspace,
                &args,
                DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
                RoundChains::default(),
            )
        };

        for bad in ["tomorrow", "03/15/2026", "2026-1-5", "2026-02-30", ""] {
            let text = write(bad).unwrap_err().to_string();
            assert!(
                text.contains(&format!("Invalid `expires` '{bad}'")) && text.contains("YYYY-MM-DD"),
                "{bad}: {text}"
            );
            assert!(!structured.join("dated.md").exists(), "{bad} wrote a file");
        }

        write("2026-01-05").unwrap();
        let raw = fs::read_to_string(structured.join("dated.md")).unwrap();
        assert!(raw.contains("\nexpires: 2026-01-05\n"), "{raw}");
        assert!(is_iso_date("2099-12-31") && !is_iso_date("2099-12-1"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_declaration_tells_the_model_stale_files_leave_its_context() {
        let declarations = memory_function_declarations();
        let write = declarations
            .iter()
            .find(|d| d.name == "memory__write")
            .unwrap();
        let properties = write.parameters.properties.as_ref().unwrap();
        for field in ["superseded_by", "expires"] {
            let description = properties[field].description.as_deref().unwrap();
            assert!(
                description
                    .contains("drops out of prompt injection and the default `memory__list`")
                    && description.contains("`include_stale`")
                    && description.contains("stays readable"),
                "{field}: {description}"
            );
        }
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
    fn write_declaration_states_the_slug_and_description_limits() {
        let declarations = memory_function_declarations();
        let write = declarations
            .iter()
            .find(|d| d.name == "memory__write")
            .unwrap();
        let properties = write.parameters.properties.as_ref().unwrap();
        let name = properties["name"].description.as_deref().unwrap();
        assert!(
            name.contains("lowercase letters, digits, hyphens and underscores")
                && name.contains("at most 64 chars"),
            "{name}"
        );
        let description = properties["description"].description.as_deref().unwrap();
        assert!(
            description.contains("no newline") && description.contains("at most 160 chars"),
            "{description}"
        );
        assert!(write.description.contains("`memory_round_chains`"));
        assert!(
            write
                .description
                .contains("Prefer updating an existing slug")
        );
        assert!(write.description.contains(
            "Either way, a round written below an existing one keeps superseded_by pointing at \
             the round above."
        ));
    }

    #[test]
    fn rename_declaration_states_the_same_slug_grammar_as_write() {
        let declarations = memory_function_declarations();
        let rename = declarations
            .iter()
            .find(|d| d.name == "memory__rename")
            .unwrap();
        let new_name = rename.parameters.properties.as_ref().unwrap()["new_name"]
            .description
            .as_deref()
            .unwrap();
        assert!(new_name.contains(&slug_grammar()), "{new_name}");
    }

    fn write_args(name: &str, description: &str, content: &str) -> Value {
        json!({
            "name": name,
            "description": description,
            "content": content,
            "scope": "workspace",
        })
    }

    fn md_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".md") && n != "MEMORY.md")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_memory_rejects_bad_slugs_and_writes_nothing() {
        let root = temp_root("write_bad_slug");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        let long = "a".repeat(65);

        for bad in [
            "Upper",
            "has space",
            long.as_str(),
            "-leading",
            "",
            "dots.too",
        ] {
            let args = write_args(bad, "d", "x");
            let text = write_memory(&store, &workspace, &args, 3, RoundChains::default())
                .unwrap_err()
                .to_string();
            assert!(
                text.starts_with(&format!("invalid name '{bad}'")) && text.contains("max 64 chars"),
                "{bad}: {text}"
            );
            assert!(md_files(&structured).is_empty(), "{bad} wrote a file");
        }
        assert_eq!(
            fs::read_to_string(structured.join("MEMORY.md")).unwrap(),
            "# Memory Index\n"
        );

        let longest = "b".repeat(64);
        write_memory(
            &store,
            &workspace,
            &write_args(&longest, "d", "x"),
            3,
            RoundChains::default(),
        )
        .unwrap();
        write_memory(
            &store,
            &workspace,
            &write_args("7_ok-slug", "d", "x"),
            3,
            RoundChains::default(),
        )
        .unwrap();
        assert_eq!(
            md_files(&structured),
            vec!["7_ok-slug.md".to_string(), format!("{longest}.md")]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_caps_the_description_and_the_body() {
        let root = temp_root("write_caps");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        let refuse = |args: Value| {
            let text = write_memory(&store, &workspace, &args, 3, RoundChains::default())
                .unwrap_err()
                .to_string();
            assert!(md_files(&structured).is_empty(), "{text} wrote a file");
            text
        };

        let text = refuse(write_args("fact", &"d".repeat(161), "x"));
        assert_eq!(text, "description is 161 chars, the cap is 160");

        for broken in ["two\nlines", "two\rlines"] {
            let text = refuse(write_args("fact", broken, "x"));
            assert_eq!(
                text,
                "description must be one line (no newline or carriage return)"
            );
        }

        let text = refuse(write_args("fact", "d", &"x".repeat(4001)));
        assert_eq!(
            text,
            "content is 4001 chars, the cap is 4000 (split the topic across linked files)"
        );

        let at_cap = write_args("fact", &"d".repeat(160), &"x".repeat(4000));
        write_memory(&store, &workspace, &at_cap, 3, RoundChains::default()).unwrap();
        let saved = MemoryFile::load(&structured.join("fact.md")).unwrap();
        assert_eq!(saved.char_len(), 4000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_guards_run_in_a_fixed_order() {
        let root = temp_root("write_guard_order");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        let refuse = |args: Value| {
            write_memory(&store, &workspace, &args, 3, RoundChains::default())
                .unwrap_err()
                .to_string()
        };

        let missing_and_bad_slug = json!({
            "name": "Bad Name",
            "content": "x",
            "scope": "workspace",
        });
        assert!(
            refuse(missing_and_bad_slug).starts_with("missing required argument(s): description.")
        );

        let front_matter_and_bad_slug = write_args("Bad Name", "d", "---\nname: x\n---\nbody");
        assert!(
            refuse(front_matter_and_bad_slug)
                .starts_with("`content` starts with a YAML front-matter block.")
        );

        let mut bad_slug_and_type = write_args("Bad Name", "two\nlines", "x");
        bad_slug_and_type["type"] = json!("bogus");
        assert!(refuse(bad_slug_and_type).starts_with("invalid name 'Bad Name'"));

        let mut bad_type_and_description = write_args("fact", "two\nlines", "x");
        bad_type_and_description["type"] = json!("bogus");
        assert!(refuse(bad_type_and_description).starts_with("Invalid value 'bogus'"));

        let bad_description_and_body = write_args("fact", "two\nlines", &"x".repeat(4001));
        assert!(refuse(bad_description_and_body).starts_with("description must be one line"));

        let mut bad_body_and_expires = write_args("fact", "d", &"x".repeat(4001));
        bad_body_and_expires["expires"] = json!("tomorrow");
        assert!(refuse(bad_body_and_expires).starts_with("content is 4001 chars"));

        assert!(md_files(&structured).is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_overwrite_refreshes_the_index_line_in_place() {
        let root = temp_root("write_refresh_index");
        let (store, workspace, structured) = workspace_store(
            &root,
            "# Memory Index\n\n- [[before]]: b\n- [[fact]]: first take\n- [[after]]: a\n",
        );

        let result = write_memory(
            &store,
            &workspace,
            &write_args("fact", "second take", "x"),
            3,
            RoundChains::default(),
        )
        .unwrap();
        assert_eq!(result["index_updated"], true, "{result}");
        assert_eq!(
            fs::read_to_string(structured.join("MEMORY.md")).unwrap(),
            "# Memory Index\n\n- [[before]]: b\n- [[fact]]: second take\n- [[after]]: a\n"
        );

        let result = write_memory(
            &store,
            &workspace,
            &write_args("fact", "  second take \t", "x"),
            3,
            RoundChains::default(),
        )
        .unwrap();
        assert_eq!(
            result["index_updated"], false,
            "surrounding whitespace on the argument does not count as a change: {result}"
        );
        assert_eq!(
            fs::read_to_string(structured.join("MEMORY.md")).unwrap(),
            "# Memory Index\n\n- [[before]]: b\n- [[fact]]: second take\n- [[after]]: a\n"
        );
        let saved = MemoryFile::load(&structured.join("fact.md")).unwrap();
        assert_eq!(
            saved.frontmatter.description.as_deref(),
            Some("second take")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn round_chain_name_splits_only_a_trailing_round_suffix() {
        assert_eq!(round_chain_name("foo-r2"), Some(("foo", 2)));
        assert_eq!(round_chain_name("foo-r02"), Some(("foo", 2)));
        assert_eq!(round_chain_name("foo-r1-r12"), Some(("foo-r1", 12)));
        for plain in [
            "foo",
            "foo-r",
            "-r2",
            "foo-rx",
            "foo-r2x",
            "r2",
            "foo-r4294967296",
        ] {
            assert_eq!(round_chain_name(plain), None, "{plain}");
        }
    }

    fn write_round(dir: &Path, name: &str, extra_frontmatter: &str) {
        fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: {name} notes\ncreated: 2020-01-01\n\
                 updated: 2020-01-01\n{extra_frontmatter}---\nold body\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn write_memory_supersedes_older_rounds_in_the_same_scope() {
        let root = temp_root("write_round_supersede");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r0", "superseded_by: foo-r1\n");
        write_round(&structured, "foo-r1", "");
        write_round(&structured, "foo-r2", "type: user\n");
        write_round(&structured, "foo-r4", "");
        write_round(&structured, "foobar-r1", "");
        let untouched_r4 = fs::read_to_string(structured.join("foo-r4.md")).unwrap();
        let untouched_other = fs::read_to_string(structured.join("foobar-r1.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r3", "round three", "new body"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(
            result["round_chain"],
            json!({"base": "foo", "round": 3, "newer_rounds": ["foo-r4"]}),
            "{result}"
        );
        assert_eq!(
            result["siblings"],
            json!([
                {"name": "foo-r0", "superseded_by": "foo-r1"},
                {"name": "foo-r1", "superseded_by": "foo-r3"},
                {"name": "foo-r2", "superseded_by": "foo-r3"},
            ]),
            "{result}"
        );

        let today = today_string();
        let r1 = MemoryFile::load(&structured.join("foo-r1.md")).unwrap();
        assert_eq!(r1.frontmatter.superseded_by.as_deref(), Some("foo-r3"));
        assert_eq!(r1.frontmatter.created.as_deref(), Some("2020-01-01"));
        assert_eq!(r1.frontmatter.updated.as_deref(), Some(today.as_str()));
        assert_eq!(r1.frontmatter.kind.kind(), MemoryKind::RunState);
        assert_eq!(r1.frontmatter.expires, None);
        assert_eq!(r1.body, "old body\n");
        let raw = fs::read_to_string(structured.join("foo-r1.md")).unwrap();
        assert!(
            raw.contains("\ntype: run-state\n") && !raw.contains("expires"),
            "{raw}"
        );

        let r2 = MemoryFile::load(&structured.join("foo-r2.md")).unwrap();
        assert_eq!(r2.frontmatter.superseded_by.as_deref(), Some("foo-r3"));
        assert_eq!(r2.frontmatter.kind.kind(), MemoryKind::User);
        assert_eq!(r2.frontmatter.updated.as_deref(), Some(today.as_str()));

        let r0 = MemoryFile::load(&structured.join("foo-r0.md")).unwrap();
        assert_eq!(r0.frontmatter.superseded_by.as_deref(), Some("foo-r1"));
        assert_eq!(r0.frontmatter.updated.as_deref(), Some("2020-01-01"));

        assert_eq!(
            fs::read_to_string(structured.join("foo-r4.md")).unwrap(),
            untouched_r4
        );
        assert_eq!(
            fs::read_to_string(structured.join("foobar-r1.md")).unwrap(),
            untouched_other
        );

        let written = fs::read_to_string(structured.join("foo-r3.md")).unwrap();
        assert!(
            written.contains("\ntype: null\n") && written.contains("\nsuperseded_by: foo-r4\n"),
            "the written file keeps its own defaulting: {written}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_keeps_a_rewritten_older_round_superseded() {
        let root = temp_root("write_round_rewrite_older");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "superseded_by: foo-r2\n");
        write_round(&structured, "foo-r2", "");
        let untouched_r2 = fs::read_to_string(structured.join("foo-r2.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r1", "round one again", "new body"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(
            result["round_chain"],
            json!({"base": "foo", "round": 1, "newer_rounds": ["foo-r2"]}),
            "{result}"
        );
        assert_eq!(result["siblings"], json!([]), "{result}");
        let r1 = MemoryFile::load(&structured.join("foo-r1.md")).unwrap();
        assert_eq!(r1.frontmatter.superseded_by.as_deref(), Some("foo-r2"));
        assert_eq!(r1.body, "new body");
        assert_eq!(
            fs::read_to_string(structured.join("foo-r2.md")).unwrap(),
            untouched_r2
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_prefers_the_previous_mark_over_the_next_round_up() {
        let root = temp_root("write_round_previous_mark");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "superseded_by: foo-r3\n");
        write_round(&structured, "foo-r2", "");
        write_round(&structured, "foo-r3", "");

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r1", "round one again", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(
            result["round_chain"]["newer_rounds"],
            json!(["foo-r2", "foo-r3"]),
            "{result}"
        );
        assert_eq!(result["siblings"], json!([]), "{result}");
        let r1 = MemoryFile::load(&structured.join("foo-r1.md")).unwrap();
        assert_eq!(
            r1.frontmatter.superseded_by.as_deref(),
            Some("foo-r3"),
            "the mark the file already carried wins over the lowest higher round"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_marks_an_unmarked_older_round_with_the_next_round_up() {
        let root = temp_root("write_round_next_up");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "");
        write_round(&structured, "foo-r3", "");
        write_round(&structured, "foo-r2", "");

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r1", "round one again", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(
            result["round_chain"]["newer_rounds"],
            json!(["foo-r2", "foo-r3"]),
            "{result}"
        );
        let r1 = MemoryFile::load(&structured.join("foo-r1.md")).unwrap();
        assert_eq!(r1.frontmatter.superseded_by.as_deref(), Some("foo-r2"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_lets_an_explicit_superseded_by_win_over_the_round_default() {
        let root = temp_root("write_round_explicit_mark");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "superseded_by: foo-r2\n");
        write_round(&structured, "foo-r2", "");

        let mut args = write_args("foo-r1", "round one again", "x");
        args["superseded_by"] = json!("elsewhere");
        let result = write_memory(&store, &workspace, &args, 3, RoundChains::Supersede).unwrap();

        assert_eq!(result["round_chain"]["newer_rounds"], json!(["foo-r2"]));
        let r1 = MemoryFile::load(&structured.join("foo-r1.md")).unwrap();
        assert_eq!(r1.frontmatter.superseded_by.as_deref(), Some("elsewhere"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_reports_a_malformed_older_round_and_settles_the_rest() {
        let root = temp_root("write_round_malformed");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "");
        fs::write(structured.join("foo-r2.md"), "---\nname: [\n---\nbroken\n").unwrap();
        let broken = fs::read_to_string(structured.join("foo-r2.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r3", "round three", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(result["status"], "ok", "{result}");
        let siblings = result["siblings"].as_array().unwrap();
        assert_eq!(
            siblings[0],
            json!({"name": "foo-r1", "superseded_by": "foo-r3"}),
            "{result}"
        );
        assert_eq!(siblings[1]["name"], "foo-r2", "{result}");
        assert_eq!(siblings[1]["superseded_by"], Value::Null, "{result}");
        let error = siblings[1]["error"].as_str().unwrap();
        assert!(error.contains("parse frontmatter"), "{error}");
        assert_eq!(
            fs::read_to_string(structured.join("foo-r2.md")).unwrap(),
            broken
        );
        assert!(structured.join("foo-r3.md").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_memory_refuses_a_round_when_the_scope_dir_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("write_round_unlistable_dir");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "");
        let index_before = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        fs::set_permissions(&structured, fs::Permissions::from_mode(0o300)).unwrap();
        if fs::read_dir(&structured).is_ok() {
            eprintln!("skipping: mode bits are ignored here (running as root)");
            fs::set_permissions(&structured, fs::Permissions::from_mode(0o755)).unwrap();
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let err = write_memory(
            &store,
            &workspace,
            &write_args("foo-r2", "round two", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap_err();

        fs::set_permissions(&structured, fs::Permissions::from_mode(0o755)).unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains(&format!("read {}", structured.display())),
            "{text}"
        );
        assert!(
            !structured.join("foo-r2.md").exists(),
            "nothing is written when the siblings cannot be listed"
        );
        assert_eq!(
            fs::read_to_string(structured.join("MEMORY.md")).unwrap(),
            index_before
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_memory_reports_an_older_round_it_cannot_save() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("write_round_readonly");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "");
        let locked = structured.join("foo-r1.md");
        let before = fs::read_to_string(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o444)).unwrap();
        if fs::write(&locked, &before).is_ok() {
            eprintln!("skipping: mode bits are ignored here (running as root)");
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r2", "round two", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(result["status"], "ok", "{result}");
        let row = &result["siblings"][0];
        assert_eq!(row["name"], "foo-r1", "{result}");
        assert_eq!(row["superseded_by"], Value::Null, "{result}");
        assert!(
            row["error"].as_str().unwrap().contains("save memory file"),
            "{result}"
        );
        assert_eq!(fs::read_to_string(&locked).unwrap(), before);
        assert!(structured.join("foo-r2.md").exists());

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_leaves_an_older_round_whose_front_matter_name_differs() {
        let root = temp_root("write_round_name_mismatch");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "other", "superseded_by: elsewhere\n");
        fs::rename(structured.join("other.md"), structured.join("foo-r1.md")).unwrap();
        let before = fs::read_to_string(structured.join("foo-r1.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r2", "round two", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(
            result["siblings"],
            json!([{
                "name": "foo-r1",
                "superseded_by": "elsewhere",
                "error": "front matter name 'other' does not match the file name",
            }]),
            "{result}"
        );
        assert_eq!(
            fs::read_to_string(structured.join("foo-r1.md")).unwrap(),
            before
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_memory_skips_a_symlinked_older_round() {
        let root = temp_root("write_round_symlink");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        write_round(&outside, "foo-r1", "");
        let target = outside.join("foo-r1.md");
        std::os::unix::fs::symlink(&target, structured.join("foo-r1.md")).unwrap();
        let before = fs::read_to_string(&target).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r2", "round two", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(result["siblings"], json!([]), "{result}");
        assert_eq!(fs::read_to_string(&target).unwrap(), before);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_keeps_older_rounds_when_configured_to() {
        let root = temp_root("write_round_keep");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r1", "");
        let before = fs::read_to_string(structured.join("foo-r1.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r2", "round two", "x"),
            3,
            RoundChains::Keep,
        )
        .unwrap();

        assert_eq!(
            result["siblings"],
            json!([{"name": "foo-r1", "superseded_by": null}]),
            "{result}"
        );
        assert_eq!(
            result["round_chain"],
            json!({"base": "foo", "round": 2, "newer_rounds": []})
        );
        assert_eq!(
            fs::read_to_string(structured.join("foo-r1.md")).unwrap(),
            before
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_marks_a_lower_round_even_when_configured_to_keep() {
        let root = temp_root("write_round_keep_lower");
        let (store, workspace, structured) = workspace_store(&root, "# Memory Index\n");
        write_round(&structured, "foo-r2", "");
        let untouched_r2 = fs::read_to_string(structured.join("foo-r2.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r1", "round one", "x"),
            3,
            RoundChains::Keep,
        )
        .unwrap();

        assert_eq!(
            result["round_chain"],
            json!({"base": "foo", "round": 1, "newer_rounds": ["foo-r2"]}),
            "{result}"
        );
        let r1 = MemoryFile::load(&structured.join("foo-r1.md")).unwrap();
        assert_eq!(
            r1.frontmatter.superseded_by.as_deref(),
            Some("foo-r2"),
            "the key governs older rounds only; the upward mark always applies"
        );
        assert_eq!(
            fs::read_to_string(structured.join("foo-r2.md")).unwrap(),
            untouched_r2
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_round_chains_never_cross_scopes() {
        let root = temp_root("write_round_scopes");
        let (store, workspace, _) = workspace_store(&root, "# Memory Index\n");
        fs::create_dir_all(&store.global_dir).unwrap();
        write_round(&store.global_dir, "foo-r1", "");
        let before = fs::read_to_string(store.global_dir.join("foo-r1.md")).unwrap();

        let result = write_memory(
            &store,
            &workspace,
            &write_args("foo-r2", "round two", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();

        assert_eq!(result["siblings"], json!([]), "{result}");
        assert_eq!(
            result["round_chain"],
            json!({"base": "foo", "round": 2, "newer_rounds": []})
        );
        assert_eq!(
            fs::read_to_string(store.global_dir.join("foo-r1.md")).unwrap(),
            before
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_memory_reports_a_round_chain_only_for_round_names() {
        let root = temp_root("write_round_report");
        let (store, workspace, _) = workspace_store(&root, "# Memory Index\n");

        let first = write_memory(
            &store,
            &workspace,
            &write_args("foo-r1", "round one", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();
        assert_eq!(first["siblings"], json!([]), "{first}");
        assert_eq!(
            first["round_chain"],
            json!({"base": "foo", "round": 1, "newer_rounds": []})
        );

        let plain = write_memory(
            &store,
            &workspace,
            &write_args("foo", "no round", "x"),
            3,
            RoundChains::Supersede,
        )
        .unwrap();
        assert!(plain.get("siblings").is_none(), "{plain}");
        assert!(plain.get("round_chain").is_none(), "{plain}");

        let _ = fs::remove_dir_all(&root);
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
        let result = write_memory(&store, &workspace, &args, 3, RoundChains::default()).unwrap();
        assert_eq!(result["index_updated"], true);
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(index.contains("- Always run tests before [[cited]] lands\n"));
        assert!(index.contains("- [[cited]]: what cited means\n"), "{index}");

        let report = lint_memory(&store).unwrap();
        assert!(report["orphans"].as_array().unwrap().is_empty(), "{report}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_memory_keeps_a_facts_line_that_cites_the_deleted_slug() {
        let root = temp_root("delete_cited_slug");
        let (store, workspace, structured) = workspace_store(
            &root,
            "# Index\n\n## FACTS\n- See [[cited]]: it explains the gate\n\n- [[cited]]: what cited means\n",
        );
        fs::write(
            structured.join("cited.md"),
            "---\nname: cited\ndescription: the real description\n---\nx\n",
        )
        .unwrap();

        let report = lint_memory(&store).unwrap();
        let drift = report["description_drift"].as_array().unwrap();
        assert_eq!(drift.len(), 1, "{report}");
        assert_eq!(drift[0]["index_description"], "what cited means");

        let result = delete_memory(
            &store,
            &workspace,
            &json!({"name": "cited", "scope": "workspace"}),
        )
        .unwrap();
        assert_eq!(result["index_updated"], true, "{result}");
        let index = fs::read_to_string(structured.join("MEMORY.md")).unwrap();
        assert!(
            index.contains("- See [[cited]]: it explains the gate\n"),
            "{index}"
        );
        assert!(!index.contains("- [[cited]]: what cited means"), "{index}");

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
