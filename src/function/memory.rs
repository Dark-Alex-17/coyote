use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::{FunctionDeclaration, JsonSchema};
use crate::config::RequestContext;
use crate::config::memory::{
    MemoryFile, MemoryFrontmatter, MemoryStore, WorkspaceMemory, bootstrap_workspace_memory,
    find_git_root,
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
                "Create or replace a memory file. Caller must also update MEMORY.md index."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "name".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Short kebab-case slug for the file (no extension)".into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "description".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("One-line description for the MEMORY.md index".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "type".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Memory type: user | feedback | project | reference".into(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        "content".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The full markdown body of the memory file".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "scope".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Where to write: 'global' (user-level) or 'workspace' (project-level)"
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
            description: "List all known drill files with metadata (size, type, scope).".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
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
                "type": file.frontmatter.kind,
                "content": file.body,
            }))
        }
        "list" => {
            let files = store.list_files()?;
            let entries: Vec<_> = files
                .iter()
                .map(|f| {
                    json!({
                        "name": f.frontmatter.name,
                        "description": f.frontmatter.description,
                        "type": f.frontmatter.kind,
                        "char_len": f.char_len(),
                        "path": f.path.display().to_string(),
                    })
                })
                .collect();

            Ok(json!({
                "files": entries,
                "global_index_exists": paths::global_memory_index_file().exists(),
                "workspace": store.workspace.as_ref().map(workspace_label),
            }))
        }
        "write" => write_memory(&store, &cwd, args),
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

fn write_memory(store: &MemoryStore, cwd: &Path, args: &Value) -> Result<Value> {
    let name = arg_str(args, "name")?;
    let description = arg_str(args, "description")?;
    let content = arg_str(args, "content")?;
    let scope = arg_str(args, "scope")?;
    let kind = args.get("type").and_then(Value::as_str).map(String::from);
    let superseded_by = args
        .get("superseded_by")
        .and_then(Value::as_str)
        .map(String::from);
    let expires = args
        .get("expires")
        .and_then(Value::as_str)
        .map(String::from);

    let target_dir = scope_dir(store, cwd, &scope)?;
    let path = target_dir.join(format!("{name}.md"));
    let previous = if path.exists() {
        MemoryFile::load(&path).ok()
    } else {
        None
    };
    let today = today_string();
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
    let files = store.list_files()?;
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
    let files = store.list_files()?;
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

fn today_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
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
    if index_references(&existing, name) {
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

fn index_references(index: &str, name: &str) -> bool {
    index.lines().any(|line| line_references(line, name))
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

fn find_file(store: &MemoryStore, name: &str) -> Result<Option<MemoryFile>> {
    Ok(store
        .list_files()?
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
    let files = store.list_files()?;
    let names: HashSet<&str> = files.iter().map(|f| f.frontmatter.name.as_str()).collect();
    let today = today_string();

    let mut oversized = Vec::new();
    let mut broken_links = Vec::new();
    let mut stale = Vec::new();
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

        if !index_references(index, &f.frontmatter.name) {
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

        fs::write(structured.join("MEMORY.md"), "- referenced\n").unwrap();
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
        let result = write_memory(&store, &workspace, &first).unwrap();
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
        let result = write_memory(&store, &workspace, &second).unwrap();
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
}
