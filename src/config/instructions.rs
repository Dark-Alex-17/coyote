use std::fs;
use std::path::{Path, PathBuf};

use log::warn;

pub const WORKSPACE_INSTRUCTIONS_FILE_NAME: &str = "COYOTE.md";
pub const DEFAULT_WORKSPACE_INSTRUCTIONS_FILES: [&str; 3] =
    [WORKSPACE_INSTRUCTIONS_FILE_NAME, "AGENTS.md", "CLAUDE.md"];
const INSTRUCTIONS_SIZE_WARN_THRESHOLD: usize = 24_000;

#[derive(Debug, Clone)]
pub struct WorkspaceInstructions {
    pub path: PathBuf,
    pub content: String,
}

pub fn default_workspace_instructions_files() -> Vec<String> {
    DEFAULT_WORKSPACE_INSTRUCTIONS_FILES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub fn discover_workspace_instructions(
    start: &Path,
    file_names: &[String],
) -> Option<WorkspaceInstructions> {
    for dir in start.ancestors() {
        for name in file_names {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            match fs::read_to_string(&candidate) {
                Ok(content) if !content.trim().is_empty() => {
                    return Some(WorkspaceInstructions {
                        path: candidate,
                        content,
                    });
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "failed to read workspace instructions at {}: {e}",
                    candidate.display()
                ),
            }
        }
    }
    None
}

pub fn build_instructions_section(instructions: &WorkspaceInstructions) -> String {
    let char_count = instructions.content.chars().count();
    if char_count > INSTRUCTIONS_SIZE_WARN_THRESHOLD {
        warn!(
            "workspace instructions at {} are large ({char_count} chars); \
             consider moving detail into workspace memory drill files",
            instructions.path.display()
        );
    }

    format!(
        "<workspace_instructions source=\"{}\">\n{}\n</workspace_instructions>",
        instructions.path.display(),
        instructions.content.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, time};
    use time::SystemTime;

    fn temp_root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-instructions-{label}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn defaults() -> Vec<String> {
        default_workspace_instructions_files()
    }

    #[test]
    fn discovery_returns_none_when_no_file_exists() {
        let root = temp_root("none");

        assert!(discover_workspace_instructions(&root, &defaults()).is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_finds_coyote_md() {
        let root = temp_root("coyote");
        fs::write(root.join("COYOTE.md"), "coyote instructions").unwrap();

        let found = discover_workspace_instructions(&root, &defaults()).unwrap();
        assert_eq!(found.path, root.join("COYOTE.md"));
        assert_eq!(found.content, "coyote instructions");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_falls_back_to_agents_md_then_claude_md() {
        let root = temp_root("fallback");
        fs::write(root.join("CLAUDE.md"), "claude instructions").unwrap();

        let found = discover_workspace_instructions(&root, &defaults()).unwrap();
        assert_eq!(found.path, root.join("CLAUDE.md"));

        fs::write(root.join("AGENTS.md"), "agents instructions").unwrap();
        let found = discover_workspace_instructions(&root, &defaults()).unwrap();
        assert_eq!(found.path, root.join("AGENTS.md"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_prefers_coyote_md_over_fallbacks() {
        let root = temp_root("precedence");
        fs::write(root.join("COYOTE.md"), "coyote").unwrap();
        fs::write(root.join("AGENTS.md"), "agents").unwrap();
        fs::write(root.join("CLAUDE.md"), "claude").unwrap();

        let found = discover_workspace_instructions(&root, &defaults()).unwrap();
        assert_eq!(found.path, root.join("COYOTE.md"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_walks_up_from_nested_dir() {
        let root = temp_root("walk_up");
        fs::write(root.join("AGENTS.md"), "root instructions").unwrap();
        let nested = root.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();

        let found = discover_workspace_instructions(&nested, &defaults()).unwrap();
        assert_eq!(found.path, root.join("AGENTS.md"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_prefers_closer_file_over_higher_priority_name_above() {
        let root = temp_root("depth_first");
        fs::write(root.join("COYOTE.md"), "root coyote").unwrap();
        let nested = root.join("packages").join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("CLAUDE.md"), "nested claude").unwrap();

        let found = discover_workspace_instructions(&nested, &defaults()).unwrap();
        assert_eq!(found.path, nested.join("CLAUDE.md"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_skips_empty_files() {
        let root = temp_root("empty");
        fs::write(root.join("COYOTE.md"), "  \n").unwrap();
        fs::write(root.join("AGENTS.md"), "real content").unwrap();

        let found = discover_workspace_instructions(&root, &defaults()).unwrap();
        assert_eq!(found.path, root.join("AGENTS.md"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_honors_custom_file_chain() {
        let root = temp_root("custom");
        fs::write(root.join("CLAUDE.md"), "claude").unwrap();

        let only_agents = vec!["AGENTS.md".to_string()];
        assert!(discover_workspace_instructions(&root, &only_agents).is_none());

        let empty: Vec<String> = vec![];
        assert!(discover_workspace_instructions(&root, &empty).is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_section_wraps_content_with_source_path() {
        let instructions = WorkspaceInstructions {
            path: PathBuf::from("/ws/COYOTE.md"),
            content: "Do the thing.\n".into(),
        };

        let section = build_instructions_section(&instructions);
        assert!(section.starts_with("<workspace_instructions source=\"/ws/COYOTE.md\">"));
        assert!(section.contains("Do the thing."));
        assert!(section.ends_with("</workspace_instructions>"));
    }
}
