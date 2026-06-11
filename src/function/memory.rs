use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::{FunctionDeclaration, JsonSchema};
use crate::config::RequestContext;
use crate::config::memory::{MemoryFile, MemoryFrontmatter, MemoryStore, WorkspaceMemory};
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
                "global_index_exists": paths::global_memory_index_path().exists(),
                "workspace": store.workspace.as_ref().map(workspace_label),
            }))
        }
        "write" => {
            let name = arg_str(args, "name")?;
            let description = arg_str(args, "description")?;
            let content = arg_str(args, "content")?;
            let scope = arg_str(args, "scope")?;
            let kind = args.get("type").and_then(Value::as_str).map(String::from);

            let target_dir = match scope.as_str() {
                "global" => paths::global_memory_dir(),
                "workspace" => workspace_write_dir(&store)?,
                other => bail!("unknown scope '{}': use 'global' or 'workspace'", other),
            };
            let file = MemoryFile {
                path: target_dir.join(format!("{name}.md")),
                frontmatter: MemoryFrontmatter {
                    name: name.clone(),
                    description: Some(description.clone()),
                    kind,
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
            }))
        }
        "lint" => lint_memory(&store),
        _ => bail!("unknown memory action: {action}"),
    }
}

fn ensure_index_entry(index_path: &Path, name: &str, description: &str) -> Result<bool> {
    let existing = fs::read_to_string(index_path).unwrap_or_default();
    let already_referenced =
        existing.contains(&format!("[[{name}]]")) || existing.contains(&format!("{name}.md"));

    if already_referenced {
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

fn workspace_write_dir(store: &MemoryStore) -> Result<PathBuf> {
    match &store.workspace {
        Some(WorkspaceMemory::Structured { dir, .. }) => Ok(dir.clone()),
        Some(WorkspaceMemory::Lite { workspace_root, .. }) => {
            Ok(paths::workspace_memory_dir_for(workspace_root))
        }
        None => bail!("no workspace memory discoverable; cannot write workspace-scoped memory"),
    }
}

fn workspace_label(w: &WorkspaceMemory) -> Value {
    match w {
        WorkspaceMemory::Structured { workspace_root, .. } => json!({
            "mode": "structured",
            "root": workspace_root.display().to_string(),
        }),
        WorkspaceMemory::Lite {
            workspace_root,
            file,
        } => json!({
            "mode": "lite",
            "root": workspace_root.display().to_string(),
            "file": file.display().to_string(),
        }),
    }
}

fn lint_memory(store: &MemoryStore) -> Result<Value> {
    let files = store.list_files()?;
    let names: HashSet<&str> = files.iter().map(|f| f.frontmatter.name.as_str()).collect();

    let mut oversized = Vec::new();
    let mut broken_links = Vec::new();
    for f in &files {
        if f.char_len() > PER_FILE_SOFT_CAP {
            oversized.push(json!({"name": &f.frontmatter.name, "chars": f.char_len()}));
        }
        for link in extract_wikilinks(&f.body) {
            if !names.contains(link.as_str()) {
                broken_links.push(json!({"from": &f.frontmatter.name, "to": link}));
            }
        }
    }

    let index_content = store
        .load_global_index()?
        .or_else(|| store.load_workspace_index().ok().flatten())
        .unwrap_or_default();
    let mut orphans = Vec::new();
    for f in &files {
        if !index_content.contains(&f.frontmatter.name) {
            orphans.push(f.frontmatter.name.clone());
        }
    }

    Ok(json!({
        "total_files": files.len(),
        "oversized": oversized,
        "broken_wikilinks": broken_links,
        "orphans": orphans,
    }))
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
        fs::write(&index, "# Memory Index\n\n- [[existing]] — already here\n").unwrap();

        let updated = ensure_index_entry(&index, "new_one", "newly added").unwrap();
        assert!(updated);
        let content = fs::read_to_string(&index).unwrap();
        assert!(content.contains("- [[existing]] — already here"));
        assert!(content.contains("- [[new_one]] — newly added"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_index_entry_skips_when_referenced() {
        let root = temp_root("index_skip");
        let index = root.join("MEMORY.md");
        let original = "# Memory Index\n\n- [[existing]] — already here\n";
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
        assert!(content.contains("- [[first]] — first ever"));

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

        let dir = workspace_write_dir(&store).unwrap();
        assert_eq!(dir, structured);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_write_dir_promotes_lite_to_structured_subdir() {
        let root = temp_root("ws_lite_promote");
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("COYOTE.md"), "lite").unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&workspace),
        };

        let dir = workspace_write_dir(&store).unwrap();
        assert_eq!(dir, workspace.join(".coyote").join("memory"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_write_dir_errors_when_no_workspace() {
        let root = temp_root("ws_none");
        let bare = root.join("nowhere");
        fs::create_dir_all(&bare).unwrap();

        let store = MemoryStore {
            global_dir: root.join("g"),
            workspace: discover_workspace_memory(&bare),
        };

        let err = workspace_write_dir(&store).unwrap_err();
        assert!(err.to_string().contains("no workspace memory discoverable"));

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
}
