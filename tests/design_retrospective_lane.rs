//! Pins the design-retrospective agent and its wiring into the closing steps
//! of sisyphus and architect: the agent stays read-only with its output
//! contract, both orchestrators can spawn it, and the architect's Phase F
//! numbering survives the inserted step.

use std::fs;
use std::path::Path;

fn agent_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn agent_config(name: &str) -> serde_yaml::Value {
    let rel = format!("{name}/config.yaml");
    serde_yaml::from_str(&agent_file(&rel)).unwrap_or_else(|e| panic!("parse {rel}: {e}"))
}

fn string_list(config: &serde_yaml::Value, key: &str) -> Vec<String> {
    config[key]
        .as_sequence()
        .unwrap_or_else(|| panic!("config has no {key} list"))
        .iter()
        .map(|v| v.as_str().expect("list entries are strings").to_string())
        .collect()
}

fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("no {start:?} in the text"));
    let to = text[from..]
        .find(end)
        .map(|i| from + i)
        .unwrap_or_else(|| panic!("no {end:?} after {start:?}"));
    &text[from..to]
}

#[test]
fn design_retrospective_agent_ships_read_only_with_explore_as_its_only_child() {
    for file in ["config.yaml", "instructions.md", "README.md", "tools.sh"] {
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/agents/design-retrospective")
                .join(file)
                .is_file(),
            "design-retrospective must ship {file}"
        );
    }
    let config = agent_config("design-retrospective");
    let variables: Vec<&str> = config["variables"]
        .as_sequence()
        .expect("config has a variables list")
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(
        variables.contains(&"project_dir"),
        "design-retrospective must declare project_dir, got {variables:?}"
    );
    let tools = string_list(&config, "global_tools");
    for write_tool in ["fs_write.sh", "fs_patch.sh"] {
        assert!(
            !tools.iter().any(|t| t == write_tool),
            "design-retrospective is read-only and must not grant {write_tool}, got {tools:?}"
        );
    }
    assert!(
        tools.iter().any(|t| t == "execute_command.sh"),
        "design-retrospective needs execute_command.sh to read git history, got {tools:?}"
    );
    assert_eq!(
        string_list(&config, "spawnable_agents"),
        ["explore"],
        "design-retrospective spawns explore and nothing else"
    );
    assert!(
        string_list(&config, "enabled_skills")
            .iter()
            .any(|s| s == "codebase-design"),
        "design-retrospective must enable codebase-design for its vocabulary"
    );
}

#[test]
fn design_retrospective_prompt_carries_the_contract_and_the_core_question() {
    let text = agent_file("design-retrospective/instructions.md");
    for anchor in [
        "DESIGN_RETROSPECTIVE: REFACTORS <n>",
        "DESIGN_RETROSPECTIVE: CLEAN",
        "## Refactor items",
        "## Choices I would make again",
        "If I built this again, would I make the same choices",
        "These are examples to prompt judgement, not a checklist",
        "Never modify repository files.",
        "Never ask the user",
    ] {
        assert!(
            text.contains(anchor),
            "design-retrospective/instructions.md lost anchor {anchor:?}"
        );
    }
    assert!(
        text.is_ascii(),
        "design-retrospective/instructions.md must stay ASCII"
    );
}

#[test]
fn both_orchestrators_can_spawn_the_design_retrospective() {
    for orchestrator in ["sisyphus", "architect"] {
        let spawnable = string_list(&agent_config(orchestrator), "spawnable_agents");
        assert!(
            spawnable.iter().any(|s| s == "design-retrospective"),
            "{orchestrator} spawnable_agents must include design-retrospective, got {spawnable:?}"
        );
    }
}

#[test]
fn sisyphus_runs_the_retrospective_in_both_cadences_before_whetstone() {
    let text = agent_file("sisyphus/instructions.md");
    let retro = section(
        &text,
        "### Design retrospective (after a large body of work)",
        "### Continuous improvement (whetstone)",
    );
    assert!(
        !retro.contains("<!-- when:") && !retro.contains("<!-- end -->"),
        "the design retrospective section must sit outside every conditional block"
    );
    for anchor in [
        "\"Large\" is your judgement, not a threshold",
        "`Design retrospective:`",
        "`retro-declined: <item> -- <reason>`",
        "agent__spawn --agent design-retrospective",
        "`Design question:`",
        "Run it ONCE per run",
    ] {
        assert!(
            retro.contains(anchor),
            "sisyphus design retrospective section lost anchor {anchor:?}"
        );
    }
    let batch = section(
        &text,
        "### Review cadence: batch (set by the parent orchestrator)",
        "<!-- end -->",
    );
    assert!(
        batch.contains("design retrospective"),
        "the batch cadence must hand the design retrospective to the parent"
    );
}

#[test]
fn architect_runs_the_retrospective_before_the_hygiene_pass() {
    let text = agent_file("architect/instructions.md");
    let retro = section(
        &text,
        "2. **Run-closing design retrospective.**",
        "3. **Run-closing hygiene pass.**",
    );
    for anchor in [
        "`TASK-NNN-run-refactor`",
        "agent__spawn --agent design-retrospective",
        "`retro-declined: <item> -- <reason>`",
    ] {
        assert!(
            retro.contains(anchor),
            "architect design retrospective step lost anchor {anchor:?}"
        );
    }
    let hygiene = section(
        &text,
        "3. **Run-closing hygiene pass.**",
        "4. **Roll up follow-ups",
    );
    assert!(
        hygiene.contains("`TASK-NNN-run-refactor`"),
        "the hygiene task must also be blocked by the refactor task"
    );
    let brief = section(&text, "   ## MUST NOT DO", "   ## CONTEXT");
    assert!(
        brief.contains("Do NOT run the design retrospective"),
        "the Phase E Sisyphus brief must hand the design retrospective to the orchestrator"
    );
}

#[test]
fn architect_phase_f_steps_are_numbered_one_to_nine() {
    let text = agent_file("architect/instructions.md");
    let phase_f = section(&text, "### Phase F", "\n## Durable state");
    let numbers: Vec<u32> = phase_f
        .lines()
        .filter_map(|l| l.split_once(". ").and_then(|(n, _)| n.parse().ok()))
        .collect();
    assert_eq!(
        numbers,
        (1..=9).collect::<Vec<_>>(),
        "Phase F top-level steps must run 1..9 without gaps"
    );
    for anchor in [
        "Steps 1-5 run at EVERY group's close-out",
        "steps 6-9 run ONCE",
        "- Steps 6-9 run once, at the end",
        "for steps 6-9, after the last group's",
    ] {
        assert!(
            phase_f.contains(anchor),
            "multi-strategy Phase F text lost renumbered anchor {anchor:?}"
        );
    }
    for stale in ["Steps 1-4", "steps 5-8", "Steps 5-8"] {
        assert!(
            !phase_f.contains(stale),
            "Phase F still carries the pre-renumber range {stale:?}"
        );
    }
}
