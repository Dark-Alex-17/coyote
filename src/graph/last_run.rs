use chrono::Utc;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use super::logging::route_suffix;

#[derive(Debug)]
pub struct GraphRunResult {
    pub output: String,
    pub final_state: HashMap<String, Value>,
    pub trace: Vec<TraceStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceStep {
    pub node_id: String,
    pub node_type: String,
    pub visit: usize,
    pub outcome: TraceOutcome,
    pub elapsed_ms: u64,
    /// The route was the node's `fallback:` edge, taken because the node
    /// failed. Traces stored before the field existed load as `false`.
    #[serde(default)]
    pub fell_back: bool,
}

/// A node failure not absorbed by `fallback:` never reaches the trace: the
/// run returns the error before the step is recorded, so a stored trace holds
/// only completed steps. An absorbed failure is recorded as `Routed` to the
/// fallback target, with the step's `fell_back` set so it can be told from a
/// clean route to that node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceOutcome {
    Routed { to: String },
    End,
}

/// Persisted record of a session's most recent graph run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub version: u32,
    pub completed_at: chrono::DateTime<Utc>,
    pub prompt: String,
    pub output: String,
    /// Final state of the keys the graph exposes, in declaration order;
    /// a key never written by the run is recorded as null.
    pub exposed: IndexMap<String, Value>,
    pub trace: Vec<TraceStep>,
    /// Session message count when the run was stored, so an undo of the
    /// exchange that produced it can be told apart from a later one.
    /// Stamped by the session when the run is stored (after its exchange is
    /// saved); capture leaves it 0.
    #[serde(default)]
    pub recorded_at_message_count: usize,
}

impl LastRun {
    pub fn capture(result: &GraphRunResult, prompt: &str, expose: &[String]) -> LastRun {
        let exposed = expose
            .iter()
            .map(|key| {
                let value = result.final_state.get(key).cloned().unwrap_or(Value::Null);
                (key.clone(), value)
            })
            .collect();
        LastRun {
            version: 1,
            completed_at: Utc::now(),
            prompt: prompt.to_string(),
            output: result.output.clone(),
            exposed,
            trace: result.trace.clone(),
            recorded_at_message_count: 0,
        }
    }

    /// The output and exposed state are fenced; trace node ids and node
    /// types are operator-authored and are rendered as-is.
    pub fn render(&self) -> String {
        let fence = fence_for(&self.output);
        let mut out = format!(
            "Last graph run (completed {})\n\n### Output\n{fence}\n{}\n{fence}\n",
            self.completed_at.to_rfc3339(),
            self.output
        );
        if !self.exposed.is_empty() {
            let yaml = serde_yaml::to_string(&self.exposed)
                .unwrap_or_else(|e| format!("unable to render exposed state: {e}\n"));
            let yaml = yaml.trim_end_matches('\n');
            let fence = fence_for(yaml);
            out.push_str(&format!("\n### Exposed state\n{fence}\n{yaml}\n{fence}\n"));
        }
        out.push_str("\n### Trace\n");
        for step in &self.trace {
            let target = match &step.outcome {
                TraceOutcome::Routed { to } => to.as_str(),
                TraceOutcome::End => "END",
            };
            out.push_str(&format!(
                "{}. {} ({}){} [{} ms]\n",
                step.visit,
                step.node_id,
                step.node_type,
                route_suffix(Some(target), step.fell_back),
                step.elapsed_ms
            ));
        }
        out
    }
}

/// One backtick more than the longest run inside the fenced text, so the
/// text cannot close its own fence.
fn fence_for(text: &str) -> String {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest_run + 1).max(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(node_id: &str, node_type: &str, visit: usize, outcome: TraceOutcome) -> TraceStep {
        TraceStep {
            node_id: node_id.to_string(),
            node_type: node_type.to_string(),
            visit,
            outcome,
            elapsed_ms: 7,
            fell_back: false,
        }
    }

    fn sample_result() -> GraphRunResult {
        GraphRunResult {
            output: "final answer".to_string(),
            final_state: HashMap::from([
                ("wrote".to_string(), json!("yes")),
                ("n".to_string(), json!(2)),
            ]),
            trace: vec![
                step(
                    "a",
                    "script",
                    1,
                    TraceOutcome::Routed {
                        to: "b".to_string(),
                    },
                ),
                step("b", "end", 1, TraceOutcome::End),
            ],
        }
    }

    #[test]
    fn capture_stores_prompt_and_exposed_keys_in_declaration_order() {
        let result = sample_result();
        let expose = vec!["n".to_string(), "missing".to_string(), "wrote".to_string()];

        let run = LastRun::capture(&result, "do the thing", &expose);

        assert_eq!(run.version, 1);
        assert_eq!(run.prompt, "do the thing");
        assert_eq!(run.output, "final answer");
        assert_eq!(run.trace, result.trace);
        assert_eq!(run.recorded_at_message_count, 0);
        let exposed: Vec<(&str, &Value)> =
            run.exposed.iter().map(|(k, v)| (k.as_str(), v)).collect();
        assert_eq!(
            exposed,
            vec![
                ("n", &json!(2)),
                ("missing", &Value::Null),
                ("wrote", &json!("yes")),
            ]
        );
    }

    #[test]
    fn render_lists_output_exposed_state_and_one_line_per_step() {
        let mut run = LastRun::capture(&sample_result(), "do the thing", &["n".to_string()]);
        run.completed_at = chrono::DateTime::from_timestamp(0, 0).unwrap();
        run.trace.push(step("a", "script", 2, TraceOutcome::End));

        let rendered = run.render();

        assert!(rendered.starts_with("Last graph run (completed 1970-01-01T00:00:00+00:00)\n"));
        assert!(rendered.contains("### Output\n```\nfinal answer\n```\n"));
        assert!(rendered.contains("### Exposed state\n```\nn: 2\n```\n\n### Trace\n"));
        assert!(rendered.contains("### Trace\n"));
        assert!(rendered.contains("1. a (script) -> b [7 ms]\n"));
        assert!(rendered.contains("1. b (end) -> END [7 ms]\n"));
        assert!(rendered.contains("2. a (script) -> END [7 ms]\n"));
        assert!(rendered.is_ascii());
    }

    /// A run record stored before the fallback bit existed (no `fell_back` key
    /// on its steps) loads as a plain route and renders the same tail it did
    /// then, and every rendered tail is byte-equal to the console narration's
    /// `route_suffix` for the same (target, bit).
    #[test]
    fn usage_probe_a_pre_ruling_trace_renders_plain_tails_equal_to_the_narration() {
        let legacy = "\
version: 1
completed_at: 1970-01-01T00:00:00Z
prompt: run
output: done
exposed: {}
trace:
- node_id: a
  node_type: script
  visit: 1
  outcome: !routed
    to: b
  elapsed_ms: 7
- node_id: b
  node_type: end
  visit: 1
  outcome: end
  elapsed_ms: 7
recorded_at_message_count: 0
";
        let run: LastRun = serde_yaml::from_str(legacy).unwrap();
        assert!(run.trace.iter().all(|s| !s.fell_back), "{:?}", run.trace);

        let rendered = run.render();
        assert!(
            rendered.contains("1. a (script) -> b [7 ms]\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("1. b (end) -> END [7 ms]\n"),
            "{rendered}"
        );
        assert!(!rendered.contains("(fallback)"), "{rendered}");

        // The tail the record renders is the narration's own, for both arms.
        use super::super::logging::route_suffix;
        assert_eq!(route_suffix(Some("b"), false), " -> b");
        assert_eq!(route_suffix(Some("b"), true), " -> b (fallback)");
        assert!(rendered.contains(&format!("(script){} [", route_suffix(Some("b"), false))));
    }

    #[test]
    fn render_marks_a_fallback_hop_with_the_console_suffix() {
        let mut result = sample_result();
        result.trace[0].fell_back = true;
        let run = LastRun::capture(&result, "do the thing", &[]);

        let rendered = run.render();

        assert!(
            rendered.contains("1. a (script) -> b (fallback) [7 ms]\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("1. b (end) -> END [7 ms]\n"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("(fallback)").count(), 1, "{rendered}");
    }

    #[test]
    fn render_omits_exposed_section_when_nothing_is_exposed() {
        let run = LastRun::capture(&sample_result(), "do the thing", &[]);

        let rendered = run.render();

        assert!(!rendered.contains("### Exposed state"));
        assert!(rendered.contains("### Output\n```\nfinal answer\n```\n\n### Trace\n"));
    }

    #[test]
    fn render_widens_the_fence_past_backtick_runs_in_the_output() {
        let mut result = sample_result();
        result.output = "before\n```\ninner\n`````\nafter".to_string();
        let run = LastRun::capture(&result, "do the thing", &[]);

        let rendered = run.render();

        assert!(
            rendered.contains(
                "### Output\n``````\nbefore\n```\ninner\n`````\nafter\n``````\n\n### Trace\n"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn render_widens_the_exposed_state_fence_past_backticks_in_a_value() {
        let mut result = sample_result();
        result
            .final_state
            .insert("snippet".to_string(), json!("a\n```\nb\n````"));
        let run = LastRun::capture(&result, "do the thing", &["snippet".to_string()]);

        let rendered = run.render();

        let section = rendered
            .split("### Exposed state\n")
            .nth(1)
            .and_then(|rest| rest.split("\n### Trace\n").next())
            .unwrap();
        assert!(section.starts_with("`````\n"), "{section}");
        assert!(section.ends_with("\n`````\n"), "{section}");
        assert!(section.contains("````"), "{section}");
        assert!(
            !section.contains("``````"),
            "the fence is exactly one longer than the longest run: {section}"
        );
    }
}
