use super::state::StateManager;
use serde_json::Value;
use std::collections::HashMap;

pub(super) const OUTPUT_KEY: &str = "output";

/// Merge the `output` keys declared under `schema["properties"]` and apply `state_updates` with
/// `{{output}}` bound to the full, unfiltered `output` for the duration of interpolation only.
///
/// The schema is the graph author's declaration of what the model may write to state; any other
/// key in the output is model- (and therefore prompt-) controlled and is dropped rather than
/// merged implicitly. No `schema`, or a schema without an object `properties`, merges nothing.
/// Authors who want an undeclared key can still lift it explicitly via
/// `state_updates: { key: '{{output.key}}' }`.
///
/// An explicit `output` key in `updates` is honored; otherwise `output` is restored to its prior
/// value, or removed again if it was absent.
pub(super) fn apply(
    state_manager: &mut StateManager,
    output: &Value,
    schema: Option<&Value>,
    updates: Option<&HashMap<String, String>>,
) {
    if let Some(declared) = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        && let Some(obj) = output.as_object()
    {
        for (k, v) in obj {
            if declared.contains_key(k) {
                state_manager.state_mut().set(k.clone(), v.clone());
            }
        }
    }

    let Some(updates) = updates else {
        return;
    };
    let prev_output = state_manager.state().get(OUTPUT_KEY).cloned();
    state_manager
        .state_mut()
        .set(OUTPUT_KEY.into(), output.clone());

    let computed: Vec<(String, Value)> = updates
        .iter()
        .map(|(key, template)| {
            (
                key.clone(),
                Value::String(state_manager.interpolate_lenient(template)),
            )
        })
        .collect();

    match prev_output {
        Some(prev) => state_manager.state_mut().set(OUTPUT_KEY.into(), prev),
        None => {
            state_manager.state_mut().remove(OUTPUT_KEY);
        }
    }

    for (key, value) in computed {
        state_manager.state_mut().set(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn updates(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn explicit_output_state_update_is_honored() {
        let u = updates(&[("output", "{{output}}")]);
        let mut state = manager_with(&[("output", json!("stale"))]);

        apply(&mut state, &json!("the completion"), None, Some(&u));

        assert_eq!(
            state.state().get(OUTPUT_KEY),
            Some(&json!("the completion"))
        );
    }

    #[test]
    fn explicit_output_is_honored_alongside_other_updates() {
        let u = updates(&[("output", "{{output}}"), ("summary", "got: {{output}}")]);
        let mut state = manager_with(&[]);

        apply(&mut state, &json!("done"), None, Some(&u));

        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("done")));
        assert_eq!(state.state().get("summary"), Some(&json!("got: done")));
    }

    #[test]
    fn no_updates_is_a_noop() {
        let mut state = manager_with(&[("keep", json!(1))]);

        apply(&mut state, &json!("ignored"), None, None);

        assert_eq!(state.state().get("keep"), Some(&json!(1)));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn output_is_restored_to_previous_value() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[("output", json!("preserved"))]);

        apply(&mut state, &json!("new"), None, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("preserved")));
    }

    #[test]
    fn output_is_removed_when_absent() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[]);

        apply(&mut state, &json!("new"), None, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn explicit_null_prior_output_is_restored_as_null() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[("output", json!(null))]);

        apply(&mut state, &json!("new"), None, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!(null)));
    }

    fn schema_with(keys: &[&str]) -> Value {
        let props: serde_json::Map<String, Value> = keys
            .iter()
            .map(|k| ((*k).to_string(), json!({"type": "string"})))
            .collect();
        json!({"type": "object", "properties": props})
    }

    #[test]
    fn schema_merges_declared_top_level_keys() {
        let schema = schema_with(&["summary", "score"]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"summary": "s", "score": 7}),
            Some(&schema),
            None,
        );

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert_eq!(state.state().get("score"), Some(&json!(7)));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn schema_drops_undeclared_keys_and_preserves_existing_state() {
        let schema = schema_with(&["summary"]);
        let mut state = manager_with(&[("verification_commands", json!(["cargo test"]))]);

        apply(
            &mut state,
            &json!({"summary": "s", "verification_commands": ["rm -rf /"]}),
            Some(&schema),
            None,
        );

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert_eq!(
            state.state().get("verification_commands"),
            Some(&json!(["cargo test"]))
        );
    }

    #[test]
    fn none_schema_merges_nothing() {
        let mut state = manager_with(&[]);

        apply(&mut state, &json!({"summary": "s"}), None, None);

        assert!(state.state().get("summary").is_none());
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn schema_without_properties_merges_nothing() {
        let schema = json!({"type": "object"});
        let mut state = manager_with(&[]);

        apply(&mut state, &json!({"summary": "s"}), Some(&schema), None);

        assert!(state.state().get("summary").is_none());
    }

    #[test]
    fn state_updates_still_see_undeclared_output_keys() {
        let schema = schema_with(&["summary"]);
        let u = updates(&[("lifted", "{{output.extra}}")]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"summary": "s", "extra": "e"}),
            Some(&schema),
            Some(&u),
        );

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert!(state.state().get("extra").is_none());
        assert_eq!(state.state().get("lifted"), Some(&json!("e")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn object_output_is_not_merged_without_schema() {
        let u = updates(&[("ctx", "{{output.context}}")]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"context": "c", "sources": ["a"]}),
            None,
            Some(&u),
        );

        assert!(state.state().get("context").is_none());
        assert!(state.state().get("sources").is_none());
        assert_eq!(state.state().get("ctx"), Some(&json!("c")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    // The coder's self_review node discloses new-to-repo patterns through
    // `design_notes`, and end_success prints them as `Design: ...` after
    // `Tests: passed`. Drive the shipped graph through the merge the engine
    // performs and the end-node render: a disclosed note reaches the final
    // output in that position, a second note landing as a continuation line
    // under it; the key is required so a model cannot omit it, and when the
    // review never runs (its fallback edge lands on self_review_fault, which
    // emits only the trail's ASK lines) a trail without one leaves the seeded
    // empty disclosure to render as a bare line; an ASK carried as the text
    // after `Design: ` renders as exactly one `Design: ASK -- ...` line; a
    // key the self_review schema does not declare (plan_summary belongs to
    // analyze_request) cannot be rewritten from the review; a null from the
    // review would render as the literal word, which is why
    // route_review_result re-emits the key as a string; and a schema without
    // `design_notes` would drop the note, so the declaration is what carries
    // it.
    #[test]
    fn usage_probe_shipped_coder_design_notes_flow_from_self_review_to_end_success() {
        use crate::graph::GraphParser;
        use crate::graph::types::NodeType;

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/coder");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("shipped coder graph parses");
        assert_eq!(
            graph.initial_state.get("design_notes"),
            Some(&json!("")),
            "initial_state seeds design_notes so the end template renders on every path"
        );
        let NodeType::Llm(review) = &graph.nodes["self_review"].node_type else {
            panic!("self_review is an llm node");
        };
        let schema = review
            .output_schema
            .as_ref()
            .expect("self_review has a schema");
        assert!(
            schema["required"]
                .as_array()
                .expect("self_review lists required keys")
                .contains(&json!("design_notes")),
            "design_notes is required so the model cannot drop the disclosure"
        );
        let NodeType::End(end) = &graph.nodes["end_success"].node_type else {
            panic!("end_success is an end node");
        };

        let note = "Strategy: src/pay/strategy.rs; call sites src/pay/mod.rs, src/pay/cli.rs\n\
                    Table dispatch: src/pay/codes.rs; call site src/pay/strategy.rs";
        let mut state = StateManager::new(graph.initial_state.clone());
        state
            .state_mut()
            .set("plan_summary".into(), json!("add payment strategies"));
        apply(
            &mut state,
            &json!({
                "review_clean": true,
                "review_notes": "",
                "design_notes": note,
                "plan_summary": "rewritten by the review",
            }),
            Some(schema),
            review.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("design_notes"), Some(&json!(note)));
        assert_eq!(
            state.state().get("plan_summary"),
            Some(&json!("add payment strategies")),
            "self_review cannot rewrite a key its schema does not declare"
        );
        let rendered = state.interpolate_lenient(&end.output);
        let tests_at = rendered
            .find("Tests: passed\n")
            .expect("end_success reports the test gate");
        let design_at = rendered
            .find(&format!("Design: {note}"))
            .expect("end_success renders the disclosure verbatim, continuation line included");
        assert!(
            design_at > tests_at,
            "Design: follows Tests: passed, got:\n{rendered}"
        );
        assert!(
            rendered.starts_with("CODER_COMPLETE\n"),
            "the sentinel stays first: {rendered}"
        );

        // Review never ran and the trail held no ASK: the seed renders blank.
        let state = StateManager::new(graph.initial_state.clone());
        assert_eq!(state.state().get("design_notes"), Some(&json!("")));
        let rendered = state.interpolate_lenient(&end.output);
        assert!(
            rendered.lines().any(|l| l == "Design: "),
            "an empty disclosure renders as a bare `Design: ` line (the orchestrator \
             drops it), got:\n{rendered}"
        );
        assert!(
            !rendered.contains("{{"),
            "no template is left unrendered: {rendered}"
        );

        // An ASK is carried without its `Design: ` label (by the review or by
        // self_review_fault), so the template supplies the label exactly once.
        let mut state = StateManager::new(graph.initial_state.clone());
        apply(
            &mut state,
            &json!({"review_clean": true, "review_notes": "", "design_notes": "ASK -- q"}),
            Some(schema),
            review.state_updates.as_ref(),
        );
        let rendered = state.interpolate_lenient(&end.output);
        assert!(
            rendered.lines().any(|l| l == "Design: ASK -- q"),
            "the rendered ASK line carries one label, got:\n{rendered}"
        );
        assert!(
            !rendered.contains("Design: Design:"),
            "a doubled label would break the orchestrators' `ASK --` match: {rendered}"
        );

        // A null disclosure merges as null and would print as the word; the
        // route script's `// ""` guard is what turns it back into a bare line.
        let mut state = StateManager::new(graph.initial_state.clone());
        apply(
            &mut state,
            &json!({"review_clean": true, "review_notes": "", "design_notes": null}),
            Some(schema),
            review.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("design_notes"), Some(&json!(null)));
        assert!(
            state
                .interpolate_lenient(&end.output)
                .lines()
                .any(|l| l == "Design: null"),
            "an unguarded null renders as the literal word"
        );
        state.state_mut().set("design_notes".into(), json!(""));
        let rendered = state.interpolate_lenient(&end.output);
        assert!(
            rendered.lines().any(|l| l == "Design: "),
            "after the script guard the null renders as a bare `Design: ` line, got:\n{rendered}"
        );

        // Control: without the declaration the same output leaves the seed
        // untouched, so the schema property is what carries the disclosure.
        let mut undeclared = schema.clone();
        undeclared["properties"]
            .as_object_mut()
            .unwrap()
            .remove("design_notes")
            .expect("the shipped schema declares design_notes");
        let mut state = StateManager::new(graph.initial_state.clone());
        apply(
            &mut state,
            &json!({"review_clean": true, "review_notes": "", "design_notes": note}),
            Some(&undeclared),
            review.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("design_notes"), Some(&json!("")));
    }

    // The fix loop re-enters implement, and each pass overwrites
    // `last_node_output`, so a `Design:` heading written on the first pass
    // would be gone by the time self_review reads the summary. The implement
    // node's state_updates append every pass's output to `design_trail`,
    // which the self_review prompt shows under its own heading; the seed is
    // the empty string so the first pass renders without a dangling template.
    #[test]
    fn usage_probe_shipped_coder_design_trail_accumulates_across_implement_passes() {
        use crate::graph::GraphParser;
        use crate::graph::types::NodeType;

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/coder");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("shipped coder graph parses");
        assert_eq!(
            graph.initial_state.get("design_trail"),
            Some(&json!("")),
            "initial_state seeds design_trail so the first pass appends to an empty string"
        );
        let NodeType::Llm(implement) = &graph.nodes["implement"].node_type else {
            panic!("implement is an llm node");
        };
        let NodeType::Llm(review) = &graph.nodes["self_review"].node_type else {
            panic!("self_review is an llm node");
        };
        assert!(
            review.prompt.contains("{{design_trail}}"),
            "self_review reads the trail: {}",
            review.prompt
        );

        let first = "Added the strategies.\nDesign: ASK -- fold the two parsers into one trait?";
        let second = "Fixed the failing test.";
        let mut state = StateManager::new(graph.initial_state.clone());
        apply(
            &mut state,
            &json!(first),
            None,
            implement.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("last_node_output"), Some(&json!(first)));
        assert_eq!(
            state.state().get("design_trail"),
            Some(&json!(format!("\n{first}")))
        );
        apply(
            &mut state,
            &json!(second),
            None,
            implement.state_updates.as_ref(),
        );
        assert_eq!(
            state.state().get("last_node_output"),
            Some(&json!(second)),
            "the latest pass overwrites the summary the self_review reconciles first"
        );
        assert_eq!(
            state.state().get("design_trail"),
            Some(&json!(format!("\n{first}\n{second}"))),
            "the trail keeps the first pass's ASK line under the second pass's summary"
        );
        let rendered = state.interpolate_lenient(&review.prompt);
        assert!(
            rendered.contains(&format!("## Implementer's summary\n{second}\n")),
            "the latest summary heads the prompt: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{first}\n{second}\n")),
            "every pass follows under the trail heading: {rendered}"
        );
    }

    // The step-runner twin: write_handoff carries the coder's `Design:` block
    // as `design_decisions` (required, though a null still merges, which is
    // why check_handoff re-emits the key as a string) and end_success prints
    // it under its own `Design decisions:` heading after
    // the downstream updates. The node's own state_updates write only
    // `handoff_failure`, so the disclosure travels through the schema merge
    // alone; every path that skips write_handoff renders the seeded empty
    // block rather than a dangling template.
    #[test]
    fn usage_probe_shipped_step_runner_design_decisions_flow_from_write_handoff_to_end_success() {
        use crate::graph::GraphParser;
        use crate::graph::types::NodeType;

        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/step-runner");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("shipped step-runner graph parses");
        assert_eq!(
            graph.initial_state.get("design_decisions"),
            Some(&json!("")),
            "initial_state seeds design_decisions so the end template renders on every path"
        );
        let NodeType::Llm(handoff) = &graph.nodes["write_handoff"].node_type else {
            panic!("write_handoff is an llm node");
        };
        let schema = handoff
            .output_schema
            .as_ref()
            .expect("write_handoff has a schema");
        assert!(
            schema["required"]
                .as_array()
                .expect("write_handoff lists required keys")
                .contains(&json!("design_decisions")),
            "design_decisions is required so the model cannot drop the disclosure"
        );
        let NodeType::End(end) = &graph.nodes["end_success"].node_type else {
            panic!("end_success is an end node");
        };

        let block = "Strategy: src/pay/strategy.rs; call sites src/pay/mod.rs\n\
                     Table dispatch: src/pay/codes.rs; call site src/pay/strategy.rs";
        let mut state = StateManager::new(graph.initial_state.clone());
        state
            .state_mut()
            .set("downstream_updates".into(), json!("none"));
        apply(
            &mut state,
            &json!({
                "step_summary": "Added the payment strategies.",
                "design_decisions": block,
                "downstream_updates": "rewritten by the handoff",
            }),
            Some(schema),
            handoff.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("design_decisions"), Some(&json!(block)));
        assert_eq!(
            state.state().get("downstream_updates"),
            Some(&json!("none")),
            "write_handoff cannot rewrite a key its schema does not declare"
        );
        let rendered = state.interpolate_lenient(&end.output);
        let updates_at = rendered
            .find("Downstream plan updates:\nnone\n")
            .expect("end_success reports the downstream updates");
        let design_at = rendered
            .find(&format!("Design decisions:\n{block}"))
            .expect("end_success renders the block verbatim under its heading, continuation line included");
        assert!(
            design_at > updates_at,
            "Design decisions: follows the downstream updates, got:\n{rendered}"
        );
        assert!(
            rendered.starts_with("STEP_COMPLETE\n"),
            "the sentinel stays first: {rendered}"
        );

        // write_handoff never ran: the seeded empty block renders as the bare heading.
        let state = StateManager::new(graph.initial_state.clone());
        let rendered = state.interpolate_lenient(&end.output);
        assert!(
            rendered.ends_with("Design decisions:\n\n"),
            "an empty disclosure leaves a bare heading at the end, got:\n{rendered:?}"
        );
        assert!(
            !rendered.contains("{{"),
            "no template is left unrendered: {rendered}"
        );

        // A null block merges as null and would print as the word; the
        // check_handoff `// ""` guard is what turns it back into a bare heading.
        let mut state = StateManager::new(graph.initial_state.clone());
        apply(
            &mut state,
            &json!({"step_summary": "s", "design_decisions": null}),
            Some(schema),
            handoff.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("design_decisions"), Some(&json!(null)));
        assert!(
            state
                .interpolate_lenient(&end.output)
                .ends_with("Design decisions:\nnull\n"),
            "an unguarded null renders as the literal word"
        );
        state.state_mut().set("design_decisions".into(), json!(""));
        let rendered = state.interpolate_lenient(&end.output);
        assert!(
            rendered.ends_with("Design decisions:\n\n"),
            "after the script guard the null renders as a bare heading, got:\n{rendered:?}"
        );

        // Control: without the declaration the same output leaves the seed
        // untouched, so the schema property is what carries the disclosure.
        let mut undeclared = schema.clone();
        undeclared["properties"]
            .as_object_mut()
            .unwrap()
            .remove("design_decisions")
            .expect("the shipped schema declares design_decisions");
        let mut state = StateManager::new(graph.initial_state.clone());
        apply(
            &mut state,
            &json!({"step_summary": "s", "design_decisions": block}),
            Some(&undeclared),
            handoff.state_updates.as_ref(),
        );
        assert_eq!(state.state().get("design_decisions"), Some(&json!("")));
    }
}
