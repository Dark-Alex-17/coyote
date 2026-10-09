//! Conformance suite for `docs/mesh/PROTOCOL.md`, keyed by requirement id.
//!
//! The halves:
//!
//! - `vectors`: data-driven cases that feed valid, boundary and invalid inputs to this crate's
//!   own codecs (announce, propagation node announce, card, message body, knock body, canonical
//!   text) and assert the receiver action the spec mandates for each id. Rust only, every
//!   platform.
//! - `env_vectors`: the same for the R3 transport: frames, the Envelope, the dispatcher's
//!   stages, refusal and version-refusal decoding. Rust only, every platform.
//! - `link_vectors`: the ids that only show on a live link (size branches, the inbound caps,
//!   handler slots and timeouts, response correlation, version marks), run over the loopback
//!   fixtures of `r3::tests::network`. The table is declared everywhere so coverage counts it;
//!   the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `share_vectors`: the wire-path grammar and the decoders that reuse it, the share set and
//!   the grant store on a share root built per row, the `/list` and `/fetch` handlers driven
//!   in-process over such a root, the requester's page and reply readers, the versioning
//!   of the stores they read (`WirePath`, `ShareSet`, `GrantStore`, `StoreSchema`,
//!   `ListServe`, `FetchServe`, `FetchClient`). Rust only, every platform.
//! - `access_vectors`: the `/access` request and reply tables, the rate rule over a bare
//!   `MeshSlot`, the LXMF carriage and its routing, the human's decision reply and the
//!   requester's correlation of it, and the envoy's outcome wording (`AccessRequest`,
//!   `AccessReply`, `AccessLxmf`, `Decision`, `Disposition`). Rust only, every
//!   platform.
//! - `live_vectors`: the file-sharing surface as two nodes see it: the list, access, decision
//!   and fetch cycle, access admission, the requester's reading of fetch replies, symlinks
//!   under the share root and the grant a reference attachment lends, run over the loopback
//!   node pair of `r3::tests::network`. The table is declared everywhere so coverage counts
//!   it; the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `crate::repl::mesh_share_vectors`: the REPL's attachment predicate over a share root
//!   built per row (`Attachment`). Defined beside the predicate because its rows build the
//!   request context the mesh module may not name; listed here so coverage counts it. Rust
//!   only, every platform.
//! - `interop`: the protocol exercised against the pinned Python Reticulum/LXMF reference,
//!   spawned as a subprocess. Those tests are `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`;
//!   `scripts/mesh-interop/setup.sh` prepares the reference and prints the environment they need.
//! - `netns`: two of this crate's nodes in separate Linux network namespaces reaching each
//!   other through an external `rnsd` relay, the two-host topology on one machine. Also
//!   `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`; `scripts/mesh-netns/setup.sh` (root)
//!   creates the namespaces.
//!
//! Every table row names the id it exercises, so `grep MESH-ENV-030 src/mesh/conformance` lands
//! on its vectors, and the tests at the bottom of this file check the ids against the spec and
//! report coverage.
//!
//! Platform note: `interop`, `link_vectors::loopback` and `live_vectors::loopback` are
//! `#[cfg(unix)]` because the Python reference harness and the loopback fixtures they drive are
//! unix-only, not because the mesh is; `netns` is `#[cfg(target_os = "linux")]` because network
//! namespaces are a Linux kernel feature. `live_vectors::loopback` also holds the symlink rows,
//! whose fixtures create symlinks, and one `share_vectors` row branches inline on unix
//! permission bits; every vector table and `listed()` stays ungated, so coverage counts the
//! same rows on every platform. Product code under `src/mesh/` is never cfg-gated; those test
//! modules and sub-probes are the only exemption. Windows mesh behaviour is covered by the unit tests, the
//! platform-independent vectors and manual verification, not by the Python interop matrix.
//!
//! `coverage_table` renders the same data as the section 20 table of the spec, which
//! `tests::the_coverage_table_in_the_spec_is_the_generated_one` holds to it.

mod access_vectors;
mod env_vectors;
mod interop_ids;
mod link_vectors;
mod live_vectors;
mod share_vectors;
mod vectors;

#[cfg(unix)]
mod interop;
#[cfg(target_os = "linux")]
mod netns;

/// Which side of a requirement a vector probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A conforming input; the receiver accepts it and the decoded fields are checked.
    Valid,
    /// An input at a cap or an edge the spec fixes (exact length, last allowed value).
    Boundary,
    /// An input the spec names as malformed; the mandated refusal or silence is checked.
    Invalid,
}

/// One row of a module's vector table, as the coverage report sees it.
pub(crate) struct Listed {
    pub id: &'static str,
    pub kind: Kind,
    pub family: &'static str,
}

/// The tests that enforce the ids of sections 15 to 18. Those ids govern scope, structure
/// and the reference's own conduct rather than bytes a vector could feed to a codec, so the
/// section 20 table names these instead of vector families.
const ENFORCED_BY: &[(&str, &[&str])] = &[
    (
        "MESH-SEC-001",
        &[
            "empty_trust_list_admits_nobody_and_never_decodes",
            "an_identity_untrusted_before_handle_is_answered_silently",
        ],
    ),
    (
        "MESH-SEC-002",
        &[
            "encode_layout_is_magic_version_name",
            "app_data_carries_only_version_and_display_name",
        ],
    ),
    (
        "MESH-SEC-003",
        &[
            "a_claimed_instance_is_bound_to_the_proven_identity",
            "identity_is_tracked_only_after_proof_and_forgotten_on_close",
        ],
    ),
    (
        "MESH-SEC-004",
        &["receipt_fails_with_the_timeout_when_nothing_answers"],
    ),
    (
        "MESH-SEC-005",
        &[
            "an_untrusted_sender_is_discarded_and_a_trusted_one_delivered",
            "a_blocked_signer_is_discarded_and_the_stamp_line_is_logged_for_a_trusted_one",
        ],
    ),
    (
        "MESH-SEC-006",
        &["an_unknown_source_is_left_on_the_node_while_a_forgery_is_acknowledged"],
    ),
    (
        "MESH-SEC-007",
        &[
            "dedup_evicts_the_oldest_past_capacity_and_logs_it",
            "dedup_forgets_past_the_horizon_on_insert_and_on_load",
        ],
    ),
    (
        "MESH-SEC-008",
        &[
            "same_hash_is_constant_time_shaped",
            "trust_destination_refuses_a_forged_name_hash",
            "a_claimed_instance_is_bound_to_the_proven_identity",
            "production_code_never_compares_hashes_with_the_equality_operators",
        ],
    ),
    (
        "MESH-SEC-009",
        &[
            "compose_envoy_input_fences_the_peer_text_and_carries_the_data_rule",
            "check_inbox_fences_content_title_fields_text_and_data_parts_under_the_senders_label_and_leaves_files",
            "card_value_fences_each_free_text_field_under_the_peers_label_and_leaves_identifiers_bare",
            "peers_with_status_fences_a_live_cards_objective_under_the_peers_label",
            "inbox_lines_clean_a_file_parts_name_and_reference_but_show_the_staged_path_verbatim",
            "usage_probe_an_access_decision_collected_for_a_pending_request_arrives_as_fenced_json",
            "usage_probe_a_collected_data_part_arrives_fenced_with_every_string_in_it_cleaned",
        ],
    ),
    (
        "MESH-SEC-010",
        &[
            "oversized_request_resource_is_dropped_before_the_handler_runs",
            "requests_beyond_the_handler_slots_are_dropped_silently",
            "a_handler_past_its_timeout_answers_nothing_and_frees_its_slot",
            "peer_inbox_evicts_the_oldest_peer_at_capacity_and_counts_it",
            "inbound_store_survives_reopen_and_prunes_by_ttl_and_cap",
        ],
    ),
    (
        "MESH-SEC-011",
        &[
            "admit_message_refuses_the_sixty_first_in_an_hour_and_resets_after_rollover",
            "try_reserve_refuses_while_a_run_is_in_flight_and_admits_once_the_guard_drops",
            "try_reserve_refuses_past_the_token_ceiling_until_rollover",
            "cost_ceiling_is_off_at_zero_and_ignores_unpriced_debits",
            "concurrency_is_unlimited_at_zero_through_check_and_reserve",
            "messages_are_unlimited_at_zero_through_admit_message",
            "tokens_are_unlimited_at_zero_through_check_and_reserve",
        ],
    ),
    (
        "MESH-SEC-012",
        &[
            "one_identity_is_rate_limited_per_identity_and_surfaced_once",
            "the_gate_forgets_the_least_recently_seen_identity_past_its_cap",
            "the_channel_sink_never_waits_on_a_reader_and_counts_what_it_drops",
        ],
    ),
    (
        "MESH-SEC-013",
        &["costs_above_the_ceiling_are_refused_before_any_mining"],
    ),
    (
        "MESH-SEC-015",
        &[
            "an_unadmitted_peer_cannot_tell_a_served_path_from_an_unknown_one",
            "usage_probe_list_and_fetch_from_an_untrusted_peer_are_silent_like_status_whatever_the_path",
            "usage_probe_an_unreadable_granted_file_is_not_shared_byte_for_byte_and_keeps_its_use",
        ],
    ),
    (
        "MESH-SEC-016",
        &[
            "an_invalid_wire_path_is_refused_before_the_filesystem_is_touched",
            "a_symlink_that_leaves_the_root_is_not_shared",
            "a_symlink_alias_inside_the_root_cannot_reach_a_built_in_denied_file",
            "a_case_flipped_name_cannot_dodge_a_deny_under_either_fold_flag",
            "a_candidate_outside_the_canonical_root_is_never_served",
            "the_grammar_refuses_traversal_before_the_inbox_is_touched",
            "a_symlinked_directory_leading_outside_the_root_is_refused_before_any_write",
        ],
    ),
    (
        "MESH-SEC-017",
        &[
            "deny_wins_across_layers_and_an_override_lifts_only_the_builtin_deny",
            "a_workspace_override_is_inert_and_only_a_global_one_lifts_the_builtin_deny",
            "usage_probe_an_override_never_lifts_a_file_under_any_git_directory",
            "the_workspace_config_dir_is_never_served_under_allow_everything",
            "usage_probe_a_grant_on_a_built_in_denied_path_never_serves_it",
        ],
    ),
    (
        "MESH-SEC-018",
        &[
            "too_large_carries_the_local_limit_and_not_modified_carries_no_body",
            "a_file_above_the_single_segment_ceiling_is_too_large_with_that_limit",
            "a_fetch_response_at_its_bound_is_delivered_and_one_byte_over_is_dropped",
            "a_message_at_every_cap_fits_under_both_receiver_bounds_on_both_routes",
        ],
    ),
    (
        "MESH-SEC-019",
        &[
            "wrap_quotes_a_body_line_that_repeats_the_end_marker",
            "wrap_quotes_an_end_marker_hidden_behind_any_line_terminator",
            "wrap_quotes_an_end_marker_behind_leading_whitespace_or_an_invisible_character",
            "a_small_utf8_fetch_carries_its_text_fenced_under_the_peer_label",
            "usage_probe_a_fetched_file_cannot_close_the_fence_with_a_marker_hidden_behind_a_separator",
        ],
    ),
    (
        "MESH-SEC-020",
        &[
            "the_reason_is_sanitised_before_it_is_shown",
            "a_path_cannot_close_the_human_lines_frame",
            "an_access_request_never_reaches_the_envoy_sink",
        ],
    ),
    (
        "MESH-SEC-021",
        &[
            "a_one_off_grant_writes_one_use_per_path_with_the_default_ttl",
            "a_one_off_grant_whose_send_fails_leaves_no_grant_and_the_request_pending",
            "a_one_off_grant_is_consumed_by_the_fetch_and_the_second_fetch_is_not_shared",
            "expired_grants_are_swept_on_open_and_on_every_check",
        ],
    ),
    (
        "MESH-SEC-022",
        &[
            "envoy_sources_never_build_a_file_part",
            "the_envoy_never_attaches_a_part_whatever_the_outcome",
        ],
    ),
    (
        "MESH-SEC-023",
        &[
            "a_rotated_peer_is_a_stranger_to_its_old_grant",
            "identity_changed_is_never_an_allow",
            "a_new_identity_on_a_known_instance_marks_the_record_once_and_notifies_once",
            "key_change_mark_survives_restart",
            "authorize_origin_returns_the_collisions_its_verdict_was_judged_on",
            "binding_conflicts_reports_a_denied_record_and_an_all_destinations_identity",
            "note_key_change_marks_every_record_binding_conflicts_reports",
            "a_denied_record_is_marked_too",
            "a_blocked_identity_marks_nothing",
            "re_trusting_the_marked_destination_keeps_its_mark",
            "blocking_the_seen_identity_keeps_the_mark",
            "trusting_the_seen_identity_for_all_destinations_keeps_the_mark",
            "trusting_the_new_destination_clears_the_old_records_mark_and_names_it",
            "trusting_the_new_destination_clears_the_old_record_even_when_its_identity_is_trusted_for_all",
            "a_standing_identity_knocking_for_a_foreign_instance_marks_the_record_and_is_not_a_knock",
            "an_all_destinations_identity_knocking_for_a_foreign_instance_is_trusted_and_marks_the_record",
            "a_denied_knocker_over_a_foreign_instance_is_denied_and_still_marks_the_record",
            "filing_an_announce_marks_a_trusted_record_seen_under_a_new_identity",
            "an_all_destinations_identity_is_served_and_marks_with_a_warning",
            "collision_protection_refuses_an_all_destinations_identity_over_a_colliding_record",
            "an_all_destinations_identity_with_a_denied_destination_marks_with_an_error",
            "a_destination_allow_admits_in_either_collision_protection_mode",
            "a_stored_message_from_a_trusted_for_all_identity_over_a_colliding_record_is_delivered_with_a_warning",
            "collision_protection_refuses_a_stored_message_over_a_colliding_record_with_an_error",
            "usage_probe_a_strangers_stored_message_over_a_colliding_record_is_silent",
            "a_stored_access_request_from_a_trusted_for_all_identity_over_a_colliding_record_is_filed_with_a_warning",
            "collision_protection_refuses_a_stored_access_request_over_a_colliding_record_with_an_error",
            "a_strangers_stored_access_request_over_a_colliding_record_is_silent",
            "a_non_colliding_stored_access_request_writes_no_trust_file_and_tells_nobody",
            "a_trusted_for_all_identity_over_a_colliding_record_is_served_its_status_with_one_warning",
            "collision_protection_refuses_a_trusted_for_all_identity_over_a_colliding_record",
            "usage_probe_a_non_colliding_allow_writes_no_trust_file_and_says_nothing",
            "a_denied_requester_over_a_colliding_record_gets_the_collisions_with_its_verdict",
            "usage_probe_a_denied_requester_over_a_colliding_record_still_marks_it",
            "an_identity_tier_grants_rotation_is_surfaced_from_the_peer_table_but_unmarked",
            "a_presence_collision_writes_nothing_and_is_surfaced_once_per_pair",
            "two_fresh_identities_presenting_the_same_instance_earn_one_presence_line",
            "a_trusted_presenter_the_presence_rung_refuses_earns_its_own_line_after_a_strangers",
            "a_dropped_presence_line_keeps_the_refusal_and_is_offered_again",
            "a_presence_collision_between_two_all_destinations_identities_is_a_warning",
            "collision_protection_refuses_a_presence_detected_rotation_of_an_identity_trusted_for_all",
            "authorize_origin_consults_the_peer_table_only_for_an_identity_allow_under_protection",
            "a_presence_refusal_outlives_the_old_row_from_the_refusal_itself",
            "the_first_heard_holder_stays_allowed_while_the_new_keys_row_is_live",
            "a_new_key_stays_refused_after_the_holders_row_aged_out",
            "an_insider_announcing_first_heard_instance_cannot_lock_out_its_first_heard_holder",
            "the_holder_presenting_first_arms_nothing_and_the_insider_is_refused_after_it",
            "trusting_the_new_destination_clears_the_instances_presence_memo",
            "blocking_the_presenting_identity_clears_the_instances_presence_memo",
            "blocking_the_holder_clears_its_memo_too",
            "remembered_presence_pairs_and_their_index_evict_together_at_the_cap",
            "usage_probe_a_presence_refusal_holds_per_request_while_its_line_is_earned_once",
            "usage_probe_the_served_presence_line_names_the_setting_and_promises_nothing",
            "usage_probe_an_evicted_presence_memory_lifts_its_refusal_and_its_line_can_be_earned_again",
            "usage_probe_each_trusted_for_all_presenter_the_rung_refuses_is_told_once_and_later_presenters_add_nothing",
            "usage_probe_a_dropped_own_line_after_a_strangers_keeps_the_refusal_and_is_offered_again",
            "a_presence_line_with_nothing_attached_keeps_the_refusal_and_is_offered_again",
            "a_presenter_refused_by_memory_alone_still_earns_its_own_line",
            "usage_probe_a_dropped_memory_only_own_line_keeps_the_memory_and_is_offered_again",
            "usage_probe_a_surface_that_went_away_takes_no_line_and_keeps_the_refusal",
            "usage_probe_with_protection_off_a_trusted_for_all_presenter_after_a_stranger_adds_no_line",
            "usage_probe_a_trusted_for_all_knocker_over_an_instance_heard_under_another_such_identity_is_refused_under_protection",
            "usage_probe_a_knocks_verdict_and_its_owner_line_see_the_same_peer_table_instant",
            "usage_probe_a_stored_message_over_an_instance_heard_under_another_trusted_for_all_identity_is_refused_under_protection",
            "usage_probe_a_stored_access_request_over_an_instance_heard_under_another_trusted_for_all_identity_is_refused_under_protection",
            "usage_probe_an_announces_owner_line_is_judged_at_the_instant_it_is_filed",
            "an_identity_tier_rotation_heard_by_a_started_node_is_warned_about_and_writes_nothing",
            "collision_protection_refuses_a_presence_detected_rotation_at_runtime",
            "usage_probe_trusting_the_named_destination_ends_a_presence_refusal_under_protection",
            "usage_probe_a_stranger_presenting_an_instance_heard_under_an_all_destinations_identity_is_an_error_and_hears_nothing",
            "usage_probe_a_dispatched_requests_verdict_and_its_owner_line_see_the_same_peer_table_instant",
            "usage_probe_a_presence_refusal_told_through_the_dispatcher_outlives_the_holders_row_on_a_registered_path",
            "usage_probe_with_protection_off_the_dispatcher_serves_a_presence_collision_silently_and_remembers_nothing",
            "peers_labels_a_presence_refused_trusted_for_all_row_by_its_grant_under_protection",
            "usage_probe_block_and_untrust_identity_through_the_repl_are_the_remedies_of_a_remembered_presence_refusal",
            "the_collision_protection_comment_is_one_text_across_readme_template_and_example",
            "rotate_identity_is_refused_while_a_node_holds_the_identity_lock",
            "a_running_node_holds_the_identity_lock_and_stop_releases_it",
            "rotate_is_refused_while_another_process_holds_the_identity_lock",
            "peers_names_the_heard_successor_of_a_marked_record_whose_row_aged_out",
            "peers_names_the_heard_successor_of_a_denied_record_whose_row_aged_out",
            "peers_labels_a_colliding_trusted_for_all_row_by_its_grant_under_protection",
            "info_labels_a_colliding_trusted_for_all_row_by_its_grant_in_both_modes",
            "usage_probe_peers_labels_a_colliding_row_by_its_grant_in_both_modes_without_marking_or_telling",
            "usage_probe_info_labels_every_heard_row_by_its_grant_without_marking_or_telling",
            "peers_labels_a_colliding_trusted_for_all_row_by_its_grant_in_both_modes",
            "usage_probe_a_colliding_identity_the_node_refuses_to_serve_is_still_one_it_sends_to",
        ],
    ),
    (
        "MESH-SEC-024",
        &[
            "a_second_message_in_the_thread_is_driven_with_the_first_exchange",
            "a_thread_is_remembered_per_identity",
            "a_record_naming_another_identity_is_refused_and_the_thread_starts_over",
            "the_remembered_turns_are_the_fenced_peer_text_and_the_reply_alone",
            "the_owners_held_answer_is_the_turn_the_thread_remembers",
            "a_hand_off_without_a_wait_remembers_the_escalated_line",
            "a_run_time_refusal_is_remembered_as_the_refusal_the_peer_heard",
            "a_resumed_thread_debits_more_than_a_fresh_run",
            "with_the_memory_off_nothing_is_loaded_or_saved",
            "a_late_answer_joins_the_remembered_thread_and_never_starts_one",
            "untrusting_an_identity_forgets_its_remembered_threads",
            "untrust_identity_forgets_the_identitys_envoy_memory_once",
            "block_identity_forgets_the_envoy_memory_of_an_identity_never_trusted",
            "revoking_a_destination_leaves_the_envoy_memory_alone",
            "turns_over_max_turns_are_truncated_on_save",
            "bytes_over_max_bytes_are_truncated_on_save",
            "a_session_older_than_ttl_is_not_loaded_and_is_removed",
            "prune_drops_expired_sessions_enforces_the_caps_and_removes_orphan_files",
            "an_envoy_sessions_record_is_never_a_repl_session",
        ],
    ),
    ("MESH-INV-001", &["mesh_module_never_names_the_request_ctx"]),
    (
        "MESH-INV-002",
        &[
            "representation_is_a_packet_up_to_the_mdu_and_a_resource_above",
            "oversize_status_card_round_trips_as_a_resource",
        ],
    ),
    (
        "MESH-INV-003",
        &[
            "admit_message_refuses_the_sixty_first_in_an_hour_and_resets_after_rollover",
            "try_reserve_refuses_while_a_run_is_in_flight_and_admits_once_the_guard_drops",
            "try_reserve_refuses_past_the_token_ceiling_until_rollover",
        ],
    ),
    (
        "MESH-INV-004",
        &[
            "blocked_identity_is_dropped_before_decode_without_a_knock",
            "empty_trust_list_admits_nobody_and_never_decodes",
        ],
    ),
    (
        "MESH-INV-005",
        &[
            "bounds_leave_room_under_the_transport_and_response_caps",
            "garbage_bodies_are_discarded_in_bound_order_and_never_reach_the_sink",
            "a_blocked_signer_is_discarded_and_the_stamp_line_is_logged_for_a_trusted_one",
            "an_untrusted_sender_is_discarded_and_a_trusted_one_delivered",
        ],
    ),
    (
        "MESH-INV-006",
        &[
            "drain_live_notifications_passes_mesh_events_without_a_supervisor",
            "top_level_mesh_note_survives_drain_live_notifications",
        ],
    ),
    (
        "MESH-INV-007",
        &[
            "child_agents_get_a_fresh_mesh_slot_never_the_parents",
            "a_spawned_child_declares_no_mesh_tools_while_the_parent_does",
            "the_envoy_child_has_only_user_tools_and_the_read_only_trio",
        ],
    ),
    (
        "MESH-INV-008",
        &[
            "a_full_list_and_fetch_cycle_over_a_live_pair_never_calls_the_envoy",
            "a_full_access_grant_and_fetch_cycle_over_a_live_pair_never_calls_the_envoy",
            "an_access_request_never_reaches_the_envoy_sink",
            "envoy_sources_never_build_a_file_part",
        ],
    ),
    (
        "MESH-INV-009",
        &[
            "a_candidate_that_is_not_canonical_is_refused_rather_than_matched",
            "a_user_deny_on_the_resolved_file_holds_through_an_alias",
            "usage_probe_a_peer_directory_that_is_itself_a_link_outside_the_root_is_refused_before_any_write",
        ],
    ),
    (
        "MESH-LOG-001",
        &["mesh_log_lines_never_carry_peer_text_or_a_full_hash"],
    ),
    (
        "MESH-LOG-002",
        &[
            "mesh_log_lines_never_carry_peer_text_or_a_full_hash",
            "serving_path_logs_never_carry_a_full_identity_or_instance_hash",
            "an_unreachable_knock_falls_back_to_the_propagation_node",
        ],
    ),
    (
        "MESH-LOG-003",
        &["redaction_scanner_flags_each_rule_and_passes_the_permitted_forms"],
    ),
    (
        "MESH-LOG-004",
        &["redaction_scanner_flags_each_rule_and_passes_the_permitted_forms"],
    ),
    (
        "MESH-LOG-005",
        &[
            "serving_a_file_logs_a_hash_prefix_and_size_but_never_the_path",
            "mutation_logs_name_the_share_file_but_never_a_pattern_or_override_path",
            "usage_probe_every_refusal_is_logged_at_debug_with_its_rule_and_without_the_path",
            "a_served_fetch_fires_mesh_fetch_served_with_peer_size_and_hash_prefix_and_no_path",
            "access_events_carry_peer_count_and_decision_but_never_a_path",
        ],
    ),
    (
        "MESH-LEN-001",
        &[
            "oversized_request_resource_is_dropped_before_the_handler_runs",
            "oversized_response_resource_is_dropped_after_assembly",
        ],
    ),
    (
        "MESH-LEN-002",
        &["a_completed_transfer_is_accepted_one_window_later_not_at_the_deadline"],
    ),
    (
        "MESH-LEN-003",
        &[
            "from_announce_refuses_negative_costs_and_files_any_other",
            "costs_above_the_ceiling_are_refused_before_any_mining",
        ],
    ),
    (
        "MESH-LEN-004",
        &["representation_is_a_packet_up_to_the_mdu_and_a_resource_above"],
    ),
    (
        "MESH-LEN-005",
        &["usage_probe_disable_ingress_control_flips_only_ingress_control_on_every_interface"],
    ),
    (
        "MESH-LEN-006",
        &["malformed_hashes_are_refused_without_panicking"],
    ),
    (
        "MESH-LEN-007",
        &[
            "a_file_above_the_single_segment_ceiling_is_too_large_with_that_limit",
            "an_ok_reply_at_the_ceiling_fits_one_resource_segment",
        ],
    ),
];

/// The `#[test]`/`#[tokio::test]` functions that execute each vector family of `all_listed`:
/// the `run_family` callers of `vectors` and `env_vectors`, the group tests of
/// `link_vectors::loopback`, and the reference exchanges of `interop`. The section 20 family
/// table is rendered from this.
const EXECUTED_BY: &[(&str, &[&str])] = &[
    (
        "Ack",
        &["acknowledgement_vectors_are_read_only_for_their_id"],
    ),
    (
        "Access",
        &["access_admission_and_decisions_hold_over_a_live_pair"],
    ),
    (
        "AccessLxmf",
        &["stored_access_requests_are_read_and_routed_as_section_10_16_mandates"],
    ),
    (
        "AccessReply",
        &["access_replies_are_read_as_section_10_16_mandates"],
    ),
    (
        "AccessRequest",
        &["access_requests_are_answered_as_section_10_16_mandates"],
    ),
    (
        "Attachment",
        &["attachments_are_held_to_section_10_17_as_the_human_named_them"],
    ),
    (
        "Announce",
        &["announce_vectors_decode_as_section_5_1_mandates"],
    ),
    (
        "AnnounceEncode",
        &["announce_encode_vectors_refuse_what_a_sender_must_not_emit"],
    ),
    (
        "AnnouncePolicy",
        &["announce_policy_vectors_withhold_the_display_name_as_section_5_2_mandates"],
    ),
    ("Card", &["card_vectors_decode_as_section_9_mandates"]),
    (
        "CardEncode",
        &["card_encode_vectors_pin_the_emission_order"],
    ),
    (
        "Correlation",
        &["size_branches_and_correlation_hold_on_a_live_link"],
    ),
    (
        "Cycle",
        &["a_list_access_grant_and_fetch_cycle_holds_over_a_live_pair"],
    ),
    ("Custom", &["custom_vectors_hold"]),
    (
        "Decision",
        &["access_decisions_travel_as_section_10_16_mandates"],
    ),
    ("Derivation", &["derivation_vectors_reproduce_section_4"]),
    (
        "Disposition",
        &["envoy_outcomes_are_worded_as_section_10_10_mandates"],
    ),
    (
        "Dispatch",
        &["dispatch_vectors_answer_as_section_6_6_mandates"],
    ),
    (
        "DispatchErrorDecode",
        &["dispatch_error_vectors_read_as_section_6_7_mandates"],
    ),
    (
        "EnvelopeDecode",
        &["envelope_vectors_decode_as_section_6_5_mandates"],
    ),
    (
        "EnvelopeEncode",
        &["envelope_vectors_encode_in_the_key_order_of_section_6_5"],
    ),
    (
        "FetchClient",
        &["requesters_read_pages_and_replies_as_sections_10_14_and_10_15_mandate"],
    ),
    (
        "FetchServe",
        &["fetch_handlers_answer_as_section_10_15_mandates"],
    ),
    (
        "GrantStore",
        &["grant_stores_lend_spend_and_sweep_as_section_10_17_mandates"],
    ),
    (
        "HandlerSlots",
        &["the_responder_drops_what_section_6_6_says_it_drops"],
    ),
    (
        "HandlerTimeout",
        &["the_timeouts_and_the_outbound_cap_end_requests_as_specified"],
    ),
    ("HashText", &["hash_text_vectors_accept_only_32_hex_digits"]),
    (
        "Identified",
        &["size_branches_and_correlation_hold_on_a_live_link"],
    ),
    (
        "InboundCap",
        &["the_responder_drops_what_section_6_6_says_it_drops"],
    ),
    (
        "IncompatibleOutbound",
        &["version_refusals_mark_peers_and_marked_peers_are_refused_outbound"],
    ),
    (
        "Interop",
        &[
            "the_reference_announce_is_filed_and_it_derives_our_destination_from_our_announce",
            "reference_requests_hear_the_specified_replies",
            "our_requests_are_decoded_by_the_reference",
            "a_propagation_node_demanding_a_raised_stamp_cost_still_takes_our_message",
            "a_reference_message_the_envoy_could_not_run_hears_throttled_before_any_ack",
        ],
    ),
    (
        "KnockBody",
        &["knock_body_vectors_read_the_intro_as_section_8_1_mandates"],
    ),
    (
        "KnockIntro",
        &["knock_intro_vectors_clean_and_refuse_as_section_8_1_mandates"],
    ),
    (
        "LinkTimeout",
        &["the_timeouts_and_the_outbound_cap_end_requests_as_specified"],
    ),
    (
        "Lending",
        &["a_reference_attachment_lends_a_grant_only_for_a_message_the_peer_heard"],
    ),
    (
        "ListServe",
        &["list_handlers_answer_as_section_10_14_mandates"],
    ),
    (
        "LxmfKnock",
        &["lxmf_knock_vectors_decode_as_section_8_6_mandates"],
    ),
    (
        "LxmfPeer",
        &["lxmf_peer_vectors_decode_as_section_10_8_mandates"],
    ),
    (
        "MessageBody",
        &["message_body_vectors_decode_as_section_10_1_mandates"],
    ),
    (
        "MessageBodyEncode",
        &["message_body_encode_vectors_pin_the_emission_order"],
    ),
    (
        "NoKnownPath",
        &["the_timeouts_and_the_outbound_cap_end_requests_as_specified"],
    ),
    (
        "OtherKnockRefusal",
        &["the_sender_outcomes_end_as_sections_8_5_and_10_4_mandate"],
    ),
    (
        "Outbound",
        &["outbound_vectors_mint_clean_and_refuse_as_section_10_1_mandates"],
    ),
    (
        "OutboundCap",
        &["the_timeouts_and_the_outbound_cap_end_requests_as_specified"],
    ),
    (
        "PnAnnounce",
        &["propagation_node_announce_vectors_file_or_refuse_as_section_5_4_mandates"],
    ),
    (
        "RefusalCodeDecode",
        &["refusal_code_vectors_decode_as_section_6_7_mandates"],
    ),
    (
        "Registry",
        &["registry_vectors_pin_the_code_points_of_section_13"],
    ),
    (
        "RequestFrameDecode",
        &["request_frame_vectors_decode_as_section_6_1_mandates"],
    ),
    (
        "Requester",
        &["the_requester_reads_fetch_replies_as_section_10_15_mandates"],
    ),
    (
        "RequestTimeout",
        &["the_timeouts_and_the_outbound_cap_end_requests_as_specified"],
    ),
    (
        "ResponseFrameDecode",
        &["response_frame_vectors_decode_as_section_6_2_mandates"],
    ),
    ("ShareSet", &["share_sets_judge_as_section_10_17_mandates"]),
    (
        "SizeBranch",
        &["size_branches_and_correlation_hold_on_a_live_link"],
    ),
    (
        "Symlinks",
        &["symlinks_never_widen_what_is_served_or_written"],
    ),
    ("Text", &["text_vectors_clean_as_section_3_2_mandates"]),
    (
        "StoreSchema",
        &["store_schemas_refuse_and_default_as_section_14_1_mandates"],
    ),
    (
        "Trust",
        &["trust_vectors_authorize_as_the_precedence_mandates"],
    ),
    (
        "UnacknowledgedReply",
        &["the_sender_outcomes_end_as_sections_8_5_and_10_4_mandate"],
    ),
    (
        "UndecodableFrame",
        &["the_responder_drops_what_section_6_6_says_it_drops"],
    ),
    (
        "VersionMark",
        &["version_refusals_mark_peers_and_marked_peers_are_refused_outbound"],
    ),
    (
        "VersionRefusalDecode",
        &["version_refusal_vectors_hold_the_shape_of_section_7"],
    ),
    (
        "VersionRefusalEncode",
        &["version_refusal_vectors_hold_the_shape_of_section_7"],
    ),
    (
        "WirePath",
        &["wire_paths_are_held_to_the_fourteen_rules_of_section_10_13"],
    ),
    (
        "WrongLink",
        &["the_responder_drops_what_section_6_6_says_it_drops"],
    ),
];

const COVERAGE_HEADING: &str = "## 20. Conformance coverage";
const FAMILY_TABLE_HEADER: &str = "| Family | Executed by |\n|---|---|";
const COVERAGE_TABLE_HEADER: &str = "| Requirement | Vectors and tests |\n|---|---|";
const NO_VECTOR: &str = "no vector yet";
/// What section 14 puts in place of a retired requirement's body and in its index entry.
const RETIRED: &str = "[RETIRED]";

fn all_listed() -> Vec<Listed> {
    let mut listed = vectors::listed();
    listed.extend(env_vectors::listed());
    listed.extend(link_vectors::listed());
    listed.extend(share_vectors::listed());
    listed.extend(access_vectors::listed());
    listed.extend(live_vectors::listed());
    listed.extend(crate::repl::mesh_share_vectors::listed());
    listed.extend(interop_ids::listed());
    listed
}

fn enforced_by(id: &str) -> &'static [&'static str] {
    ENFORCED_BY
        .iter()
        .find(|(enforced, _)| *enforced == id)
        .map_or(&[], |(_, tests)| tests)
}

fn executed_by(family: &str) -> &'static [&'static str] {
    EXECUTED_BY
        .iter()
        .find(|(executed, _)| *executed == family)
        .map_or(&[], |(_, tests)| tests)
}

fn spec_ids() -> Vec<String> {
    crate::mesh::spec_pins::requirement_ids(crate::mesh::spec_pins::SPEC)
        .expect("the spec index parses")
}

/// The spec ids whose definition is followed by the `RETIRED` marker, `**[id]** [RETIRED]`,
/// wherever on its line the definition sits (a table row opens with `|`): they keep their
/// number and index entry and nothing covers them. A line that merely mentions the marker
/// away from the definition retires nothing.
fn retired_ids() -> std::collections::BTreeSet<String> {
    let spec = crate::mesh::spec_pins::SPEC;
    spec_ids()
        .into_iter()
        .filter(|id| {
            let opener = format!("**[{id}]** {RETIRED}");
            spec.lines().any(|line| line.contains(&opener))
        })
        .collect()
}

/// The spec ids, retired ones aside, with neither a vector in `all_listed` nor a test in
/// `ENFORCED_BY`.
fn uncovered_ids() -> std::collections::BTreeSet<String> {
    let covered: std::collections::BTreeSet<&str> =
        all_listed().iter().map(|listed| listed.id).collect();
    let retired = retired_ids();
    spec_ids()
        .into_iter()
        .filter(|id| {
            !retired.contains(id) && !covered.contains(id.as_str()) && enforced_by(id).is_empty()
        })
        .collect()
}

/// The section 20 tables: one row per vector family in name order naming the tests of
/// `EXECUTED_BY` that run it, then one row per requirement id in index order naming each
/// vector family with the kinds it feeds and the tests of `ENFORCED_BY`, or `no vector yet`,
/// or `[RETIRED]` for a retired id.
fn coverage_table() -> String {
    use std::collections::{BTreeMap, BTreeSet};

    let mut families: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for row in all_listed() {
        families
            .entry(row.id)
            .or_default()
            .insert(format!("{} ({:?})", row.family, row.kind));
        names.insert(row.family);
    }
    let mut table = FAMILY_TABLE_HEADER.to_string();
    for family in names {
        let tests: Vec<String> = executed_by(family)
            .iter()
            .map(|test| format!("`{test}`"))
            .collect();
        table.push_str(&format!("\n| {family} | {} |", tests.join(", ")));
    }
    table.push_str("\n\n");
    table.push_str(COVERAGE_TABLE_HEADER);
    let retired = retired_ids();
    for id in spec_ids() {
        let mut cells: Vec<String> = families
            .remove(id.as_str())
            .unwrap_or_default()
            .into_iter()
            .collect();
        cells.extend(enforced_by(&id).iter().map(|test| format!("`{test}`")));
        let coverage = if retired.contains(&id) {
            RETIRED.to_string()
        } else if cells.is_empty() {
            NO_VECTOR.to_string()
        } else {
            cells.join(", ")
        };
        table.push_str(&format!("\n| {id} | {coverage} |"));
    }
    table
}

#[cfg(test)]
mod tests {
    use super::{
        COVERAGE_HEADING, ENFORCED_BY, EXECUTED_BY, Kind, NO_VECTOR, RETIRED, all_listed,
        coverage_table, retired_ids, spec_ids, uncovered_ids,
    };
    use crate::mesh::r3::{
        ACCESS_PATH, FETCH_PATH, KNOCK_PATH, LIST_PATH, MESSAGE_PATH, STATUS_PATH,
    };
    use crate::mesh::spec_pins::{
        CATCH_ALL_ROW_PREFIXES, SPEC, is_catch_all_row, rust_sources, split_spans, test_functions,
    };

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    /// The ids that must have at least one vector: every id of the areas whose receiver
    /// actions are fully decidable from bytes, plus every catch-all row of the other areas.
    const REQUIRED_IDS: &[&str] = &[
        "MESH-DEST-001",
        "MESH-DEST-002",
        "MESH-DEST-003",
        "MESH-DEST-004",
        "MESH-DEST-005",
        "MESH-DEST-006",
        "MESH-DEST-007",
        "MESH-DEST-008",
        "MESH-DEST-009",
        "MESH-DEST-010",
        "MESH-ANN-001",
        "MESH-ANN-002",
        "MESH-ANN-003",
        "MESH-ANN-004",
        "MESH-ANN-005",
        "MESH-ANN-006",
        "MESH-ANN-007",
        "MESH-ANN-008",
        "MESH-ANN-009",
        "MESH-ANN-010",
        "MESH-ANN-011",
        "MESH-ANN-012",
        "MESH-ANN-013",
        "MESH-ANN-014",
        "MESH-ANN-015",
        "MESH-ANN-016",
        "MESH-ANN-017",
        "MESH-ANN-018",
        "MESH-ANN-019",
        "MESH-ANN-020",
        "MESH-ANN-021",
        "MESH-ANN-022",
        "MESH-ANN-023",
        "MESH-ANN-024",
        "MESH-ANN-025",
        "MESH-ANN-026",
        "MESH-ANN-027",
        "MESH-ANN-028",
        "MESH-ANN-029",
        "MESH-ANN-030",
        "MESH-ANN-031",
        "MESH-ANN-032",
        "MESH-ANN-033",
        "MESH-ENV-001",
        "MESH-ENV-002",
        "MESH-ENV-003",
        "MESH-ENV-004",
        "MESH-ENV-005",
        "MESH-ENV-006",
        "MESH-ENV-007",
        "MESH-ENV-008",
        "MESH-ENV-009",
        "MESH-ENV-010",
        "MESH-ENV-011",
        "MESH-ENV-012",
        "MESH-ENV-013",
        "MESH-ENV-014",
        "MESH-ENV-015",
        "MESH-ENV-016",
        "MESH-ENV-017",
        "MESH-ENV-018",
        "MESH-ENV-019",
        "MESH-ENV-020",
        "MESH-ENV-021",
        "MESH-ENV-022",
        "MESH-ENV-023",
        "MESH-ENV-024",
        "MESH-ENV-025",
        "MESH-ENV-026",
        "MESH-ENV-027",
        "MESH-ENV-028",
        "MESH-ENV-029",
        "MESH-ENV-030",
        "MESH-ENV-031",
        "MESH-ENV-032",
        "MESH-ENV-033",
        "MESH-ENV-034",
        "MESH-ENV-035",
        "MESH-ENV-036",
        "MESH-ENV-037",
        "MESH-ENV-038",
        "MESH-ENV-039",
        "MESH-ENV-040",
        "MESH-ENV-041",
        "MESH-ENV-042",
        "MESH-ENV-043",
        "MESH-ENV-044",
        "MESH-ENV-045",
        "MESH-ENV-046",
        "MESH-ENV-047",
        "MESH-ENV-048",
        "MESH-ENV-049",
        "MESH-ENV-050",
        "MESH-VER-001",
        "MESH-VER-002",
        "MESH-VER-003",
        "MESH-VER-004",
        "MESH-VER-005",
        "MESH-VER-006",
        "MESH-VER-007",
        "MESH-VER-008",
        "MESH-VER-009",
        "MESH-VER-010",
        "MESH-VER-011",
        "MESH-VER-012",
        "MESH-VER-013",
        "MESH-VER-014",
        "MESH-EXT-001",
        "MESH-EXT-002",
        "MESH-EXT-003",
        "MESH-EXT-004",
        "MESH-EXT-005",
        "MESH-EXT-006",
        "MESH-EXT-007",
        "MESH-EXT-008",
        "MESH-CODE-001",
        "MESH-CODE-002",
        "MESH-CODE-003",
        "MESH-CODE-004",
        "MESH-CODE-005",
        "MESH-KNOCK-002",
        "MESH-KNOCK-018",
        "MESH-KNOCK-022",
        "MESH-KNOCK-024",
        "MESH-STATUS-013",
        "MESH-STATUS-019",
        "MESH-STATUS-022",
        "MESH-STATUS-024",
        "MESH-STATUS-028",
        "MESH-MSG-010",
        "MESH-MSG-017",
        "MESH-MSG-023",
        "MESH-MSG-039",
        "MESH-MSG-048",
        "MESH-MSG-055",
        "MESH-PROP-009",
        "MESH-LIST-001",
        "MESH-LIST-002",
        "MESH-LIST-003",
        "MESH-LIST-004",
        "MESH-LIST-005",
        "MESH-LIST-006",
        "MESH-LIST-007",
        "MESH-LIST-008",
        "MESH-LIST-009",
        "MESH-LIST-010",
        "MESH-LIST-011",
        "MESH-LIST-012",
        "MESH-LIST-013",
        "MESH-LIST-014",
        "MESH-LIST-015",
        "MESH-LIST-016",
        "MESH-LIST-017",
        "MESH-LIST-018",
        "MESH-LIST-019",
        "MESH-LIST-020",
        "MESH-LIST-021",
        "MESH-LIST-022",
        "MESH-LIST-023",
        "MESH-LIST-024",
        "MESH-LIST-025",
        "MESH-FETCH-001",
        "MESH-FETCH-002",
        "MESH-FETCH-003",
        "MESH-FETCH-004",
        "MESH-FETCH-005",
        "MESH-FETCH-006",
        "MESH-FETCH-007",
        "MESH-FETCH-008",
        "MESH-FETCH-009",
        "MESH-FETCH-010",
        "MESH-FETCH-011",
        "MESH-FETCH-012",
        "MESH-FETCH-013",
        "MESH-FETCH-014",
        "MESH-FETCH-015",
        "MESH-FETCH-016",
        "MESH-FETCH-017",
        "MESH-FETCH-018",
        "MESH-FETCH-019",
        "MESH-FETCH-020",
        "MESH-FETCH-021",
        "MESH-FETCH-022",
        "MESH-FETCH-023",
        "MESH-FETCH-024",
        "MESH-FETCH-025",
        "MESH-FETCH-026",
        "MESH-FETCH-027",
        "MESH-FETCH-028",
        "MESH-FETCH-029",
        "MESH-FETCH-030",
        "MESH-FETCH-031",
        "MESH-FETCH-032",
        "MESH-FETCH-033",
        "MESH-FETCH-034",
        "MESH-ACCESS-001",
        "MESH-ACCESS-002",
        "MESH-ACCESS-003",
        "MESH-ACCESS-004",
        "MESH-ACCESS-005",
        "MESH-ACCESS-006",
        "MESH-ACCESS-007",
        "MESH-ACCESS-008",
        "MESH-ACCESS-009",
        "MESH-ACCESS-010",
        "MESH-ACCESS-011",
        "MESH-ACCESS-012",
        "MESH-ACCESS-013",
        "MESH-ACCESS-014",
        "MESH-ACCESS-015",
        "MESH-ACCESS-016",
        "MESH-ACCESS-017",
        "MESH-ACCESS-018",
        "MESH-ACCESS-019",
        "MESH-ACCESS-020",
        "MESH-ACCESS-021",
        "MESH-ACCESS-022",
        "MESH-ACCESS-023",
        "MESH-ACCESS-024",
        "MESH-ACCESS-025",
        "MESH-ACCESS-026",
        "MESH-ACCESS-027",
        "MESH-ACCESS-028",
        "MESH-ACCESS-029",
        "MESH-SHARE-001",
        "MESH-SHARE-002",
        "MESH-SHARE-003",
        "MESH-SHARE-004",
        "MESH-SHARE-005",
        "MESH-SHARE-006",
        "MESH-SHARE-007",
        "MESH-SHARE-008",
        "MESH-SHARE-009",
        "MESH-SHARE-010",
        "MESH-SHARE-011",
        "MESH-SHARE-012",
        "MESH-SHARE-013",
        "MESH-SHARE-014",
        "MESH-SHARE-015",
        "MESH-SHARE-016",
        "MESH-SHARE-017",
        "MESH-SHARE-018",
        "MESH-SHARE-019",
        "MESH-SHARE-020",
        "MESH-SCHEMA-001",
        "MESH-SCHEMA-002",
        "MESH-SCHEMA-003",
        "MESH-SCHEMA-004",
        "MESH-PART-008",
        "MESH-PART-011",
        "MESH-PART-014",
        "MESH-PART-026",
    ];

    fn area(id: &str) -> &str {
        id.split('-').nth(1).unwrap()
    }

    #[test]
    fn every_vector_id_is_defined_in_the_spec() {
        let spec: BTreeSet<String> = spec_ids().into_iter().collect();
        let unknown: Vec<&str> = all_listed()
            .iter()
            .map(|listed| listed.id)
            .filter(|id| !spec.contains(*id))
            .collect();
        assert_eq!(
            unknown,
            Vec::<&str>::new(),
            "vectors name ids the spec does not define"
        );
        let missing: Vec<&str> = REQUIRED_IDS
            .iter()
            .copied()
            .filter(|id| !spec.contains(*id))
            .collect();
        assert_eq!(
            missing,
            Vec::<&str>::new(),
            "REQUIRED_IDS names ids the spec does not define"
        );
    }

    #[test]
    fn coverage_report() {
        let listed = all_listed();
        let covered: BTreeSet<&str> = listed.iter().map(|listed| listed.id).collect();
        let spec = spec_ids();
        let uncovered = uncovered_ids();
        let enforced_only: Vec<&str> = spec
            .iter()
            .map(String::as_str)
            .filter(|id| !covered.contains(id) && !uncovered.contains(*id))
            .collect();

        let mut per_area: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for id in &spec {
            per_area.entry(area(id)).or_default().0 += 1;
        }
        for id in &covered {
            per_area.entry(area(id)).or_default().1 += 1;
        }
        let mut per_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut per_family: BTreeMap<&str, usize> = BTreeMap::new();
        for row in &listed {
            *per_kind.entry(format!("{:?}", row.kind)).or_default() += 1;
            *per_family.entry(row.family).or_default() += 1;
        }

        println!(
            "conformance vectors: {} rows over {} of {} spec ids",
            listed.len(),
            covered.len(),
            spec.len()
        );
        println!("by kind: {per_kind:?}");
        println!("by family: {per_family:?}");
        println!("by area (ids covered / ids in spec):");
        for (area, (total, covered)) in &per_area {
            println!("  {area}: {covered} / {total}");
        }
        println!("ids enforced by test, no vector ({}):", enforced_only.len());
        for id in &enforced_only {
            println!("  {id}");
        }
        println!("ids without a vector ({}):", uncovered.len());
        for id in &uncovered {
            println!("  {id}");
        }
        assert!(!listed.is_empty(), "no vectors are listed");
        assert!(listed.iter().any(|row| row.kind == Kind::Invalid));
    }

    #[test]
    fn every_required_id_has_a_vector() {
        let covered: BTreeSet<&str> = all_listed().iter().map(|listed| listed.id).collect();
        let missing: Vec<&&str> = REQUIRED_IDS
            .iter()
            .filter(|id| !covered.contains(**id))
            .collect();
        assert_eq!(
            missing,
            Vec::<&&str>::new(),
            "required ids without a vector"
        );
    }

    /// The attachment family is defined beside the REPL predicate it drives; this runs it
    /// with the rest of the conformance slice so `cargo test mesh::conformance` covers every
    /// listed family.
    #[test]
    #[serial_test::serial]
    fn the_attachment_family_runs_with_the_conformance_slice() {
        crate::repl::mesh_share_vectors::run_all();
    }

    // The minimum-coverage rule reads: "every id in areas DEST, ANN, ENV, VER, EXT, CODE,
    // LIST, FETCH, ACCESS, SHARE, SCHEMA and every catch-all/`any other value` row in
    // KNOCK/STATUS/MSG/PROP/PART/DISP". These derive that set from `docs/mesh/PROTOCOL.md`
    // itself so the hand-maintained `REQUIRED_IDS` above cannot silently drift from it, in
    // either direction.

    /// Areas whose every id must have a vector.
    const FULLY_COVERED_AREAS: &[&str] = &[
        "DEST", "ANN", "ENV", "VER", "EXT", "CODE", "LIST", "FETCH", "ACCESS", "SHARE", "SCHEMA",
    ];
    /// Areas where only the catch-all table rows must have a vector. The spec writes the
    /// disposition's `any other value` rule in the action column of the `disposition` key
    /// row rather than as a positional catch-all row, so `DISP` contributes no id today and
    /// is listed for the day a disposition table gains one; MESH-DISP-002 has rows regardless.
    const CATCH_ALL_AREAS: &[&str] = &["KNOCK", "STATUS", "MSG", "PROP", "PART", "DISP"];

    /// The `**[MESH-AREA-NNN]**` ids on one spec line, in order.
    fn ids_on(line: &str) -> Vec<&str> {
        line.match_indices("**[MESH-")
            .filter_map(|(start, _)| {
                let from = start + "**[".len();
                let end = line[from..].find("]**")?;
                Some(&line[from..from + end])
            })
            .collect()
    }

    fn required_ids_from_the_spec() -> BTreeSet<String> {
        spec_ids()
            .into_iter()
            .filter(|id| FULLY_COVERED_AREAS.contains(&area(id)))
            .chain(catch_all_row_ids())
            .collect()
    }

    /// The ids a catch-all row cites from a catch-all area: the second source of
    /// `REQUIRED_IDS`, next to the fully covered areas.
    fn catch_all_row_ids() -> BTreeSet<String> {
        table_rows()
            .filter(|line| is_catch_all_row(line))
            .flat_map(ids_on)
            .filter(|id| CATCH_ALL_AREAS.contains(&area(id)))
            .map(str::to_string)
            .collect()
    }

    fn table_rows() -> impl Iterator<Item = &'static str> {
        SPEC.lines()
            .filter(|line| line.trim_start().starts_with('|'))
    }

    /// `is_catch_all_row` is case-sensitive on purpose (spec_pins asserts an uppercase
    /// catch-all fails its table check), so a row written `| Any other ... |` would drop
    /// out of the required set without a sound. This counts the rows by a case-folded
    /// first cell and holds the predicate to the same number.
    #[test]
    fn catch_all_rows_are_lowercase_so_the_shared_predicate_sees_every_one() {
        let folded = table_rows()
            .filter(|row| {
                row.trim()
                    .trim_start_matches('|')
                    .split('|')
                    .next()
                    .map(|cell| cell.trim().to_ascii_lowercase())
                    .is_some_and(|cell| {
                        CATCH_ALL_ROW_PREFIXES
                            .iter()
                            .any(|prefix| cell.starts_with(prefix))
                    })
            })
            .count();
        let accepted = table_rows().filter(|row| is_catch_all_row(row)).count();
        assert!(folded > 0, "no catch-all rows in the spec");
        assert_eq!(
            folded, accepted,
            "a catch-all row is not lowercase and the shared predicate skips it"
        );
    }

    #[test]
    fn the_spec_derived_required_set_is_the_shape_the_ruling_describes() {
        let derived = required_ids_from_the_spec();
        let fully_covered = spec_ids()
            .into_iter()
            .filter(|id| FULLY_COVERED_AREAS.contains(&area(id)))
            .count();
        assert!(
            derived.len() > fully_covered,
            "no catch-all rows were found: {derived:?}"
        );
        assert!(
            derived.contains("MESH-MSG-010"),
            "the `any other key` body row"
        );
        assert!(
            derived.contains("MESH-PROP-009"),
            "the `any other element` PN row"
        );
        assert!(
            derived.contains("MESH-KNOCK-018"),
            "the `any other refusal code` outcome row, in a table without the positional header"
        );
        assert!(
            derived.contains("MESH-MSG-023"),
            "the `any other reply value` outcome row, in a table without the positional header"
        );
        assert!(derived.iter().all(|id| {
            FULLY_COVERED_AREAS.contains(&area(id)) || CATCH_ALL_AREAS.contains(&area(id))
        }));
    }

    #[test]
    fn no_retired_id_lies_in_a_fully_covered_area() {
        let cited_by_a_catch_all_row = catch_all_row_ids();
        let retired: Vec<String> = retired_ids()
            .into_iter()
            .filter(|id| {
                FULLY_COVERED_AREAS.contains(&area(id)) || cited_by_a_catch_all_row.contains(id)
            })
            .collect();
        assert_eq!(
            retired,
            Vec::<String>::new(),
            "every id in a fully covered area or cited by a catch-all row needs a vector (the minimum-coverage rule behind REQUIRED_IDS); retire by rewording in place rather than marking the id retired"
        );
    }

    #[test]
    fn required_ids_lists_every_id_the_ruling_requires() {
        let derived = required_ids_from_the_spec();
        let listed: BTreeSet<&str> = REQUIRED_IDS.iter().copied().collect();
        let omitted: Vec<&str> = derived
            .iter()
            .map(String::as_str)
            .filter(|id| !listed.contains(id))
            .collect();
        assert_eq!(
            omitted,
            Vec::<&str>::new(),
            "catch-all / fully-covered-area ids the ruling requires but REQUIRED_IDS omits"
        );
        let surplus: Vec<&str> = listed
            .iter()
            .copied()
            .filter(|id| !derived.contains(*id))
            .collect();
        assert_eq!(
            surplus,
            Vec::<&str>::new(),
            "REQUIRED_IDS names ids the ruling does not require; the areas or the catch-all rows changed"
        );
    }

    #[test]
    fn every_id_the_ruling_requires_has_a_vector() {
        let covered: BTreeSet<&str> = all_listed().iter().map(|listed| listed.id).collect();
        let missing: Vec<String> = required_ids_from_the_spec()
            .into_iter()
            .filter(|id| !covered.contains(id.as_str()))
            .collect();
        assert_eq!(
            missing,
            Vec::<String>::new(),
            "ids the minimum-coverage ruling requires that have no vector"
        );
    }

    /// The `| id | coverage |` rows of the spec's section 20, in document order.
    fn spec_coverage_rows() -> Vec<(String, String)> {
        SPEC.lines()
            .skip_while(|line| *line != COVERAGE_HEADING)
            .skip(1)
            .take_while(|line| !line.starts_with("## "))
            .filter(|line| line.starts_with("| MESH-"))
            .map(|line| {
                let mut cells = line.trim_matches('|').split(" | ").map(str::trim);
                (
                    cells.next().unwrap().to_string(),
                    cells.next().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn the_coverage_table_in_the_spec_is_the_generated_one() {
        assert!(
            SPEC.contains(COVERAGE_HEADING),
            "the coverage heading moved; update COVERAGE_HEADING to the section's new number"
        );
        let in_spec: Vec<&str> = SPEC
            .lines()
            .skip_while(|line| *line != COVERAGE_HEADING)
            .skip(1)
            .take_while(|line| !line.starts_with("## "))
            .filter(|line| line.starts_with('|'))
            .collect();
        let generated = coverage_table();
        let generated_rows: Vec<&str> = generated
            .lines()
            .filter(|line| line.starts_with('|'))
            .collect();
        assert_eq!(
            in_spec, generated_rows,
            "section 20 of docs/mesh/PROTOCOL.md is stale; replace its tables with:\n{generated}"
        );
    }

    #[test]
    fn retired_ids_are_exactly_the_ones_section_14_has_retired() {
        let retired: Vec<String> = retired_ids().into_iter().collect();
        assert_eq!(retired, vec!["MESH-SEC-014".to_string()]);

        let index = SPEC
            .split_once("\n## 21. Requirements index\n")
            .expect("the spec has a requirements index")
            .1;
        let marked_in_index: BTreeSet<String> = index
            .lines()
            .filter(|line| line.contains(RETIRED))
            .map(|line| {
                line.strip_prefix("- [")
                    .and_then(|rest| rest.split_once("]("))
                    .map(|(id, _)| id.to_string())
                    .unwrap_or_else(|| panic!("index line marked retired is not an entry: {line}"))
            })
            .collect();
        assert_eq!(
            marked_in_index,
            retired_ids(),
            "the index marks [RETIRED] exactly the ids whose definition is retired"
        );
    }

    /// A retired id has no vector, is not required to have one and is enforced by no named
    /// test: `coverage_table` reads it as retired before it looks for any of those.
    #[test]
    fn a_retired_id_has_no_vector_and_is_neither_required_nor_enforced() {
        let retired = retired_ids();
        let listed: BTreeSet<&str> = all_listed()
            .iter()
            .map(|listed| listed.id)
            .filter(|id| retired.contains(*id))
            .collect();
        assert_eq!(listed, BTreeSet::new(), "a vector names a retired id");
        let required: Vec<&str> = REQUIRED_IDS
            .iter()
            .copied()
            .filter(|id| retired.contains(*id))
            .collect();
        assert_eq!(
            required,
            Vec::<&str>::new(),
            "REQUIRED_IDS names a retired id"
        );
        let enforced: Vec<&str> = ENFORCED_BY
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| retired.contains(*id))
            .collect();
        assert_eq!(
            enforced,
            Vec::<&str>::new(),
            "ENFORCED_BY names a retired id"
        );
    }

    #[test]
    fn enforced_by_covers_exactly_the_ids_that_have_no_vector_by_design() {
        let retired = retired_ids();
        let by_design: BTreeSet<String> = spec_ids()
            .into_iter()
            .filter(|id| ["SEC", "INV", "LOG", "LEN"].contains(&area(id)) && !retired.contains(id))
            .collect();
        let keys: Vec<String> = ENFORCED_BY.iter().map(|(id, _)| id.to_string()).collect();
        let unique: BTreeSet<String> = keys.iter().cloned().collect();
        assert_eq!(
            keys.len(),
            unique.len(),
            "an id is listed twice in ENFORCED_BY"
        );
        assert_eq!(
            unique, by_design,
            "ENFORCED_BY and the SEC/INV/LOG/LEN ids of the spec differ"
        );
        assert!(
            all_listed().iter().all(|row| !by_design.contains(row.id)),
            "a SEC/INV/LOG/LEN id has a vector; the section 20 intro no longer holds"
        );
        for (id, tests) in ENFORCED_BY {
            assert!(!tests.is_empty(), "{id} names no test");
        }
    }

    #[test]
    fn enforced_by_names_tests_that_exist() {
        let sources = rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap();
        let missing: Vec<String> = ENFORCED_BY
            .iter()
            .flat_map(|(id, tests)| tests.iter().map(move |test| (id, test)))
            .filter(|(_, test)| !sources.contains(&format!("fn {test}(")))
            .map(|(id, test)| format!("{id}: {test}"))
            .collect();
        assert_eq!(
            missing,
            Vec::<String>::new(),
            "ENFORCED_BY names functions that do not exist under src/"
        );
    }

    /// For each id of `enforced`, the test functions (those of `tests`) its defining line of
    /// `spec` cites in code spans, against the names `enforced` lists for it.
    fn citation_mismatches(
        spec: &str,
        tests: &BTreeSet<String>,
        enforced: &[(&str, &[&str])],
    ) -> Vec<String> {
        enforced
            .iter()
            .filter_map(|(id, names)| {
                let opener = format!("**[{id}]**");
                let Some(line) = spec.lines().find(|line| line.contains(&opener)) else {
                    return Some(format!("{id}: not defined in the spec"));
                };
                let cited: BTreeSet<&str> = split_spans(line)
                    .into_iter()
                    .map(|(_, span)| span)
                    .filter(|span| tests.contains(*span))
                    .collect();
                let listed: BTreeSet<&str> = names.iter().copied().collect();
                (cited != listed).then(|| {
                    format!("{id}: ENFORCED_BY lists {listed:?}, the spec cites {cited:?}")
                })
            })
            .collect()
    }

    #[test]
    fn citation_mismatch_checker_reports_extra_missing_and_undefined() {
        let spec = "\
**[MESH-SEC-001]** A MUST (`alpha_test_one`, `helper_fn_x`, src/a.rs).
**[MESH-SEC-002]** B MUST (`beta_test_two`).
";
        let tests: BTreeSet<String> = ["alpha_test_one", "beta_test_two", "gamma_test_three"]
            .map(String::from)
            .into();
        assert_eq!(
            citation_mismatches(
                spec,
                &tests,
                &[
                    ("MESH-SEC-001", &["alpha_test_one"][..]),
                    ("MESH-SEC-002", &["beta_test_two"][..]),
                ]
            ),
            Vec::<String>::new()
        );
        let mismatched = citation_mismatches(
            spec,
            &tests,
            &[
                ("MESH-SEC-001", &["alpha_test_one", "gamma_test_three"][..]),
                ("MESH-SEC-002", &["alpha_test_one"][..]),
                ("MESH-SEC-003", &["beta_test_two"][..]),
            ],
        );
        assert_eq!(mismatched.len(), 3, "{mismatched:?}");
        assert!(
            mismatched[0].contains("gamma_test_three"),
            "{}",
            mismatched[0]
        );
        assert!(
            mismatched[1].contains("the spec cites {\"beta_test_two\"}"),
            "{}",
            mismatched[1]
        );
        assert_eq!(mismatched[2], "MESH-SEC-003: not defined in the spec");
    }

    #[test]
    fn enforced_by_matches_the_inline_citations() {
        let sources = rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap();
        let tests = test_functions(&sources);
        assert!(!tests.is_empty(), "no test functions under src/");
        assert_eq!(
            citation_mismatches(SPEC, &tests, ENFORCED_BY),
            Vec::<String>::new()
        );
    }

    #[test]
    fn executed_by_covers_exactly_the_families_that_are_listed() {
        let listed: BTreeSet<&str> = all_listed().iter().map(|row| row.family).collect();
        let keys: Vec<&str> = EXECUTED_BY.iter().map(|(family, _)| *family).collect();
        let unique: BTreeSet<&str> = keys.iter().copied().collect();
        assert_eq!(
            keys.len(),
            unique.len(),
            "a family is listed twice in EXECUTED_BY"
        );
        assert_eq!(
            unique, listed,
            "EXECUTED_BY and the families of all_listed differ"
        );
        for (family, tests) in EXECUTED_BY {
            assert!(!tests.is_empty(), "{family} names no test");
        }
    }

    /// Each test of `EXECUTED_BY` is defined in a conformance module that names the family it
    /// is claimed to run, as the `"Family"` literal of a vector table or the `` `Family` `` of
    /// a doc comment, so a test cannot be credited with a family from another module. This
    /// file names every family in `EXECUTED_BY` itself, so it is left out of the match. The
    /// one family that drives the REPL's attachment predicate is defined beside that
    /// predicate, so its module is read alongside the ones in this directory.
    #[test]
    fn executed_by_names_tests_in_the_module_of_their_family() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(src.join("mesh/conformance"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .filter(|path| path.file_name().is_some_and(|name| name != "mod.rs"))
            .collect();
        paths.push(src.join("repl/mesh_share_vectors.rs"));
        let sources: Vec<(String, String)> = paths
            .into_iter()
            .map(|path| {
                let source = std::fs::read_to_string(&path).unwrap();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    source,
                )
            })
            .collect();
        let names_family = |source: &str, family: &str| {
            source.contains(&format!("\"{family}\"")) || source.contains(&format!("`{family}`"))
        };
        let missing: Vec<String> = EXECUTED_BY
            .iter()
            .flat_map(|(family, tests)| tests.iter().map(move |test| (*family, *test)))
            .filter(|(family, test)| {
                !sources.iter().any(|(_, source)| {
                    source.contains(&format!("fn {test}(")) && names_family(source, family)
                })
            })
            .map(|(family, test)| {
                let defined_in: Vec<&str> = sources
                    .iter()
                    .filter(|(_, source)| source.contains(&format!("fn {test}(")))
                    .map(|(file, _)| file.as_str())
                    .collect();
                format!("{family}: {test} (defined in {defined_in:?})")
            })
            .collect();
        assert_eq!(
            missing,
            Vec::<String>::new(),
            "EXECUTED_BY names tests that are not defined in a conformance module naming their family"
        );
    }

    #[test]
    fn ids_marked_no_vector_yet_are_exactly_the_uncovered_ids() {
        let rows = spec_coverage_rows();
        let spec = spec_ids();
        assert_eq!(
            rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            spec.iter().map(String::as_str).collect::<Vec<_>>(),
            "section 20 lists a different set of ids than the index"
        );
        let marked: BTreeSet<String> = rows
            .iter()
            .filter(|(_, coverage)| coverage == NO_VECTOR)
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(marked, uncovered_ids());
        assert!(!marked.is_empty(), "every id has a vector; drop the marker");
    }

    // The CI job, pinned on the workflow file rather than on a run of it: a Linux-only
    // `mesh-interop` job prepares the reference with `setup.sh`, then runs the conformance
    // suite with the interop tests un-ignored and switched on, under a `timeout-minutes`,
    // records its wall-clock in the step summary, and is kept out of every job's `needs`
    // (the `All` gate included) with a comment saying so. Being outside `needs` is the whole
    // of "informational": the job must NOT also be `continue-on-error`, or a red suite would
    // leave the run and the README badge green. The same job creates the network namespaces
    // for the `netns` suite before the tests and removes them after, whatever the outcome.
    const CI_YAML: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yaml"
    ));

    #[test]
    fn the_ci_job_runs_the_interop_suite_the_way_the_ruling_says() {
        use serde_yaml::Value;

        let workflow: Value = serde_yaml::from_str(CI_YAML).expect("ci.yaml parses");
        let jobs = workflow["jobs"].as_mapping().expect("jobs is a map");
        let job = &workflow["jobs"]["mesh-interop"];
        assert!(!job.is_null(), "no `mesh-interop` job in ci.yaml");

        assert_eq!(job["runs-on"].as_str(), Some("ubuntu-latest"), "Linux-only");
        assert!(
            job["timeout-minutes"].as_u64().is_some_and(|m| m > 0),
            "timeout-minutes is set: {:?}",
            job["timeout-minutes"]
        );
        assert!(
            job["continue-on-error"].is_null(),
            "the job must not be continue-on-error; a red interop suite has to be a red job: {:?}",
            job["continue-on-error"]
        );

        // Not a prerequisite of anything, `all` included.
        assert!(!workflow["jobs"]["all"].is_null(), "the `all` gate exists");
        for (name, other) in jobs {
            let needs: Vec<&str> = match &other["needs"] {
                Value::String(one) => vec![one.as_str()],
                Value::Sequence(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            assert!(
                !needs.contains(&"mesh-interop"),
                "{} lists mesh-interop in its needs",
                name.as_str().unwrap_or("?")
            );
        }
        // ... and the comment above the job says so.
        let lines: Vec<&str> = CI_YAML.lines().collect();
        let at = lines
            .iter()
            .position(|line| line.trim_end() == "  mesh-interop:")
            .expect("the job key is at the two-space indent");
        let comment: String = lines[..at]
            .iter()
            .rev()
            .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            .filter(|line| line.trim_start().starts_with('#'))
            .map(|line| line.trim_start().trim_start_matches('#').trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            comment.contains("`All`")
                && (comment.contains("not part") || comment.contains("needs")),
            "the comment above the job must say it is not in the `All` gate: {comment:?}"
        );
        assert!(
            comment.contains("CAP_NET_ADMIN") || comment.contains("sudo"),
            "the comment above the job must state the privilege the namespaces need: {comment:?}"
        );

        // The interop setup.sh, then the namespaces, then the suite with the ignored tests
        // un-ignored and switched on, then the namespaces removed whatever the suite did.
        let steps = job["steps"].as_sequence().expect("steps");
        let runs: Vec<(usize, &str)> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, step)| step["run"].as_str().map(|run| (i, run)))
            .collect();
        let setup = runs
            .iter()
            .find(|(_, run)| run.contains("scripts/mesh-interop/setup.sh"))
            .map(|(i, _)| *i)
            .expect("a step runs scripts/mesh-interop/setup.sh");
        let (test, command) = runs
            .iter()
            .find(|(_, run)| run.contains("cargo test"))
            .map(|(i, run)| (*i, *run))
            .expect("a step runs cargo test");
        assert!(setup < test, "setup.sh runs before the suite");
        let (namespaces, namespaces_run) = runs
            .iter()
            .find(|(_, run)| run.contains("scripts/mesh-netns/setup.sh"))
            .map(|(i, run)| (*i, *run))
            .expect("a step runs scripts/mesh-netns/setup.sh");
        assert!(
            namespaces_run.trim_start().starts_with("sudo "),
            "creating the namespaces needs root: {namespaces_run:?}"
        );
        assert!(
            setup < namespaces && namespaces < test,
            "the namespaces are created after the reference and before the suite"
        );
        let (teardown, teardown_step) = steps
            .iter()
            .enumerate()
            .find(|(_, step)| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| run.contains("scripts/mesh-netns/teardown.sh"))
            })
            .expect("a step runs scripts/mesh-netns/teardown.sh");
        assert!(
            teardown_step["run"]
                .as_str()
                .is_some_and(|run| run.trim_start().starts_with("sudo ")),
            "removing the namespaces needs root: {:?}",
            teardown_step["run"]
        );
        assert!(
            teardown > test,
            "the namespaces are removed after the suite"
        );
        assert_eq!(
            teardown_step["if"].as_str().map(str::trim),
            Some("always()"),
            "the namespaces are removed even when the suite fails"
        );
        for (i, step) in steps.iter().enumerate() {
            assert!(
                step["continue-on-error"].is_null(),
                "step {i} ({:?}) must not be continue-on-error either: {:?}",
                step["name"].as_str().unwrap_or("?"),
                step["continue-on-error"]
            );
        }
        let words: Vec<&str> = command.split_whitespace().collect();
        assert!(words.contains(&"COYOTE_MESH_INTEROP=1"), "{command}");
        assert!(words.contains(&"mesh::conformance"), "{command}");
        assert!(words.contains(&"--include-ignored"), "{command}");
        assert!(
            !words.contains(&"--ignored"),
            "libtest rejects --ignored together with --include-ignored: {command}"
        );
        let dashes = words
            .iter()
            .position(|w| *w == "--")
            .expect("libtest args follow --");
        assert!(
            words[dashes..].contains(&"--include-ignored"),
            "--include-ignored is a libtest flag and must follow `--`: {command}"
        );

        // Wall-clock recorded in the step summary, whatever the suite did.
        let summary = steps
            .iter()
            .enumerate()
            .find(|(_, step)| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| run.contains("GITHUB_STEP_SUMMARY"))
            })
            .expect("a step writes the step summary");
        assert!(
            summary.0 > test,
            "the wall-clock is recorded after the suite"
        );
        assert_eq!(
            summary.1["if"].as_str().map(str::trim),
            Some("always()"),
            "the wall-clock step runs even when the suite fails"
        );
        let started_before_setup = steps[..setup].iter().any(|step| {
            step["run"]
                .as_str()
                .is_some_and(|run| run.contains("date +%s") && run.contains("GITHUB_ENV"))
        });
        assert!(
            started_before_setup,
            "the clock starts before setup.sh so the recorded wall-clock covers the reference too"
        );
        assert!(
            summary.1["run"].as_str().unwrap().contains("date +%s"),
            "the summary step reads the clock"
        );
    }

    // The reference peer's served paths and the harness README's account of them, pinned
    // on the files alone so the check runs on every platform, not only where the Python
    // reference can be spawned.
    const HARNESS_README: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-interop/README.md"
    ));
    const REFERENCE_PEER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-interop/reference_peer.py"
    ));

    #[test]
    fn the_reference_peer_serves_exactly_the_paths_the_readme_says_it_does() {
        let gap = format!(
            "The reference peer does not implement `{KNOCK_PATH}`, `{LIST_PATH}`, `{FETCH_PATH}` or `{ACCESS_PATH}`;"
        );
        let unwrapped = HARNESS_README
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            unwrapped.contains(&gap),
            "scripts/mesh-interop/README.md no longer says {gap:?}"
        );
        // The README must also say what does cover those paths: the Rust-only vector
        // modules, each of which must exist and list rows.
        let covered_by = "the file-sharing paths are covered by Rust-only in-process conformance vectors in `src/mesh/conformance/share_vectors.rs`, `access_vectors.rs` and `live_vectors.rs`";
        assert!(
            unwrapped.contains(covered_by),
            "scripts/mesh-interop/README.md no longer says {covered_by:?}"
        );
        for (module, listed) in [
            ("share_vectors.rs", super::share_vectors::listed().len()),
            ("access_vectors.rs", super::access_vectors::listed().len()),
            ("live_vectors.rs", super::live_vectors::listed().len()),
        ] {
            assert!(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/mesh/conformance")
                    .join(module)
                    .is_file()
                    && listed > 0,
                "the README names {module} as a vector module, which must exist and list rows"
            );
        }

        let mut lines = REFERENCE_PEER.lines();
        let mut registered = Vec::new();
        while let Some(line) = lines.next() {
            if !line.contains(".register_request_handler(") {
                continue;
            }
            let args = lines
                .next()
                .unwrap_or_else(|| panic!("{line:?} has no argument line"));
            let (_, after_quote) = args
                .split_once('"')
                .unwrap_or_else(|| panic!("{args:?} does not open a path literal"));
            let (path, _) = after_quote
                .split_once('"')
                .unwrap_or_else(|| panic!("{args:?} does not close its path literal"));
            registered.push(path);
        }
        assert_eq!(
            registered,
            [STATUS_PATH, MESSAGE_PATH],
            "scripts/mesh-interop/reference_peer.py registers a different set of request paths"
        );
        for path in registered {
            assert!(
                unwrapped.contains(&format!("`{path}`")),
                "scripts/mesh-interop/README.md does not name the served path `{path}`"
            );
        }
    }
}
