use crate::config::sanitize_display_text;
use crate::mesh::announce::is_control_or_invisible;
use crate::mesh::node::{MeshRuntime, MeshSlot};
use crate::mesh::r3::{
    AdmittedRequest, DispatchError, Handler, R3Error, Reply, RequestOptions, STATUS_PATH,
};
use crate::mesh::snapshot::{MeshSnapshot, TurnState};

use async_trait::async_trait;
use rmpv::Value;
use rns_transport::destination::DestinationDesc;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const STATUS_CARD_VERSION: u64 = 1;

/// `state.code` values. A code point is never renumbered or reused; new ones are only
/// appended, and a reader keeps a code it does not know rather than refusing the card.
pub(crate) const STATE_UNKNOWN: u8 = 0;
pub(crate) const STATE_IDLE: u8 = 1;
pub(crate) const STATE_WORKING: u8 = 2;

/// Caps on the text the card carries, in characters, applied on the serving side.
/// `DISPLAY_NAME_MAX_CHARS` is the peer-facing cap; announces enforce a separate 64-byte
/// limit (`MAX_DISPLAY_NAME_BYTES`) at startup.
pub(crate) const DISPLAY_NAME_MAX_CHARS: usize = 64;
pub(crate) const OBJECTIVE_MAX_CHARS: usize = 280;
pub(crate) const REPO_NAME_MAX_CHARS: usize = 64;
pub(crate) const BRANCH_MAX_CHARS: usize = 64;
pub(crate) const PLAN_TITLE_MAX_CHARS: usize = 120;
pub(crate) const TODO_GOAL_MAX_CHARS: usize = 280;

/// What a trusted peer learns about this session when it asks `/status`.
///
/// Wire form: a msgpack map with string keys. `v` (integer) and `served_at_secs` (integer)
/// are required, as is `state` (a map whose `code` is required and `since_secs` optional).
/// `display_name`, `objective`, `repo` (`name` required, `branch` optional), `plan`
/// (`title`), `todo` (`goal` optional, `done`, `total`) and `snapshot_age_secs` are
/// optional and left out when absent; a reader treats a missing key and nil alike.
///
/// Two rules keep a card readable across versions. Keys a reader does not know are
/// ignored, so a same-version peer may add fields without breaking older readers. State
/// code points are immutable: never renumbered, only appended, and an unknown code is kept
/// as it came rather than refused. A `v` above `STATUS_CARD_VERSION` is refused with an
/// error that names the upgrade.
///
/// The card never carries a path, the working directory, the session name, the model, the
/// role, individual todo items or brief text. `repo.name` is the last component of the
/// repository root and nothing more.
///
/// A decoded card is peer-supplied data. Its text has been through `card_text`, so it is
/// clean and within the caps, but `since_secs`, `snapshot_age_secs` and `served_at_secs` are
/// kept as sent: clock skew between peers is normal, so no value is refused as too large,
/// and a consumer turning them into ages or instants must use saturating arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusCard {
    pub display_name: Option<String>,
    pub objective: Option<String>,
    pub state: CardState,
    pub repo: Option<CardRepo>,
    pub plan: Option<CardPlan>,
    pub todo: Option<CardTodo>,
    pub snapshot_age_secs: Option<u64>,
    pub served_at_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardState {
    pub code: u8,
    pub since_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardRepo {
    pub name: String,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardPlan {
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardTodo {
    pub goal: Option<String>,
    pub done: u32,
    pub total: u32,
}

impl StatusCard {
    pub(crate) fn to_value(&self) -> Value {
        let mut entries = vec![(Value::from("v"), Value::from(STATUS_CARD_VERSION))];
        push_text(&mut entries, "display_name", self.display_name.as_deref());
        push_text(&mut entries, "objective", self.objective.as_deref());
        let mut state = vec![(Value::from("code"), Value::from(self.state.code))];
        push_u64(&mut state, "since_secs", self.state.since_secs);
        entries.push((Value::from("state"), Value::Map(state)));
        if let Some(repo) = &self.repo {
            let mut map = vec![(Value::from("name"), Value::from(repo.name.as_str()))];
            push_text(&mut map, "branch", repo.branch.as_deref());
            entries.push((Value::from("repo"), Value::Map(map)));
        }
        if let Some(plan) = &self.plan {
            let map = vec![(Value::from("title"), Value::from(plan.title.as_str()))];
            entries.push((Value::from("plan"), Value::Map(map)));
        }
        if let Some(todo) = &self.todo {
            let mut map = Vec::new();
            push_text(&mut map, "goal", todo.goal.as_deref());
            map.push((Value::from("done"), Value::from(todo.done)));
            map.push((Value::from("total"), Value::from(todo.total)));
            entries.push((Value::from("todo"), Value::Map(map)));
        }
        push_u64(&mut entries, "snapshot_age_secs", self.snapshot_age_secs);
        entries.push((
            Value::from("served_at_secs"),
            Value::from(self.served_at_secs),
        ));
        Value::Map(entries)
    }

    /// Validates the version first, so a card from a newer Coyote is named as such rather
    /// than failing on whatever the newer layout changed. Every string is sanitised and
    /// capped as if this node had served it; a required string that comes out blank is
    /// malformed.
    pub(crate) fn from_value(value: &Value) -> Result<Self, StatusError> {
        let card = Fields::of(value).ok_or_else(|| malformed("the reply is not a map"))?;
        let version = card.u64("v")?.ok_or_else(|| malformed("`v` is missing"))?;
        if version < 1 {
            return Err(malformed("`v` is 0"));
        }
        if version > STATUS_CARD_VERSION {
            return Err(StatusError::UnsupportedVersion {
                found: version,
                supported: STATUS_CARD_VERSION,
            });
        }
        let state = card
            .map("state")?
            .ok_or_else(|| malformed("`state` is missing"))?;
        let code = state
            .u64("code")?
            .and_then(|code| u8::try_from(code).ok())
            .ok_or_else(|| malformed("`state.code` is missing or not a byte"))?;
        let repo = match card.map("repo")? {
            Some(repo) => Some(CardRepo {
                name: repo
                    .text("name", REPO_NAME_MAX_CHARS)?
                    .ok_or_else(|| malformed("`repo.name` is missing or blank"))?,
                branch: repo.text("branch", BRANCH_MAX_CHARS)?,
            }),
            None => None,
        };
        let plan = match card.map("plan")? {
            Some(plan) => Some(CardPlan {
                title: plan
                    .text("title", PLAN_TITLE_MAX_CHARS)?
                    .ok_or_else(|| malformed("`plan.title` is missing or blank"))?,
            }),
            None => None,
        };
        let todo = match card.map("todo")? {
            Some(todo) => Some(CardTodo {
                goal: todo.text("goal", TODO_GOAL_MAX_CHARS)?,
                done: todo.u32("done")?,
                total: todo.u32("total")?,
            }),
            None => None,
        };
        Ok(Self {
            display_name: card.text("display_name", DISPLAY_NAME_MAX_CHARS)?,
            objective: card.text("objective", OBJECTIVE_MAX_CHARS)?,
            state: CardState {
                code,
                since_secs: state.u64("since_secs")?,
            },
            repo,
            plan,
            todo,
            snapshot_age_secs: card.u64("snapshot_age_secs")?,
            served_at_secs: card
                .u64("served_at_secs")?
                .ok_or_else(|| malformed("`served_at_secs` is missing"))?,
        })
    }
}

fn push_text(entries: &mut Vec<(Value, Value)>, key: &str, text: Option<&str>) {
    if let Some(text) = text {
        entries.push((Value::from(key), Value::from(text)));
    }
}

fn push_u64(entries: &mut Vec<(Value, Value)>, key: &str, number: Option<u64>) {
    if let Some(number) = number {
        entries.push((Value::from(key), Value::from(number)));
    }
}

fn malformed(reason: &str) -> StatusError {
    StatusError::Malformed(reason.to_string())
}

/// One msgpack map being read as a card or one of its sub-maps. A missing key and a nil
/// value both read as absent; a present value of the wrong type is malformed.
struct Fields<'a>(&'a [(Value, Value)]);

impl<'a> Fields<'a> {
    fn of(value: &'a Value) -> Option<Self> {
        value.as_map().map(Vec::as_slice).map(Self)
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0
            .iter()
            .find(|(name, _)| name.as_str() == Some(key))
            .map(|(_, value)| value)
            .filter(|value| !value.is_nil())
    }

    /// The string at `key` as `card_text` leaves it; `None` when absent or blank.
    fn text(&self, key: &str, max_chars: usize) -> Result<Option<String>, StatusError> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(|text| card_text(text, max_chars))
                .ok_or_else(|| malformed(&format!("`{key}` is not a string"))),
        }
    }

    fn u64(&self, key: &str) -> Result<Option<u64>, StatusError> {
        self.get(key)
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| malformed(&format!("`{key}` is not a non-negative integer")))
            })
            .transpose()
    }

    fn u32(&self, key: &str) -> Result<u32, StatusError> {
        self.u64(key)?
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(|| malformed(&format!("`{key}` is missing or not a 32-bit count")))
    }

    fn map(&self, key: &str) -> Result<Option<Fields<'a>>, StatusError> {
        self.get(key)
            .map(|value| {
                Fields::of(value).ok_or_else(|| malformed(&format!("`{key}` is not a map")))
            })
            .transpose()
    }
}

/// Text as the card may carry it: terminal escape sequences stripped, every other control
/// character a space, the invisible formatting characters dropped, trimmed, and cut to
/// `max_chars` characters on a character boundary with no trailing whitespace. Blank text
/// is `None`.
fn card_text(text: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = sanitize_display_text(text)
        .chars()
        .filter(|c| !is_control_or_invisible(*c))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let capped = match trimmed.char_indices().nth(max_chars) {
        Some((cut, _)) => &trimmed[..cut],
        None => trimmed,
    };
    Some(capped.trim_end().to_string())
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn count(items: usize) -> u32 {
    u32::try_from(items).unwrap_or(u32::MAX)
}

/// The card for `snapshot` as of `now`. The objective is the first of `objective_override`,
/// the snapshot's own objective and `digest_objective` that has any text once sanitised.
/// No snapshot, as before the first turn boundary, still yields a card: state unknown and
/// every optional field absent, since a trusted peer asking early is not refused.
pub(crate) fn build_card(
    snapshot: Option<&MeshSnapshot>,
    objective_override: Option<&str>,
    digest_objective: Option<&str>,
    display_name: Option<&str>,
    now: SystemTime,
) -> StatusCard {
    let objective = [
        objective_override,
        snapshot.and_then(|snapshot| snapshot.objective.as_deref()),
        digest_objective,
    ]
    .into_iter()
    .flatten()
    .find_map(|text| card_text(text, OBJECTIVE_MAX_CHARS));
    let state = match snapshot.map(|snapshot| snapshot.state) {
        None => CardState {
            code: STATE_UNKNOWN,
            since_secs: None,
        },
        Some(TurnState::Idle { since }) => CardState {
            code: STATE_IDLE,
            since_secs: Some(unix_secs(since)),
        },
        Some(TurnState::Working { since }) => CardState {
            code: STATE_WORKING,
            since_secs: Some(unix_secs(since)),
        },
    };
    let repo = snapshot
        .and_then(|snapshot| snapshot.repo.as_ref())
        .and_then(|repo| {
            let name = repo.root.file_name()?.to_string_lossy();
            Some(CardRepo {
                name: card_text(&name, REPO_NAME_MAX_CHARS)?,
                branch: repo
                    .branch
                    .as_deref()
                    .and_then(|branch| card_text(branch, BRANCH_MAX_CHARS)),
            })
        });
    let plan = snapshot
        .and_then(|snapshot| snapshot.plan.as_ref())
        .and_then(|plan| card_text(&plan.title, PLAN_TITLE_MAX_CHARS))
        .map(|title| CardPlan { title });
    let todo = snapshot.and_then(|snapshot| {
        let goal = card_text(&snapshot.todo.goal, TODO_GOAL_MAX_CHARS);
        let total = snapshot.todo.todos.len();
        if total == 0 && goal.is_none() {
            return None;
        }
        Some(CardTodo {
            goal,
            done: count(snapshot.todo.todos.iter().filter(|item| item.done).count()),
            total: count(total),
        })
    });
    StatusCard {
        display_name: display_name.and_then(|name| card_text(name, DISPLAY_NAME_MAX_CHARS)),
        objective,
        state,
        repo,
        plan,
        todo,
        snapshot_age_secs: snapshot.map(|snapshot| snapshot.age(now).as_secs()),
        served_at_secs: unix_secs(now),
    }
}

/// Where the status provider reads the session from: the published snapshot and the live
/// values overlaid on it. Everything is owned data, so serving never waits on a turn.
pub(crate) trait CardSource: Send + Sync {
    fn snapshot(&self) -> Option<Arc<MeshSnapshot>>;
    fn objective_override(&self) -> Option<Arc<String>>;
    fn display_name(&self) -> Option<String>;
}

impl CardSource for MeshSlot {
    fn snapshot(&self) -> Option<Arc<MeshSnapshot>> {
        MeshSlot::snapshot(self)
    }

    fn objective_override(&self) -> Option<Arc<String>> {
        MeshSlot::objective_override(self)
    }

    fn display_name(&self) -> Option<String> {
        self.get()
            .and_then(|runtime| runtime.display_name().map(str::to_string))
    }
}

/// Serves `/status` to whoever the dispatcher has already let through. The request body
/// is ignored: a status request carries nothing. The source is held weakly because the
/// slot owns the runtime that owns the dispatcher that owns this handler; a source that
/// is gone is served the minimal card.
pub(crate) struct StatusHandler {
    source: Weak<dyn CardSource>,
}

impl StatusHandler {
    pub(crate) fn new(source: Weak<dyn CardSource>) -> Self {
        Self { source }
    }

    fn card(&self, now: SystemTime) -> StatusCard {
        let Some(source) = self.source.upgrade() else {
            debug!(
                "Mesh /status served the minimal card: the session slot behind the provider is gone"
            );
            return build_card(None, None, None, None, now);
        };
        let snapshot = source.snapshot();
        let objective_override = source.objective_override();
        let display_name = source.display_name();
        build_card(
            snapshot.as_deref(),
            objective_override.as_deref().map(String::as_str),
            None,
            display_name.as_deref(),
            now,
        )
    }
}

#[async_trait]
impl Handler for StatusHandler {
    async fn handle(&self, _request: AdmittedRequest) -> Reply {
        Reply::Value(self.card(SystemTime::now()).to_value())
    }
}

/// Why a status request did not yield a card. Callers match on this, so it is a closed set
/// rather than `anyhow`; `?` into an `anyhow::Result` still works through `std::error::Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusError {
    /// The request itself failed: no link, no answer in time, refused, or this node is not
    /// running. A status request is answered live or not at all; nothing queues it.
    Transport(R3Error),
    /// The peer let the request through but nothing there serves `/status`.
    NotServed(DispatchError),
    /// The reply is not a card: not a map, or a required key is missing or the wrong type.
    Malformed(String),
    /// The card was written by a newer Coyote than this one reads.
    UnsupportedVersion { found: u64, supported: u64 },
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "{err}"),
            Self::NotServed(DispatchError::NoProvider { path }) => write!(
                f,
                "The mesh peer accepted the request but serves nothing at {path}; it may be running an older Coyote without a status provider"
            ),
            Self::NotServed(DispatchError::UnknownPath { path_hash }) => write!(
                f,
                "The mesh peer accepted the request but does not know the path (hash {path_hash}); it may be running a different Coyote"
            ),
            Self::Malformed(reason) => {
                write!(f, "The mesh peer's status card could not be read: {reason}")
            }
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "The mesh peer's status card is a version {found} record but this Coyote reads version {supported}. Upgrade Coyote if the peer runs a newer Coyote."
            ),
        }
    }
}

impl std::error::Error for StatusError {}

impl MeshRuntime {
    /// Asks `destination` for its status card over a live link with the default timeouts.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn request_status(
        &self,
        destination: &DestinationDesc,
    ) -> Result<StatusCard, StatusError> {
        self.request_status_with(destination, RequestOptions::default())
            .await
    }

    /// `request_status` with the caller's timeouts. The request goes to the peer directly
    /// and fails typed when it cannot be answered now; it is never held for later.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn request_status_with(
        &self,
        destination: &DestinationDesc,
        options: RequestOptions,
    ) -> Result<StatusCard, StatusError> {
        let outcome = self
            .request(destination, STATUS_PATH, Value::Nil, options)
            .await
            .map_err(StatusError::Transport)?;
        if let Some(error) = DispatchError::from_value(&outcome.value) {
            return Err(StatusError::NotServed(error));
        }
        StatusCard::from_value(&outcome.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::todo::{TodoItem, TodoList};
    use crate::mesh::snapshot::{PlanRef, RepoInfo, SessionInfo};
    use crate::mesh::test_support::{contains_bytes, rust_sources, snapshot_fixture};
    use rns_transport::resource::LINK_PACKET_MDU;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    fn encoded(card: &StatusCard) -> Vec<u8> {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &card.to_value()).unwrap();
        bytes
    }

    /// Every string the card puts on the wire.
    fn texts(card: &StatusCard) -> Vec<&str> {
        [
            card.display_name.as_deref(),
            card.objective.as_deref(),
            card.repo.as_ref().map(|repo| repo.name.as_str()),
            card.repo.as_ref().and_then(|repo| repo.branch.as_deref()),
            card.plan.as_ref().map(|plan| plan.title.as_str()),
            card.todo.as_ref().and_then(|todo| todo.goal.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// A snapshot with a distinctive literal in every field the card must not carry.
    fn adversarial_snapshot() -> MeshSnapshot {
        let mut todo = TodoList {
            goal: "goal \u{1b}[31mred\u{1b}]0;title\u{07} with\r\nbreaks".into(),
            todos: Vec::new(),
        };
        todo.todos.push(TodoItem {
            id: 1,
            desc: "ITEM-SECRET-1".into(),
            done: true,
        });
        todo.todos.push(TodoItem {
            id: 2,
            desc: "ITEM-SECRET-2".into(),
            done: false,
        });
        MeshSnapshot {
            objective: Some("x".repeat(10 * 1024)),
            state: TurnState::Working { since: now() },
            repo: Some(RepoInfo {
                root: PathBuf::from("/home/u/SECRET-DIR/proj"),
                branch: Some("\u{200B}feat/\u{202E}spoof\u{2060}".into()),
            }),
            plan: Some(PlanRef {
                path: PathBuf::from("/home/u/SECRET-DIR/proj/plans/PLAN-secret.md"),
                title: "\u{e9}".repeat(65),
            }),
            todo,
            brief: crate::mesh::snapshot::BriefState {
                mode: crate::config::mesh_config::MeshBrief::Auto,
                text: Some("BRIEF-SECRET".into()),
            },
            cwd: PathBuf::from("/home/u/SECRET-DIR/proj/sub"),
            captured_at: now() - Duration::from_secs(7),
            session: SessionInfo {
                name: Some("SESSION-SECRET".into()),
                model: "MODEL-SECRET".into(),
                role: Some("ROLE-SECRET".into()),
            },
        }
    }

    #[test]
    fn a_session_without_repo_plan_or_todo_still_yields_a_card_that_round_trips() {
        let mut snapshot = snapshot_fixture();
        snapshot.captured_at = now();
        let card = build_card(Some(&snapshot), None, None, Some("Alex"), now());
        assert_eq!(card.display_name.as_deref(), Some("Alex"));
        assert_eq!(card.objective.as_deref(), Some("ship it"));
        assert_eq!(card.state.code, STATE_IDLE);
        assert!(card.state.since_secs.is_some());
        assert_eq!(card.repo, None);
        assert_eq!(card.plan, None);
        assert_eq!(card.todo, None);
        assert_eq!(card.snapshot_age_secs, Some(0));
        assert_eq!(card.served_at_secs, unix_secs(now()));
        assert_eq!(StatusCard::from_value(&card.to_value()), Ok(card));

        let minimal = build_card(None, None, None, None, now());
        assert_eq!(
            minimal,
            StatusCard {
                display_name: None,
                objective: None,
                state: CardState {
                    code: STATE_UNKNOWN,
                    since_secs: None,
                },
                repo: None,
                plan: None,
                todo: None,
                snapshot_age_secs: None,
                served_at_secs: unix_secs(now()),
            }
        );
        assert_eq!(StatusCard::from_value(&minimal.to_value()), Ok(minimal));
    }

    #[test]
    fn a_dropped_source_is_served_the_minimal_card() {
        let source = Arc::new(FixtureSource(snapshot_fixture()));
        let handler = StatusHandler::new(Arc::downgrade(&source) as Weak<dyn CardSource>);
        assert_eq!(handler.card(now()).objective.as_deref(), Some("ship it"));

        drop(source);
        assert_eq!(
            handler.card(now()),
            build_card(None, None, None, None, now())
        );
    }

    struct FixtureSource(MeshSnapshot);

    impl CardSource for FixtureSource {
        fn snapshot(&self) -> Option<Arc<MeshSnapshot>> {
            Some(Arc::new(self.0.clone()))
        }

        fn objective_override(&self) -> Option<Arc<String>> {
            None
        }

        fn display_name(&self) -> Option<String> {
            None
        }
    }

    #[test]
    fn objective_prefers_the_override_then_the_snapshot_then_the_digest() {
        let mut snapshot = snapshot_fixture();
        snapshot.objective = Some("from the todo goal".into());
        let card = build_card(
            Some(&snapshot),
            Some("from the override"),
            Some("from the digest"),
            None,
            now(),
        );
        assert_eq!(card.objective.as_deref(), Some("from the override"));

        let card = build_card(Some(&snapshot), None, Some("from the digest"), None, now());
        assert_eq!(card.objective.as_deref(), Some("from the todo goal"));

        snapshot.objective = None;
        let card = build_card(Some(&snapshot), None, Some("from the digest"), None, now());
        assert_eq!(card.objective.as_deref(), Some("from the digest"));

        let card = build_card(Some(&snapshot), Some("  "), None, None, now());
        assert_eq!(card.objective, None);
    }

    fn with_version(card: &StatusCard, version: Value) -> Value {
        let Value::Map(entries) = card.to_value() else {
            unreachable!("a card encodes as a map");
        };
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| {
                    if key.as_str() == Some("v") {
                        (key, version.clone())
                    } else {
                        (key, value)
                    }
                })
                .collect(),
        )
    }

    #[test]
    fn version_is_one_and_newer_or_missing_versions_are_refused_by_name() {
        let card = build_card(None, None, None, None, now());
        let Value::Map(entries) = card.to_value() else {
            unreachable!("a card encodes as a map");
        };
        assert_eq!(entries[0], (Value::from("v"), Value::from(1u64)));

        let err = StatusCard::from_value(&with_version(&card, Value::from(2u64))).unwrap_err();
        assert_eq!(
            err,
            StatusError::UnsupportedVersion {
                found: 2,
                supported: 1,
            }
        );
        let text = err.to_string();
        assert!(text.contains("version 2"), "{text}");
        assert!(text.contains("Upgrade Coyote"), "{text}");

        assert!(matches!(
            StatusCard::from_value(&with_version(&card, Value::from(0u64))),
            Err(StatusError::Malformed(_))
        ));
        assert!(matches!(
            StatusCard::from_value(&with_version(&card, Value::Nil)),
            Err(StatusError::Malformed(_))
        ));
        assert!(matches!(
            StatusCard::from_value(&with_version(&card, Value::from("1"))),
            Err(StatusError::Malformed(_))
        ));
        assert!(matches!(
            StatusCard::from_value(&Value::from("not a map")),
            Err(StatusError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_keys_are_ignored_and_unknown_state_codes_are_kept() {
        let value = Value::Map(vec![
            (Value::from("v"), Value::from(1u64)),
            (
                Value::from("future"),
                Value::from("from a later same-major peer"),
            ),
            (
                Value::from("state"),
                Value::Map(vec![
                    (Value::from("code"), Value::from(9u8)),
                    (Value::from("later"), Value::Boolean(true)),
                ]),
            ),
            (Value::from("objective"), Value::Nil),
            (Value::from("served_at_secs"), Value::from(5u64)),
        ]);
        let card = StatusCard::from_value(&value).unwrap();
        assert_eq!(card.state.code, 9);
        assert_eq!(card.state.since_secs, None);
        assert_eq!(card.objective, None);
        assert_eq!(card.served_at_secs, 5);
    }

    #[test]
    fn decoding_sanitises_and_caps_peer_text_and_refuses_a_blank_required_string() {
        let objective = format!("\u{1b}[31m{}\u{202E}", "y".repeat(10 * 1024));
        let value = Value::Map(vec![
            (Value::from("v"), Value::from(1u64)),
            (Value::from("objective"), Value::from(objective)),
            (
                Value::from("state"),
                Value::Map(vec![(Value::from("code"), Value::from(STATE_IDLE))]),
            ),
            (Value::from("served_at_secs"), Value::from(5u64)),
        ]);
        let card = StatusCard::from_value(&value).unwrap();
        let objective = card.objective.as_deref().unwrap();
        assert_eq!(objective.chars().count(), OBJECTIVE_MAX_CHARS);
        assert!(objective.chars().all(|c| c == 'y'), "{objective:?}");

        let Value::Map(mut entries) = value else {
            unreachable!("the fixture is a map");
        };
        entries.push((
            Value::from("repo"),
            Value::Map(vec![(Value::from("name"), Value::from("\u{1b}[2J"))]),
        ));
        let err = StatusCard::from_value(&Value::Map(entries)).unwrap_err();
        assert_eq!(
            err,
            StatusError::Malformed("`repo.name` is missing or blank".into())
        );
    }

    #[test]
    fn card_text_is_stripped_capped_in_chars_and_never_leaks_paths_or_session_data() {
        let snapshot = adversarial_snapshot();
        let display_name = format!("Zed\u{1b}[2J Quill{}", "\u{1f600}".repeat(70));
        let card = build_card(Some(&snapshot), None, None, Some(&display_name), now());

        let objective = card.objective.as_deref().unwrap();
        assert_eq!(objective.chars().count(), OBJECTIVE_MAX_CHARS);
        assert!(objective.chars().all(|c| c == 'x'));

        let name = card.display_name.as_deref().unwrap();
        assert_eq!(name.chars().count(), DISPLAY_NAME_MAX_CHARS);
        assert!(name.starts_with("Zed Quill"), "{name:?}");
        assert!(name.ends_with('\u{1f600}'), "{name:?}");

        let repo = card.repo.as_ref().unwrap();
        assert_eq!(repo.name, "proj");
        assert_eq!(repo.branch.as_deref(), Some("feat/spoof"));

        let title = card.plan.as_ref().unwrap().title.as_str();
        assert_eq!(title, "\u{e9}".repeat(65));

        let todo = card.todo.as_ref().unwrap();
        assert_eq!(todo.goal.as_deref(), Some("goal red with  breaks"));
        assert_eq!((todo.done, todo.total), (1, 2));

        assert_eq!(card.state.code, STATE_WORKING);
        assert_eq!(card.snapshot_age_secs, Some(7));

        for text in texts(&card) {
            assert!(
                !text.chars().any(is_control_or_invisible),
                "a control or invisible character survived in {text:?}"
            );
        }
        let bytes = encoded(&card);
        for secret in [
            "SECRET-DIR",
            "/home/u",
            "PLAN-secret",
            "SESSION-SECRET",
            "MODEL-SECRET",
            "ROLE-SECRET",
            "ITEM-SECRET-1",
            "ITEM-SECRET-2",
            "BRIEF-SECRET",
            "\u{200B}",
            "\u{202E}",
            "\u{2060}",
        ] {
            assert!(!contains_bytes(&bytes, secret), "{secret:?} leaked");
        }
        assert_eq!(StatusCard::from_value(&card.to_value()), Ok(card));
    }

    #[test]
    fn card_text_caps_on_a_character_boundary() {
        let sixty_five_accents = "\u{e9}".repeat(65);
        let capped = card_text(&sixty_five_accents, 64).unwrap();
        assert_eq!(capped.chars().count(), 64);
        assert_eq!(capped.len(), 128);
        assert_eq!(capped, "\u{e9}".repeat(64));

        let emoji_at_the_cap = format!("{}\u{1f600}\u{1f600}", "a".repeat(63));
        let capped = card_text(&emoji_at_the_cap, 64).unwrap();
        assert_eq!(capped.chars().count(), 64);
        assert!(capped.ends_with('\u{1f600}'));
        assert_eq!(capped.len(), 63 + 4);

        let space_at_the_cap = format!("{} {}", "a".repeat(63), "b".repeat(5));
        let capped = card_text(&space_at_the_cap, 64).unwrap();
        assert_eq!(capped, "a".repeat(63));

        assert_eq!(card_text("  \u{1b}[31m \r\n\t ", 64), None);
        assert_eq!(card_text("\u{200B}\u{FEFF}", 64), None);
        assert_eq!(card_text("  keep  ", 64).as_deref(), Some("keep"));
    }

    #[test]
    fn repo_name_is_the_root_basename_or_nothing() {
        let mut snapshot = snapshot_fixture();
        snapshot.repo = Some(RepoInfo {
            root: PathBuf::from("/home/u/proj"),
            branch: None,
        });
        let card = build_card(Some(&snapshot), None, None, None, now());
        assert_eq!(
            card.repo,
            Some(CardRepo {
                name: "proj".into(),
                branch: None,
            })
        );

        snapshot.repo = Some(RepoInfo {
            root: PathBuf::from("/"),
            branch: Some("main".into()),
        });
        let card = build_card(Some(&snapshot), None, None, None, now());
        assert_eq!(card.repo, None);
    }

    #[test]
    fn todo_is_absent_when_empty_and_counts_done_items() {
        let mut snapshot = snapshot_fixture();
        snapshot.todo = TodoList::default();
        assert_eq!(
            build_card(Some(&snapshot), None, None, None, now()).todo,
            None
        );

        snapshot.todo.goal = "  finish  ".into();
        let card = build_card(Some(&snapshot), None, None, None, now());
        assert_eq!(
            card.todo,
            Some(CardTodo {
                goal: Some("finish".into()),
                done: 0,
                total: 0,
            })
        );
    }

    #[test]
    fn card_module_never_names_store_and_forward() {
        let needle = ["propag", "at"].concat();
        let card_rs = rust_sources()
            .into_iter()
            .find(|path| path.file_name().is_some_and(|file| file == "card.rs"))
            .expect("card.rs is among the mesh sources");
        let source = fs::read_to_string(&card_rs).unwrap();
        assert!(
            !source.contains(&needle),
            "{} must not reference {needle}",
            card_rs.display()
        );
    }

    fn maximal_card() -> StatusCard {
        StatusCard {
            display_name: Some("n".repeat(DISPLAY_NAME_MAX_CHARS)),
            objective: Some("o".repeat(OBJECTIVE_MAX_CHARS)),
            state: CardState {
                code: STATE_WORKING,
                since_secs: Some(u64::MAX),
            },
            repo: Some(CardRepo {
                name: "r".repeat(REPO_NAME_MAX_CHARS),
                branch: Some("b".repeat(BRANCH_MAX_CHARS)),
            }),
            plan: Some(CardPlan {
                title: "p".repeat(PLAN_TITLE_MAX_CHARS),
            }),
            todo: Some(CardTodo {
                goal: Some("g".repeat(TODO_GOAL_MAX_CHARS)),
                done: u32::MAX,
                total: u32::MAX,
            }),
            snapshot_age_secs: Some(u64::MAX),
            served_at_secs: u64::MAX,
        }
    }

    #[test]
    fn encoded_keys_are_only_the_documented_ones() {
        let card = maximal_card();
        let Value::Map(entries) = card.to_value() else {
            unreachable!("a card encodes as a map");
        };
        let known = [
            "v",
            "display_name",
            "objective",
            "state",
            "repo",
            "plan",
            "todo",
            "snapshot_age_secs",
            "served_at_secs",
        ];
        let keys: Vec<&str> = entries
            .iter()
            .map(|(key, _)| key.as_str().unwrap())
            .collect();
        assert_eq!(
            keys, known,
            "a maximal card carries every documented key once"
        );
        assert_eq!(StatusCard::from_value(&card.to_value()), Ok(card));
    }

    #[test]
    fn maximal_card_exceeds_the_link_mdu_and_the_minimal_card_is_small() {
        assert!(encoded(&maximal_card()).len() > LINK_PACKET_MDU);
        assert!(encoded(&build_card(None, None, None, None, now())).len() < 100);
    }

    #[test]
    fn status_error_display_teaches_the_remedy() {
        let not_served = StatusError::NotServed(DispatchError::NoProvider {
            path: STATUS_PATH.to_string(),
        })
        .to_string();
        assert!(not_served.contains("/status"), "{not_served}");
        assert!(not_served.contains("older Coyote"), "{not_served}");
        let transport = StatusError::Transport(R3Error::NotRunning).to_string();
        assert_eq!(transport, R3Error::NotRunning.to_string());
        let malformed = StatusError::Malformed("`v` is 0".into()).to_string();
        assert!(malformed.contains("status card"), "{malformed}");
    }
}
