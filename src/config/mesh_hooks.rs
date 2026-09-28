//! Runs the `mesh.*` hooks. Mesh events are node-level, so only the global `hooks:` map
//! applies and no agent whitelist is consulted; that holds even for `mesh.message.sent`,
//! `mesh.message.failed` and `mesh.bulletin.sent` when an agent's `mesh__*` tool call
//! triggered them. `.set` replaces the `AppState` Arc, so the REPL re-points
//! the bridge after each command and it reads whatever `hooks:` map the current state
//! carries.

use super::AppState;
use crate::hooks::{self, HookEvent};
use crate::mesh::events::MeshHookSink;

use arc_swap::ArcSwap;
use std::sync::Arc;

pub(crate) struct MeshHookBridge {
    app: ArcSwap<AppState>,
}

impl MeshHookBridge {
    pub(crate) fn new(app: Arc<AppState>) -> Arc<Self> {
        Arc::new(Self {
            app: ArcSwap::new(app),
        })
    }

    /// Points the bridge at the app state the REPL now holds. The one in place is kept
    /// when it is the same.
    pub(crate) fn refresh(&self, app: &Arc<AppState>) {
        if !Arc::ptr_eq(&self.app.load(), app) {
            self.app.store(Arc::clone(app));
        }
    }
}

impl MeshHookSink for MeshHookBridge {
    fn fire(&self, event: HookEvent, extras: Vec<(&'static str, String)>, payload: Option<String>) {
        let app = self.app.load();
        let resolved = hooks::resolve_global_hooks(event, &app.config.hooks);
        if resolved.is_empty() {
            return;
        }
        hooks::fire_resolved(
            event,
            resolved,
            hooks::base_envs_parts(event, None, None),
            &extras,
            payload,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::hooks::{HookDef, HooksMap, test_sink};
    use crate::mesh::MeshSlot;
    use crate::mesh::test_support::snapshot_fixture;
    use crate::testing::TestConfigDirGuard;
    use serial_test::serial;

    fn hooks_map(event: &str, name: &str) -> HooksMap {
        HooksMap::from([(
            event.to_string(),
            vec![HookDef {
                name: name.to_string(),
                command: "true".to_string(),
            }],
        )])
    }

    fn app_with_hooks(hooks: HooksMap) -> Arc<AppState> {
        Arc::new(AppState {
            config: Arc::new(AppConfig {
                hooks,
                ..AppConfig::default()
            }),
            ..AppState::test_default()
        })
    }

    fn started_extras() -> Vec<(&'static str, String)> {
        vec![
            ("COYOTE_MESH_INSTANCE_ID", "inst-1".to_string()),
            ("COYOTE_MESH_INTERFACES", "lan".to_string()),
        ]
    }

    #[test]
    #[serial]
    fn a_global_hook_fires_with_node_envs_and_no_session_or_agent() {
        let guard = TestConfigDirGuard::new("mesh-hooks-global");
        let _sink = test_sink::install();
        let bridge = MeshHookBridge::new(app_with_hooks(hooks_map(
            "mesh.started",
            "t_mesh_hooks_started",
        )));

        bridge.fire(HookEvent::MeshStarted, started_extras(), None);

        let captures: Vec<_> = test_sink::drain()
            .into_iter()
            .filter(|capture| capture.hook_name == "t_mesh_hooks_started")
            .collect();
        assert_eq!(captures.len(), 1);
        let envs = &captures[0].envs;
        assert_eq!(captures[0].event, HookEvent::MeshStarted);
        assert_eq!(envs["COYOTE_EVENT"], "mesh.started");
        assert_eq!(envs["COYOTE_HOOK_NAME"], "t_mesh_hooks_started");
        assert_eq!(envs["COYOTE_MESH_INSTANCE_ID"], "inst-1");
        assert_eq!(envs["COYOTE_MESH_INTERFACES"], "lan");
        assert_eq!(envs["COYOTE_CONFIG_DIR"], guard.path.display().to_string());
        assert!(!envs.contains_key("COYOTE_AGENT_NAME"));
        assert!(!envs.contains_key("COYOTE_SESSION_ID"));
        assert!(!envs.contains_key("COYOTE_ROLE"));
        assert_eq!(captures[0].payload, None);
    }

    #[test]
    #[serial]
    fn an_event_with_no_hook_configured_fires_nothing() {
        let _guard = TestConfigDirGuard::new("mesh-hooks-none");
        let _sink = test_sink::install();
        let bridge = MeshHookBridge::new(app_with_hooks(hooks_map(
            "mesh.started",
            "t_mesh_hooks_only_started",
        )));

        bridge.fire(HookEvent::MeshStopped, started_extras(), None);

        assert!(
            test_sink::drain()
                .iter()
                .all(|capture| capture.hook_name != "t_mesh_hooks_only_started")
        );
    }

    #[test]
    #[serial]
    fn refresh_switches_to_the_new_app_states_hooks() {
        let _guard = TestConfigDirGuard::new("mesh-hooks-refresh");
        let _sink = test_sink::install();
        let first = app_with_hooks(hooks_map("mesh.started", "t_mesh_hooks_first"));
        let second = app_with_hooks(hooks_map("mesh.started", "t_mesh_hooks_second"));
        let bridge = MeshHookBridge::new(Arc::clone(&first));

        bridge.refresh(&first);
        bridge.fire(HookEvent::MeshStarted, started_extras(), None);
        bridge.refresh(&second);
        bridge.fire(HookEvent::MeshStarted, started_extras(), None);

        let names: Vec<String> = test_sink::drain()
            .into_iter()
            .map(|capture| capture.hook_name)
            .filter(|name| name.starts_with("t_mesh_hooks_"))
            .collect();
        assert_eq!(names, ["t_mesh_hooks_first", "t_mesh_hooks_second"]);
    }

    #[test]
    #[serial]
    fn a_brief_change_on_the_slot_reaches_a_global_hook_without_the_text() {
        let _guard = TestConfigDirGuard::new("mesh-hooks-brief");
        let _sink = test_sink::install();
        let bridge = MeshHookBridge::new(app_with_hooks(hooks_map(
            "mesh.brief.updated",
            "t_mesh_hooks_brief",
        )));
        let slot = MeshSlot::default();
        slot.set_hook_sink(bridge as Arc<dyn MeshHookSink>);
        slot.publish(snapshot_fixture());

        let note = "Ask before merging the release branch";
        slot.set_user_brief(Some(note.to_string()));

        let captures: Vec<_> = test_sink::drain()
            .into_iter()
            .filter(|capture| capture.hook_name == "t_mesh_hooks_brief")
            .collect();
        assert_eq!(captures.len(), 1);
        let envs = &captures[0].envs;
        assert_eq!(envs["COYOTE_EVENT"], "mesh.brief.updated");
        assert_eq!(envs["COYOTE_MESH_BRIEF_SOURCE"], "user");
        let served = slot.brief().unwrap().text.chars().count();
        assert_eq!(envs["COYOTE_MESH_BRIEF_CHARS"], served.to_string());
        assert!(
            envs.values().all(|value| !value.contains(note)),
            "{envs:#?}"
        );
        assert!(
            envs.values().all(|value| !value.contains("merging")),
            "{envs:#?}"
        );
    }
}
