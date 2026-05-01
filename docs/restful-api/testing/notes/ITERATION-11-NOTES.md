# Iteration 11 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/11-sub-agent-spawning.md`

## Tests created

### src/supervisor/escalation.rs (11 new tests)

| Test name | What it verifies |
|---|---|
| `queue_default_has_no_pending` | Default queue empty |
| `submit_and_has_pending` | Submit makes has_pending true |
| `submit_returns_id` | Returns the request's id |
| `take_removes_request` | Take removes and empties queue |
| `take_nonexistent_returns_none` | Missing id → None |
| `pending_summary_contains_fields` | Summary has id, agent_id, question |
| `pending_summary_includes_options_when_present` | Options included |
| `pending_summary_empty_when_no_requests` | Empty queue → empty summary |
| `reply_reaches_receiver` | oneshot channel delivers reply |
| `new_escalation_id_has_prefix` | Starts with "esc_" |
| `new_escalation_id_unique` | Two calls produce different ids |

### src/supervisor/mailbox.rs (8 new tests)

| Test name | What it verifies |
|---|---|
| `inbox_new_is_empty` | New inbox drains empty |
| `inbox_default_is_empty` | Default inbox drains empty |
| `deliver_and_drain` | Deliver + drain returns message |
| `drain_empties_inbox` | Second drain returns empty |
| `drain_orders_shutdown_before_task_before_text` | Priority ordering |
| `clone_preserves_messages` | Clone has same messages |
| `clone_is_independent` | Clone doesn't share mutations |
| `multiple_deliveries` | 5 messages all drained |

### src/supervisor/mod.rs (12 new tests)

| Test name | What it verifies |
|---|---|
| `supervisor_new_empty` | Initial state: 0 active, correct limits |
| `supervisor_register_increments_count` | Register increases active_count |
| `supervisor_register_rejects_at_capacity` | At max → error with "at capacity" |
| `supervisor_register_rejects_exceeding_depth` | Over max_depth → error |
| `supervisor_register_allows_at_max_depth` | Exactly max_depth → ok |
| `supervisor_take_removes_handle` | Take decrements count |
| `supervisor_take_nonexistent_returns_none` | Missing → None |
| `supervisor_list_agents` | Lists all registered agent ids/names |
| `supervisor_inbox_returns_handle_inbox` | Inbox accessor works |
| `supervisor_task_queue_accessible` | task_queue/task_queue_mut work |
| `agent_exit_status_equality` | Completed == Completed, != Failed |

### src/supervisor/taskqueue.rs (10 new tests, 16 total)

| Test name | What it verifies |
|---|---|
| `test_fail_sets_status` | fail() sets TaskStatus::Failed |
| `test_get_returns_none_for_missing` | get() on nonexistent → None |
| `test_dispatch_agent_stored` | dispatch_agent and prompt captured |
| `test_claim_blocked_task_fails` | Can't claim blocked task |
| `test_list_sorted_by_id` | list() returns numeric order |
| `test_default_is_empty` | TaskQueue::default() empty |
| `test_dependency_on_nonexistent_task_errors` | Bad dep → error |
| `test_complete_nonexistent_returns_empty` | Complete unknown → empty |
| `test_task_node_is_runnable` | Pending + unblocked = runnable |
| `test_task_node_not_runnable_when_blocked` | Blocked = not runnable |

### src/function/supervisor.rs (36 new handler integration tests)

| Test name | What it verifies |
|---|---|
| `handle_list_empty_supervisor` | Empty supervisor → 0 active, empty agents |
| `handle_list_with_agents` | Registered agents appear in list |
| `handle_list_no_supervisor_errors` | No supervisor → error |
| `handle_check_unknown_agent` | Check unknown → error status |
| `handle_check_pending_agent` | Check running agent → pending status |
| `handle_cancel_registered_agent` | Cancel removes and signals abort |
| `handle_cancel_unknown_agent` | Cancel unknown → error status |
| `handle_cancel_no_supervisor_errors` | No supervisor → error |
| `handle_send_message_to_registered_agent` | Message delivered to inbox |
| `handle_send_message_to_unknown_agent` | Unknown agent → error status |
| `handle_check_inbox_with_messages` | Inbox drains messages with count |
| `handle_check_inbox_no_inbox` | No inbox → count 0 |
| `handle_check_inbox_empty_inbox` | Empty inbox → count 0 |
| `handle_reply_escalation_success` | Reply delivered via oneshot |
| `handle_reply_escalation_missing_id` | Missing id → error status |
| `handle_reply_escalation_no_queue_errors` | No queue → error |
| `handle_task_create_simple` | Simple task created with id |
| `handle_task_create_with_dependencies` | Task with blocked_by |
| `handle_task_create_with_dispatch_agent` | Auto-dispatch flag set |
| `handle_task_create_agent_without_prompt_errors` | Agent without prompt → error |
| `handle_task_list_empty` | Empty queue → empty tasks array |
| `handle_task_list_with_tasks` | Tasks listed |
| `handle_task_complete_unblocks_dependents` | Complete unblocks with newly_runnable |
| `handle_task_fail_marks_failed` | Fail sets status |
| `handle_task_fail_reports_blocked_dependents` | Reports blocked deps |
| `handle_task_fail_missing_task` | Missing task → error status |
| `dispatch_unknown_action_errors` | Unknown action → error |
| `dispatch_routes_list` | agent__list → handle_list |
| `dispatch_routes_task_list` | agent__task_list → handle_task_list |
| `new_for_child_inherits_escalation_queue` | Shared Arc |
| `new_for_child_sets_depth_and_id` | Depth and self_agent_id |
| `new_for_child_has_inbox` | Shared inbox Arc |
| `new_for_child_inherits_parent_supervisor` | parent_supervisor set |
| `new_for_child_starts_with_empty_scope` | Empty functions, mcp, role, session |
| `ensure_root_escalation_queue_creates_on_first_call` | Lazy init |
| `ensure_root_escalation_queue_returns_same_on_second_call` | Same Arc |

### Infrastructure

- Added `AppState::test_default()` method for cross-module test construction
- Refactored `input.rs` and `request_context.rs` test helpers to use `test_default()`

**Total: 76 new tests (418 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **Supervisor.register enforces both capacity and depth**: These
   are the two runaway safeguards. Both tested at boundaries
   (at capacity, at max_depth, over max_depth).

2. **EscalationQueue uses oneshot channels**: The reply_tx/rx pair
   enables async blocking-wait semantics for child agents. The
   channel delivery is verified end-to-end in the test.

3. **Inbox drain ordering is a priority system**: Shutdown messages
   come first, then task completions, then text. This ensures
   lifecycle-critical messages aren't buried under chat.

4. **AgentHandle requires a tokio JoinHandle**: Creating test
   handles requires a tokio runtime. Used `rt.spawn()` with
   `mem::forget(rt)` to keep the handle alive. This is a test-only
   pattern — not ideal but necessary since JoinHandle can't be
   mocked.

5. **handle_spawn requires real agent config on disk**: This is the
   only handler that calls Agent::init. All other handlers (list,
   check, cancel, messaging, tasks, escalation) work with just a
   RequestContext + Supervisor, which we can construct in tests.

6. **Handler integration tests cover the full dispatch chain**: The
   tests call handler functions with real RequestContext instances
   containing real Supervisor/EscalationQueue/Inbox instances. This
   verifies the JSON arg parsing, supervisor interactions, and
   response formatting all at once.

7. **AppState::test_default() centralizes test construction**: Added
   a `#[cfg(test)]` constructor that avoids importing private
   modules (mcp_factory, rag_cache) from outside the config module.

## Next iteration

Plan file 12: RAG — RAG init/load/search, embeddings, document
management.
