//! Script execution for `script`-type graph nodes.
//!
//! Scripts receive graph state via either `GRAPH_STATE` (inline JSON env var)
//! or `GRAPH_STATE_FILE` (path to a file containing the JSON) when state
//! exceeds [`super::MAX_STATE_SIZE_BYTES`]. Scripts MUST print a single JSON
//! object on stdout. The `_next` key (if present) is consumed for routing
//! and removed before the remaining keys are merged into state.

use super::state::{StateManager, StateRepresentation};
use super::types::ScriptNode;
use crate::function::Language;
use crate::utils::dimmed_text;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Executor for script nodes. `base_dir` is the directory script paths are
/// resolved against (typically the owning agent's data directory) and is
/// also used as the child process's working directory.
pub struct ScriptExecutor {
    base_dir: PathBuf,
}

impl ScriptExecutor {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Run the script, merge its JSON output into state (extracting `_next`
    /// for routing), and then apply any `state_updates` templates. Returns
    /// the routing decision from `_next`, or `None` if the script did not
    /// emit one (in which case the executor falls back to `Node.next`).
    pub async fn execute(
        &self,
        node: &ScriptNode,
        state_manager: &mut StateManager,
    ) -> Result<Option<String>> {
        let script_path = self.base_dir.join(&node.script);
        if !script_path.exists() {
            bail!("Script file not found: '{}'", script_path.display());
        }

        eprintln!(
            "{}",
            dimmed_text(&format!("▸   running script '{}'", node.script))
        );

        let language = detect_language(&script_path)?;
        let state_repr = state_manager.serialize_state()?;

        let mut cmd = build_command(language, &script_path)?;
        cmd.current_dir(&self.base_dir);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        match &state_repr {
            StateRepresentation::Inline(json) => {
                cmd.env("GRAPH_STATE", json);
            }
            StateRepresentation::File(path) => {
                cmd.env("GRAPH_STATE_FILE", path);
            }
        }

        let timeout_dur = Duration::from_secs(node.timeout);
        let output = timeout(timeout_dur, cmd.output())
            .await
            .with_context(|| {
                format!(
                    "Script '{}' timed out after {}s",
                    script_path.display(),
                    node.timeout
                )
            })?
            .with_context(|| {
                format!(
                    "Failed to spawn script process for '{}'",
                    script_path.display()
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Script '{}' failed with exit code {:?}:\n{}",
                script_path.display(),
                output.status.code(),
                stderr.trim()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json_output = stdout.trim();
        if json_output.is_empty() {
            bail!(
                "Script '{}' produced no output (scripts must emit a single JSON object on stdout)",
                script_path.display()
            );
        }

        let next = state_manager
            .merge_script_output(json_output)
            .with_context(|| {
                format!(
                    "Failed to merge output from script '{}'",
                    script_path.display()
                )
            })?;

        if let Ok(parsed) = serde_json::from_str::<serde_json::Map<String, Value>>(json_output) {
            let keys: Vec<&str> = parsed
                .keys()
                .filter(|k| k.as_str() != "_next")
                .map(|s| s.as_str())
                .collect();
            if !keys.is_empty() {
                eprintln!(
                    "{}",
                    dimmed_text(&format!("▸   merged: {}", keys.join(", ")))
                );
            }
            if let Some(n) = &next {
                eprintln!("{}", dimmed_text(&format!("▸   script set _next = '{n}'")));
            }
        }

        apply_state_updates(node, state_manager);

        Ok(next)
    }
}

fn apply_state_updates(node: &ScriptNode, state_manager: &mut StateManager) {
    let Some(updates) = &node.state_updates else {
        return;
    };
    for (key, template) in updates {
        let value = state_manager.interpolate_lenient(template);
        state_manager
            .state_mut()
            .set(key.clone(), Value::String(value));
    }
}

fn detect_language(script_path: &Path) -> Result<Language> {
    let ext = script_path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| anyhow!("Script has no file extension: '{}'", script_path.display()))?
        .to_string();
    match Language::from(&ext) {
        Language::Unsupported => bail!(
            "Unsupported script extension '.{}' for '{}'",
            ext,
            script_path.display()
        ),
        lang => Ok(lang),
    }
}

fn build_command(language: Language, script_path: &Path) -> Result<Command> {
    let (program, prefix_args) = language.direct_invoker().ok_or_else(|| {
        anyhow!(
            "No direct invoker available for script '{}'",
            script_path.display()
        )
    })?;
    let mut cmd = Command::new(program);
    for arg in prefix_args {
        cmd.arg(arg);
    }
    cmd.arg(script_path);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::super::MAX_STATE_SIZE_BYTES;
    use super::*;
    use crate::utils::temp_file;
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;

    fn cmd_available(name: &str) -> bool {
        which::which(name).is_ok()
    }

    fn write_script(contents: &str, ext: &str) -> (PathBuf, PathBuf) {
        let dir = temp_file("-graph-script-test-", "");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("script.{ext}"));
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    fn node_for(script_filename: &str, timeout: u64) -> ScriptNode {
        ScriptNode {
            script: script_filename.into(),
            state_updates: None,
            fallback: None,
            timeout,
        }
    }

    #[tokio::test]
    async fn bash_script_merges_json_output_into_state() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
echo '{"quality": 0.85, "issues": 3, "_next": "approve"}'
"#,
            "sh",
        );
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let next = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(next.as_deref(), Some("approve"));
        assert_eq!(state.state().get("quality"), Some(&json!(0.85)));
        assert_eq!(state.state().get("issues"), Some(&json!(3)));
        assert!(state.state().get("_next").is_none());
        cleanup(&dir);
    }

    #[tokio::test]
    async fn bash_script_can_read_state_from_env() {
        if !cmd_available("bash") || !cmd_available("python3") {
            eprintln!("skipping: bash or python3 not available");
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
NAME=$(python3 -c 'import json,os; print(json.loads(os.environ["GRAPH_STATE"])["name"])')
printf '{"greeting": "hello %s"}' "$NAME"
"#,
            "sh",
        );
        let mut initial = HashMap::new();
        initial.insert("name".into(), json!("alice"));
        let mut state = StateManager::new(initial);
        let executor = ScriptExecutor::new(&dir);
        let _ = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(state.state().get("greeting"), Some(&json!("hello alice")));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn script_without_next_returns_none() {
        if !cmd_available("bash") {
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
echo '{"ok": true}'
"#,
            "sh",
        );
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let next = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap();
        assert!(next.is_none());
        assert_eq!(state.state().get("ok"), Some(&json!(true)));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn state_updates_apply_after_json_merge() {
        if !cmd_available("bash") {
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
echo '{"raw": "hello"}'
"#,
            "sh",
        );
        let mut node = node_for(path.file_name().unwrap().to_str().unwrap(), 5);
        let mut updates = HashMap::new();
        updates.insert("decorated".into(), "[{{raw}}]".into());
        node.state_updates = Some(updates);

        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        executor.execute(&node, &mut state).await.unwrap();

        assert_eq!(state.state().get("raw"), Some(&json!("hello")));
        assert_eq!(state.state().get("decorated"), Some(&json!("[hello]")));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn missing_script_file_errors_before_spawning() {
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(std::env::temp_dir());
        let err = executor
            .execute(&node_for("__does_not_exist__.sh", 5), &mut state)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Script file not found"), "got: {err}");
    }

    #[tokio::test]
    async fn empty_stdout_errors() {
        if !cmd_available("bash") {
            return;
        }
        let (dir, path) = write_script("#!/bin/bash\n", "sh");
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let err = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("produced no output"), "got: {err}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn non_json_output_errors() {
        if !cmd_available("bash") {
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
echo "not json at all"
"#,
            "sh",
        );
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let err = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge output"), "got: {err}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn non_zero_exit_errors_and_includes_stderr() {
        if !cmd_available("bash") {
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
echo "bad happened" >&2
exit 7
"#,
            "sh",
        );
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let err = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit code"), "got: {err}");
        assert!(err.contains("bad happened"), "got: {err}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn execution_timeout_is_enforced() {
        if !cmd_available("bash") {
            return;
        }
        let (dir, path) = write_script(
            r#"#!/bin/bash
sleep 5
echo '{"ok":true}'
"#,
            "sh",
        );
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let err = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 1),
                &mut state,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "got: {err}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn large_state_is_delivered_via_file_env_var() {
        if !cmd_available("bash") || !cmd_available("python3") {
            return;
        }
        let big = "x".repeat(MAX_STATE_SIZE_BYTES + 1024);
        let mut initial = HashMap::new();
        initial.insert("blob".into(), json!(big));

        let (dir, path) = write_script(
            r#"#!/bin/bash
if [ -n "$GRAPH_STATE_FILE" ]; then
    LEN=$(python3 -c 'import json,os; print(len(json.load(open(os.environ["GRAPH_STATE_FILE"]))["blob"]))')
    printf '{"blob_len": %s, "via_file": true}' "$LEN"
elif [ -n "$GRAPH_STATE" ]; then
    echo '{"via_file": false}'
fi
"#,
            "sh",
        );

        let mut state = StateManager::new(initial);
        let executor = ScriptExecutor::new(&dir);
        executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 10),
                &mut state,
            )
            .await
            .unwrap();

        assert_eq!(state.state().get("via_file"), Some(&json!(true)));
        let len = state.state().get("blob_len").unwrap().as_i64().unwrap();
        assert_eq!(len as usize, big.len());
        cleanup(&dir);
    }

    #[tokio::test]
    async fn python_script_can_emit_routing_and_state() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let (dir, path) = write_script(
            r#"import os, json
state = json.loads(os.environ["GRAPH_STATE"])
print(json.dumps({
    "_next": "next_node",
    "doubled": state.get("n", 0) * 2,
}))
"#,
            "py",
        );
        let mut initial = HashMap::new();
        initial.insert("n".into(), json!(21));
        let mut state = StateManager::new(initial);

        let executor = ScriptExecutor::new(&dir);
        let next = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(next.as_deref(), Some("next_node"));
        assert_eq!(state.state().get("doubled"), Some(&json!(42)));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn unknown_extension_is_rejected() {
        let (dir, path) = write_script("echo hi", "xyz");
        let mut state = StateManager::new(HashMap::new());
        let executor = ScriptExecutor::new(&dir);
        let err = executor
            .execute(
                &node_for(path.file_name().unwrap().to_str().unwrap(), 5),
                &mut state,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Unsupported script extension '.xyz'"),
            "got: {err}"
        );
        cleanup(&dir);
    }
}
