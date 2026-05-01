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

**Total: 40 new tests (382 total in suite)**

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

5. **Most spawn/collect/check behaviors require integration tests**:
   The actual agent__spawn handler needs a full RequestContext with
   agent config on disk. The Supervisor struct itself is fully
   testable in isolation.

## Next iteration

Plan file 12: RAG — RAG init/load/search, embeddings, document
management.
