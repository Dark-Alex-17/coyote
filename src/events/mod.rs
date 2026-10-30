use crate::config::RequestContext;
use crate::config::todo::TodoItem;
use crate::hooks::HookEvent;
use crate::prompt::PromptAction;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

/// Where an event came from: the publishing agent (None for the root) and its
/// nesting depth below the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub agent_id: Option<String>,
    pub depth: usize,
}

impl Origin {
    /// An origin for publishers that have no request context (snapshot emitters).
    pub fn detached() -> Self {
        Self {
            agent_id: None,
            depth: 0,
        }
    }
}

impl From<&RequestContext> for Origin {
    fn from(ctx: &RequestContext) -> Self {
        Self {
            agent_id: ctx.self_agent_id.clone(),
            depth: ctx.current_depth,
        }
    }
}

/// Which process stream a tool output chunk was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// What kind of engine notice a [`Event::Notice`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Checkpoint,
    AutoContinue,
    Autoname,
    Interrupted,
    Compress,
    Routing,
    GraphNode,
    ToolWarning,
}

/// Something that happened inside the engine, published on the [`EventBus`]
/// for in-process observers. Every variant owns its data so sinks can keep
/// events past the publishing call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The in-process twin of a shell hook dispatch. Published for every hook
    /// event whether or not a shell hook is configured for it. `envs` is the
    /// sanitized environment a hook would see minus the per-hook
    /// `COYOTE_HOOK_NAME`.
    Hook {
        event: HookEvent,
        envs: Vec<(String, String)>,
        payload: Option<String>,
        origin: Origin,
    },
    ToolNarration {
        call_id: String,
        line: String,
    },
    TextDelta(String),
    #[allow(dead_code)] // TASK-312
    ThinkingStarted,
    #[allow(dead_code)] // TASK-312
    ThinkingStopped,
    AssistantText {
        text: String,
        markdown: bool,
    },
    Notice {
        kind: NoticeKind,
        text: String,
    },
    ToolOutputChunk {
        call_id: String,
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    UsageUpdated {
        right_prompt: String,
    },
    ChildTranscriptFlushed {
        session_id: String,
    },
    JobCancelled {
        job_id: String,
    },
    TodoUpdated {
        items: Vec<TodoItem>,
    },
    PromptRequested {
        id: String,
        action: PromptAction,
        question: String,
        options: Vec<String>,
        default: Option<serde_json::Value>,
        tool: Option<String>,
        call_id: Option<String>,
        job_id: Option<String>,
        origin: Origin,
    },
    PromptAnswered {
        id: String,
        answer: String,
        auto: bool,
    },
    #[allow(dead_code)] // TASK-315
    QueuedInput {
        text: String,
    },
}

/// Observes events published on an [`EventBus`]. Called synchronously on the
/// publisher's thread, so implementations must return quickly and never block.
pub trait EventSink: Send + Sync {
    fn on_event(&self, event: &Event);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

/// Synchronous fan-out of [`Event`]s to subscribed sinks. Publishing never
/// holds the subscriber lock while a sink runs, so a sink may subscribe or
/// unsubscribe from inside `on_event`. A sink that panics is unsubscribed
/// so one broken observer cannot take the publisher down with it.
#[derive(Default)]
pub struct EventBus {
    sinks: RwLock<Vec<(SubscriptionId, Arc<dyn EventSink>)>>,
    next_id: AtomicU64,
}

impl EventBus {
    #[allow(dead_code)] // TASK-312
    pub fn subscribe(&self, sink: Arc<dyn EventSink>) -> SubscriptionId {
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::SeqCst));
        self.sinks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((id, sink));
        id
    }

    /// Removes the subscription; unknown ids are ignored.
    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.sinks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(existing, _)| *existing != id);
    }

    pub fn publish(&self, event: Event) {
        let sinks: Vec<(SubscriptionId, Arc<dyn EventSink>)> = self
            .sinks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for (id, sink) in sinks {
            if catch_unwind(AssertUnwindSafe(|| sink.on_event(&event))).is_err() {
                warn!("Event sink panicked; dropping it");
                self.unsubscribe(id);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn sink_count(&self) -> usize {
        self.sinks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

static BUS: LazyLock<EventBus> = LazyLock::new(EventBus::default);

pub(crate) fn bus() -> &'static EventBus {
    &BUS
}

#[cfg(test)]
pub(crate) use recording::RecordingSink;

#[cfg(test)]
mod recording {
    use super::{Event, EventSink, SubscriptionId, bus};
    use std::sync::{Arc, Mutex};

    /// Records every event published on the global bus for the sink's
    /// lifetime. The bus is process-global and its publishers are not
    /// serialized, so tests must both serialize and filter the recorded
    /// events on a fixture-unique value instead of asserting raw counts.
    pub(crate) struct RecordingSink {
        inner: Arc<Inner>,
        id: SubscriptionId,
    }

    struct Inner {
        events: Mutex<Vec<Event>>,
    }

    impl EventSink for Inner {
        fn on_event(&self, event: &Event) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event.clone());
        }
    }

    impl RecordingSink {
        /// Subscribes to the global bus; unsubscribes when dropped.
        pub(crate) fn install() -> Self {
            let inner = Arc::new(Inner {
                events: Mutex::new(Vec::new()),
            });
            let id = bus().subscribe(Arc::clone(&inner) as Arc<dyn EventSink>);
            Self { inner, id }
        }

        /// Removes and returns everything recorded so far.
        pub(crate) fn drain(&self) -> Vec<Event> {
            std::mem::take(
                &mut *self
                    .inner
                    .events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            )
        }

        /// Drains the recording and keeps only the [`Event::Hook`]s whose
        /// envs carry the `(key, value)` pair.
        pub(crate) fn drain_hooks_with_env(&self, key: &str, value: &str) -> Vec<Event> {
            let probe = (key.to_string(), value.to_string());
            self.drain()
                .into_iter()
                .filter(|event| matches!(event, Event::Hook { envs, .. } if envs.contains(&probe)))
                .collect()
        }
    }

    impl Drop for RecordingSink {
        fn drop(&mut self) {
            bus().unsubscribe(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, WorkingMode};
    use serial_test::serial;
    use std::sync::Mutex;

    struct CollectingSink {
        seen: Mutex<Vec<Event>>,
    }

    impl EventSink for CollectingSink {
        fn on_event(&self, event: &Event) {
            self.seen.lock().unwrap().push(event.clone());
        }
    }

    fn probe_event(text: &str) -> Event {
        Event::Hook {
            event: HookEvent::TurnStarted,
            envs: vec![("PROBE".to_string(), text.to_string())],
            payload: None,
            origin: Origin::detached(),
        }
    }

    #[test]
    fn publish_reaches_subscribed_sink() {
        let bus = EventBus::default();
        let sink = Arc::new(CollectingSink {
            seen: Mutex::new(Vec::new()),
        });
        bus.subscribe(Arc::clone(&sink) as Arc<dyn EventSink>);

        bus.publish(probe_event("one"));

        assert_eq!(*sink.seen.lock().unwrap(), vec![probe_event("one")]);
    }

    #[test]
    fn unsubscribed_sink_stops_receiving() {
        let bus = EventBus::default();
        let sink = Arc::new(CollectingSink {
            seen: Mutex::new(Vec::new()),
        });
        let id = bus.subscribe(Arc::clone(&sink) as Arc<dyn EventSink>);

        bus.publish(probe_event("before"));
        bus.unsubscribe(id);
        bus.publish(probe_event("after"));

        assert_eq!(*sink.seen.lock().unwrap(), vec![probe_event("before")]);
    }

    #[test]
    fn unsubscribing_unknown_id_is_a_no_op() {
        let bus = EventBus::default();
        let sink = Arc::new(CollectingSink {
            seen: Mutex::new(Vec::new()),
        });
        let id = bus.subscribe(Arc::clone(&sink) as Arc<dyn EventSink>);

        bus.unsubscribe(id);
        bus.unsubscribe(id);

        assert_eq!(bus.sink_count(), 0);
    }

    #[test]
    fn panicking_sink_is_dropped_and_other_sinks_keep_receiving() {
        struct PanickingSink;

        impl EventSink for PanickingSink {
            fn on_event(&self, _: &Event) {
                panic!("sink failure under test");
            }
        }

        let bus = EventBus::default();
        bus.subscribe(Arc::new(PanickingSink));
        let sink = Arc::new(CollectingSink {
            seen: Mutex::new(Vec::new()),
        });
        bus.subscribe(Arc::clone(&sink) as Arc<dyn EventSink>);

        bus.publish(probe_event("first"));
        assert_eq!(bus.sink_count(), 1);
        assert_eq!(*sink.seen.lock().unwrap(), vec![probe_event("first")]);

        bus.publish(probe_event("second"));
        assert_eq!(
            *sink.seen.lock().unwrap(),
            vec![probe_event("first"), probe_event("second")]
        );
    }

    #[test]
    #[serial]
    fn recording_sink_drop_leaves_global_bus_empty() {
        {
            let sink = RecordingSink::install();
            assert_eq!(bus().sink_count(), 1);
            bus().publish(probe_event("recorded"));
            let recorded: Vec<Event> = sink
                .drain()
                .into_iter()
                .filter(|event| *event == probe_event("recorded"))
                .collect();
            assert_eq!(recorded, vec![probe_event("recorded")]);
            assert!(!sink.drain().contains(&probe_event("recorded")));
        }
        assert_eq!(bus().sink_count(), 0);
    }

    #[test]
    fn detached_origin_has_no_agent_and_zero_depth() {
        assert_eq!(
            Origin::detached(),
            Origin {
                agent_id: None,
                depth: 0
            }
        );
    }

    #[test]
    fn origin_from_context_copies_agent_id_and_depth() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.self_agent_id = Some("child".into());
        ctx.current_depth = 3;

        assert_eq!(
            Origin::from(&ctx),
            Origin {
                agent_id: Some("child".to_string()),
                depth: 3
            }
        );
    }
}
