use crate::mesh::brief::{Digest, digest_objective_for};
use crate::mesh::display_text;
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The card's own schema version, evolving under the one mesh protocol version the
/// envelope carries.
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
pub(crate) const ABOUT_MAX_CHARS: usize = 200;
pub(crate) const CAPS_MAX_ENTRIES: usize = 16;
pub(crate) const CAP_MAX_CHARS: usize = 32;

/// What a trusted peer learns about this session when it asks `/status`.
///
/// Wire form: a msgpack map with string keys. `v` (integer) and `served_at_secs` (integer)
/// are required, as is `state` (a map whose `code` is required and `since_secs` optional).
/// `display_name`, `objective`, `repo` (`name` required, `branch` optional), `plan`
/// (`title`), `todo` (`goal` optional, `done`, `total`), `about`, `caps` and
/// `snapshot_age_secs` are optional and left out when absent; a reader treats a missing
/// key and nil alike, and an empty `caps` list is never emitted.
///
/// Two rules keep a card readable across versions. Keys a reader does not know are
/// ignored, so a same-version peer may add fields without breaking older readers. State
/// code points are immutable: never renumbered, only appended, and an unknown code is kept
/// as it came rather than refused. `caps` entries this Coyote does not define are kept as
/// they came for the same reason. A `v` above `STATUS_CARD_VERSION` is refused with an
/// error that names the upgrade.
///
/// The card never carries a path, the working directory, the session name, the model, the
/// role, individual todo items or brief text. `repo.name` is the last component of the
/// repository root and nothing more.
///
/// A decoded card is peer-supplied data. Its text has been through `display_text`, so it is
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
    pub about: Option<String>,
    pub caps: Vec<String>,
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
        push_text(&mut entries, "about", self.about.as_deref());
        if !self.caps.is_empty() {
            let caps = self.caps.iter().map(|cap| Value::from(cap.as_str()));
            entries.push((Value::from("caps"), Value::Array(caps.collect())));
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
            about: card.text_or_absent("about", ABOUT_MAX_CHARS),
            caps: card.text_list_or_empty("caps", CAPS_MAX_ENTRIES, CAP_MAX_CHARS),
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
/// value both read as absent; a present value of the wrong type is malformed, except
/// under the `_or_absent` and `_or_empty` readers, which keep the rest of the card.
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

    /// The string at `key` as `display_text` leaves it; `None` when absent or blank.
    fn text(&self, key: &str, max_chars: usize) -> Result<Option<String>, StatusError> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(|text| display_text(text, max_chars))
                .ok_or_else(|| malformed(&format!("`{key}` is not a string"))),
        }
    }

    /// `text` for a key added after `v: 1` shipped: a value that is not a string reads as
    /// absent, since a reader from before the key would have accepted the card.
    fn text_or_absent(&self, key: &str, max_chars: usize) -> Option<String> {
        let value = self.get(key)?;
        match value.as_str() {
            Some(text) => display_text(text, max_chars),
            None => {
                debug!("Mesh status card `{key}` is not a string; read as absent");
                None
            }
        }
    }

    /// The strings in the list at `key`, each as `display_text` leaves it, for a key added
    /// after `v: 1` shipped. A value that is not a list reads as no entries, an entry that
    /// is not a string or comes out blank is skipped, and entries past `max_entries` are
    /// dropped; nothing here refuses the card.
    fn text_list_or_empty(&self, key: &str, max_entries: usize, max_chars: usize) -> Vec<String> {
        let Some(value) = self.get(key) else {
            return Vec::new();
        };
        let Some(entries) = value.as_array() else {
            debug!("Mesh status card `{key}` is not a list; read as empty");
            return Vec::new();
        };
        entries
            .iter()
            .take(max_entries)
            .filter_map(|entry| match entry.as_str() {
                Some(text) => display_text(text, max_chars),
                None => {
                    debug!("Mesh status card `{key}` entry is not a string; skipped");
                    None
                }
            })
            .collect()
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

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn count(items: usize) -> u32 {
    u32::try_from(items).unwrap_or(u32::MAX)
}

/// The card as `.mesh status` shows it, ages in the `age_text` form the rest of `.mesh`
/// uses. Peer-supplied instants may be anything, clock skew included, so every age
/// saturates and an unknown state code is shown as such.
pub(crate) fn render_for_human(card: &StatusCard, now: SystemTime) -> String {
    let age = |then: u64| {
        let then = UNIX_EPOCH
            .checked_add(Duration::from_secs(then))
            .unwrap_or(now);
        crate::mesh::age_text(now, then)
    };
    let mut lines = vec![
        format!("name: {}", card.display_name.as_deref().unwrap_or("(none)")),
        format!(
            "objective: {}",
            card.objective.as_deref().unwrap_or("(none)")
        ),
    ];
    if let Some(about) = &card.about {
        lines.push(format!("about: {about}"));
    }
    if !card.caps.is_empty() {
        lines.push(format!("caps: {}", card.caps.join(", ")));
    }
    let state = match card.state.code {
        STATE_IDLE => "idle".to_string(),
        STATE_WORKING => "working".to_string(),
        STATE_UNKNOWN => "unknown".to_string(),
        code => format!("unknown ({code})"),
    };
    lines.push(format!("state: {state}"));
    if let Some(since) = card.state.since_secs {
        lines.push(format!("since: {}", age(since)));
    }
    if let Some(repo) = &card.repo {
        lines.push(match &repo.branch {
            Some(branch) => format!("repo: {} ({branch})", repo.name),
            None => format!("repo: {}", repo.name),
        });
    }
    if let Some(plan) = &card.plan {
        lines.push(format!("plan: {}", plan.title));
    }
    if let Some(todo) = &card.todo {
        lines.push(match &todo.goal {
            Some(goal) => format!("todo: {}/{} ({goal})", todo.done, todo.total),
            None => format!("todo: {}/{}", todo.done, todo.total),
        });
    }
    if let Some(snapshot_age) = card.snapshot_age_secs {
        lines.push(format!("snapshot: {snapshot_age}s old when served"));
    }
    lines.push(format!("served: {}", age(card.served_at_secs)));
    lines.join("\n")
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
    about: Option<&str>,
    caps: &[String],
    now: SystemTime,
) -> StatusCard {
    let objective = [
        objective_override,
        snapshot.and_then(|snapshot| snapshot.objective.as_deref()),
        digest_objective,
    ]
    .into_iter()
    .flatten()
    .find_map(|text| display_text(text, OBJECTIVE_MAX_CHARS));
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
                name: display_text(&name, REPO_NAME_MAX_CHARS)?,
                branch: repo
                    .branch
                    .as_deref()
                    .and_then(|branch| display_text(branch, BRANCH_MAX_CHARS)),
            })
        });
    let plan = snapshot
        .and_then(|snapshot| snapshot.plan.as_ref())
        .and_then(|plan| display_text(&plan.title, PLAN_TITLE_MAX_CHARS))
        .map(|title| CardPlan { title });
    let todo = snapshot.and_then(|snapshot| {
        let goal = display_text(&snapshot.todo.goal, TODO_GOAL_MAX_CHARS);
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
        display_name: display_name.and_then(|name| display_text(name, DISPLAY_NAME_MAX_CHARS)),
        objective,
        state,
        repo,
        plan,
        todo,
        about: about.and_then(|about| display_text(about, ABOUT_MAX_CHARS)),
        caps: caps.to_vec(),
        snapshot_age_secs: snapshot.map(|snapshot| snapshot.age(now).as_secs()),
        served_at_secs: unix_secs(now),
    }
}

/// Where the status provider reads the session from: the published snapshot and the live
/// values overlaid on it. Everything is owned data, so serving never waits on a turn.
pub(crate) trait CardSource: Send + Sync {
    fn snapshot(&self) -> Option<Arc<MeshSnapshot>>;
    fn objective_override(&self) -> Option<Arc<String>>;
    fn digest(&self) -> Option<Arc<Digest>>;
    fn display_name(&self) -> Option<String>;
    fn about(&self) -> Option<String>;
    fn caps(&self) -> Vec<String>;
}

impl CardSource for MeshSlot {
    fn snapshot(&self) -> Option<Arc<MeshSnapshot>> {
        MeshSlot::snapshot(self)
    }

    fn objective_override(&self) -> Option<Arc<String>> {
        MeshSlot::objective_override(self)
    }

    fn digest(&self) -> Option<Arc<Digest>> {
        MeshSlot::digest(self)
    }

    fn display_name(&self) -> Option<String> {
        self.get()
            .and_then(|runtime| runtime.display_name().map(str::to_string))
    }

    fn about(&self) -> Option<String> {
        self.get()
            .and_then(|runtime| runtime.about().map(str::to_string))
    }

    // TASK-111 advertises "fetch" once the /fetch handler exists.
    fn caps(&self) -> Vec<String> {
        Vec::new()
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

    /// The digest's objective is judged against the one snapshot this card is built from,
    /// so a mode change between two reads of the slot cannot mix them.
    pub(crate) fn card(&self, now: SystemTime) -> StatusCard {
        let Some(source) = self.source.upgrade() else {
            debug!(
                "Mesh /status served the minimal card: the session slot behind the provider is gone"
            );
            return build_card(None, None, None, None, None, &[], now);
        };
        let snapshot = source.snapshot();
        let objective_override = source.objective_override();
        let digest = source.digest();
        let digest_objective = snapshot
            .as_deref()
            .and_then(|snapshot| digest_objective_for(snapshot, digest.as_deref()));
        let display_name = source.display_name();
        let about = source.about();
        build_card(
            snapshot.as_deref(),
            objective_override.as_deref().map(String::as_str),
            digest_objective.as_deref(),
            display_name.as_deref(),
            about.as_deref(),
            &source.caps(),
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
    /// The card was written by a newer Coyote than this one reads. This is the card's
    /// schema version; a mesh protocol mismatch fails as `Transport(UnsupportedVersion)`.
    UnsupportedVersion { found: u64, supported: u64 },
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(
                f,
                "{err}. A status request is answered live or not at all; nothing queues it for later."
            ),
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
    pub(crate) async fn request_status(
        &self,
        destination: &DestinationDesc,
    ) -> Result<StatusCard, StatusError> {
        self.request_status_with(destination, RequestOptions::default())
            .await
    }

    /// `request_status` with the caller's timeouts. The request goes to the peer directly
    /// and fails typed when it cannot be answered now; it is never held for later.
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
    use crate::config::mesh_config::MeshBrief;
    use crate::config::todo::{TodoItem, TodoList};
    use crate::mesh::announce::is_control_or_invisible;
    use crate::mesh::snapshot::{PlanRef, RepoInfo, SessionInfo};
    use crate::mesh::test_support::{contains_bytes, rust_sources, snapshot_fixture};
    use rns_transport::resource::LINK_PACKET_MDU;
    use std::fs;
    use std::path::PathBuf;

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
            card.about.as_deref(),
        ]
        .into_iter()
        .flatten()
        .chain(card.caps.iter().map(String::as_str))
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
                digest_generated_at: Some(now() - Duration::from_secs(9)),
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
        let card = build_card(Some(&snapshot), None, None, Some("Alex"), None, &[], now());
        assert_eq!(card.display_name.as_deref(), Some("Alex"));
        assert_eq!(card.objective.as_deref(), Some("ship it"));
        assert_eq!(card.state.code, STATE_IDLE);
        assert!(card.state.since_secs.is_some());
        assert_eq!(card.repo, None);
        assert_eq!(card.plan, None);
        assert_eq!(card.todo, None);
        assert_eq!(card.about, None);
        assert!(card.caps.is_empty());
        assert_eq!(card.snapshot_age_secs, Some(0));
        assert_eq!(card.served_at_secs, unix_secs(now()));
        assert_eq!(StatusCard::from_value(&card.to_value()), Ok(card));

        let minimal = build_card(None, None, None, None, None, &[], now());
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
                about: None,
                caps: Vec::new(),
                snapshot_age_secs: None,
                served_at_secs: unix_secs(now()),
            }
        );
        assert_eq!(StatusCard::from_value(&minimal.to_value()), Ok(minimal));
    }

    #[test]
    fn a_dropped_source_is_served_the_minimal_card() {
        let source = Arc::new(FixtureSource {
            snapshot: snapshot_fixture(),
            digest: None,
        });
        let handler = StatusHandler::new(Arc::downgrade(&source) as Weak<dyn CardSource>);
        assert_eq!(handler.card(now()).objective.as_deref(), Some("ship it"));

        drop(source);
        assert_eq!(
            handler.card(now()),
            build_card(None, None, None, None, None, &[], now())
        );
    }

    struct FixtureSource {
        snapshot: MeshSnapshot,
        digest: Option<Digest>,
    }

    impl CardSource for FixtureSource {
        fn snapshot(&self) -> Option<Arc<MeshSnapshot>> {
            Some(Arc::new(self.snapshot.clone()))
        }

        fn objective_override(&self) -> Option<Arc<String>> {
            None
        }

        fn digest(&self) -> Option<Arc<Digest>> {
            self.digest.clone().map(Arc::new)
        }

        fn display_name(&self) -> Option<String> {
            None
        }

        fn about(&self) -> Option<String> {
            None
        }

        fn caps(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn a_slot_without_a_runtime_has_no_about_and_advertises_no_caps() {
        let slot = Arc::new(MeshSlot::default());
        slot.publish(snapshot_fixture());
        assert_eq!(CardSource::about(slot.as_ref()), None);
        assert!(CardSource::caps(slot.as_ref()).is_empty());
        let handler = StatusHandler::new(Arc::downgrade(&slot) as Weak<dyn CardSource>);
        let card = handler.card(now());
        assert_eq!(card.objective.as_deref(), Some("ship it"));
        assert_eq!(card.about, None);
        assert!(card.caps.is_empty());
    }

    #[test]
    fn status_handler_serves_the_digest_objective_when_nothing_else_names_one() {
        let mut snapshot = snapshot_fixture();
        snapshot.objective = None;
        snapshot.todo = TodoList::default();
        snapshot.brief.mode = MeshBrief::Auto;
        let source = Arc::new(FixtureSource {
            snapshot,
            digest: Some(Digest {
                text: "- from the digest".into(),
                generated_at: now(),
                covered_messages: 4,
            }),
        });
        let handler = StatusHandler::new(Arc::downgrade(&source) as Weak<dyn CardSource>);
        assert_eq!(
            handler.card(now()).objective.as_deref(),
            Some("from the digest")
        );
    }

    #[test]
    fn the_slot_serves_a_digest_objective_under_auto_only() {
        for (mode, expected) in [
            (MeshBrief::Manual, None),
            (MeshBrief::Off, None),
            (MeshBrief::Auto, Some("from the digest")),
        ] {
            let slot = Arc::new(MeshSlot::default());
            let mut snapshot = snapshot_fixture();
            snapshot.objective = None;
            snapshot.todo = TodoList::default();
            snapshot.brief.mode = mode;
            slot.publish(snapshot);
            slot.publish_digest(Some(Digest {
                text: "- from the digest".into(),
                generated_at: now(),
                covered_messages: 4,
            }));
            let handler = StatusHandler::new(Arc::downgrade(&slot) as Weak<dyn CardSource>);
            assert_eq!(
                handler.card(now()).objective.as_deref(),
                expected,
                "{mode:?}"
            );
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
            None,
            &[],
            now(),
        );
        assert_eq!(card.objective.as_deref(), Some("from the override"));

        let card = build_card(
            Some(&snapshot),
            None,
            Some("from the digest"),
            None,
            None,
            &[],
            now(),
        );
        assert_eq!(card.objective.as_deref(), Some("from the todo goal"));

        snapshot.objective = None;
        let card = build_card(
            Some(&snapshot),
            None,
            Some("from the digest"),
            None,
            None,
            &[],
            now(),
        );
        assert_eq!(card.objective.as_deref(), Some("from the digest"));

        let card = build_card(Some(&snapshot), Some("  "), None, None, None, &[], now());
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
        let card = build_card(None, None, None, None, None, &[], now());
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
        assert_eq!(card.about, None, "a card from before `about` reads as none");
        assert!(
            card.caps.is_empty(),
            "a card from before `caps` reads as none"
        );
    }

    fn card_with(key: &str, value: Value) -> Value {
        Value::Map(vec![
            (Value::from("v"), Value::from(1u64)),
            (
                Value::from("state"),
                Value::Map(vec![(Value::from("code"), Value::from(STATE_IDLE))]),
            ),
            (Value::from(key), value),
            (Value::from("served_at_secs"), Value::from(5u64)),
        ])
    }

    #[test]
    fn about_is_sanitised_and_cut_on_a_character_boundary() {
        let about = format!("\u{1b}[31m{}\u{202E}", "\u{e9}".repeat(ABOUT_MAX_CHARS + 5));
        let card = StatusCard::from_value(&card_with("about", Value::from(about))).unwrap();
        let about = card.about.as_deref().unwrap();
        assert_eq!(about.chars().count(), ABOUT_MAX_CHARS);
        assert_eq!(about.len(), ABOUT_MAX_CHARS * 2);
        assert!(about.chars().all(|c| c == '\u{e9}'), "{about:?}");

        let numeric = StatusCard::from_value(&card_with("about", Value::from(7u64))).unwrap();
        assert_eq!(
            numeric.about, None,
            "an `about` that is not a string reads as absent, not as a malformed card"
        );
        let blank = StatusCard::from_value(&card_with("about", Value::from(" \u{200B} "))).unwrap();
        assert_eq!(blank.about, None);
    }

    #[test]
    fn caps_skips_entries_that_are_not_text_and_drops_those_past_the_cap() {
        let mut entries: Vec<Value> = (0..20).map(|i| Value::from(format!("cap-{i}"))).collect();
        entries[3] = Value::from(3u64);
        entries[5] = Value::from("\u{1b}[2J");
        entries[7] = Value::from(format!("fetch{}", "x".repeat(CAP_MAX_CHARS)));
        let card = StatusCard::from_value(&card_with("caps", Value::Array(entries))).unwrap();
        assert_eq!(card.caps.len(), CAPS_MAX_ENTRIES - 2);
        assert!(
            card.caps
                .iter()
                .all(|cap| cap.chars().count() <= CAP_MAX_CHARS)
        );
        assert_eq!(card.caps[0], "cap-0");
        assert_eq!(card.caps[3], "cap-4");
        assert_eq!(card.caps[4], "cap-6");
        assert_eq!(
            card.caps[5],
            format!("fetch{}", "x".repeat(CAP_MAX_CHARS - 5)),
            "an over-long entry is cut, not dropped"
        );
        assert_eq!(card.caps[6], "cap-8");
        assert_eq!(card.caps.last().map(String::as_str), Some("cap-15"));
        assert!(!card.caps.iter().any(|cap| cap.contains("cap-16")));

        let empty = StatusCard::from_value(&card_with("caps", Value::Array(vec![]))).unwrap();
        assert!(empty.caps.is_empty());
        let nil = StatusCard::from_value(&card_with("caps", Value::Nil)).unwrap();
        assert!(nil.caps.is_empty());
    }

    #[test]
    fn caps_that_are_not_a_list_read_as_no_caps() {
        for wrong in [
            Value::Map(vec![(Value::from("fetch"), Value::Boolean(true))]),
            Value::from("fetch"),
            Value::from(1u64),
        ] {
            let card = StatusCard::from_value(&card_with("caps", wrong)).unwrap();
            assert!(card.caps.is_empty(), "{:?}", card.caps);
        }
    }

    /// The keys a `v: 1` reader from before `about` and `caps` would have ignored may
    /// not refuse the card now; the rest of the card reads as if they were absent.
    #[test]
    fn a_card_with_a_malformed_about_and_caps_still_reads_the_rest_intact() {
        let value = Value::Map(vec![
            (Value::from("v"), Value::from(1u64)),
            (Value::from("display_name"), Value::from("Alex")),
            (
                Value::from("state"),
                Value::Map(vec![(Value::from("code"), Value::from(STATE_IDLE))]),
            ),
            (Value::from("about"), Value::Array(vec![Value::from("x")])),
            (Value::from("caps"), Value::from("fetch")),
            (Value::from("served_at_secs"), Value::from(5u64)),
        ]);

        let card = StatusCard::from_value(&value).unwrap();

        assert_eq!(card.display_name.as_deref(), Some("Alex"));
        assert_eq!(card.state.code, STATE_IDLE);
        assert_eq!(card.served_at_secs, 5);
        assert_eq!(card.about, None);
        assert!(card.caps.is_empty());
        assert_eq!(StatusCard::from_value(&card.to_value()), Ok(card));
    }

    /// Usage probe, amendment "ignore-on-receipt (card)": the lenient readers must hold
    /// for the shapes a non-Rust peer can put on the wire that are not `rmpv` strings —
    /// msgpack `bin` and a `str` holding bytes that are not UTF-8 — and must hold after a
    /// real encode→decode, not only on a hand-built `Value`. A wrong-typed `about` is
    /// absent, every non-text `caps` entry is skipped, and the card's required keys read
    /// as served.
    #[test]
    fn usage_probe_binary_and_invalid_utf8_about_and_caps_read_as_absent_through_msgpack_bytes() {
        // `rmpv` only builds a `Utf8String` from valid text, so the invalid `str` is made
        // the way a peer would make it: a 3-byte fixstr whose bytes are not UTF-8,
        // patched over a 3-byte marker after encoding.
        const MARKER: &str = "QQQ";
        fn patch_marker(bytes: &mut [u8]) {
            let needle = [0xa3, b'Q', b'Q', b'Q'];
            let at = bytes.windows(4).position(|w| w == needle).unwrap();
            bytes[at + 1..at + 4].copy_from_slice(&[0xff, 0xfe, 0x41]);
            assert!(!bytes.windows(4).any(|w| w == needle));
        }
        let value = Value::Map(vec![
            (Value::from("v"), Value::from(1u64)),
            (Value::from("display_name"), Value::from("Alex")),
            (
                Value::from("state"),
                Value::Map(vec![(Value::from("code"), Value::from(STATE_IDLE))]),
            ),
            (
                Value::from("about"),
                Value::Binary(b"about as bytes".to_vec()),
            ),
            (
                Value::from("caps"),
                Value::Array(vec![
                    Value::Binary(b"fetch".to_vec()),
                    Value::from(MARKER),
                    Value::Boolean(true),
                    Value::from("fetch"),
                    Value::Nil,
                    Value::Array(vec![Value::from("nested")]),
                    Value::from("sync"),
                ]),
            ),
            (Value::from("served_at_secs"), Value::from(5u64)),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &value).unwrap();
        patch_marker(&mut bytes);
        let decoded = rmpv::decode::read_value(&mut bytes.as_slice()).unwrap();

        let card = StatusCard::from_value(&decoded).unwrap();

        assert_eq!(card.about, None, "msgpack bin is not a string");
        assert_eq!(card.caps, vec!["fetch".to_string(), "sync".to_string()]);
        assert_eq!(card.display_name.as_deref(), Some("Alex"));
        assert_eq!(card.state.code, STATE_IDLE);
        assert_eq!(card.served_at_secs, 5);
        assert_eq!(StatusCard::from_value(&card.to_value()), Ok(card));

        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &card_with("about", Value::from(MARKER))).unwrap();
        patch_marker(&mut bytes);
        let decoded = rmpv::decode::read_value(&mut bytes.as_slice()).unwrap();
        let card = StatusCard::from_value(&decoded).unwrap();
        assert_eq!(card.about, None, "a str that is not UTF-8 reads as absent");
    }

    #[test]
    fn unknown_caps_are_kept_and_the_maximal_card_round_trips_about_and_caps() {
        let card = StatusCard::from_value(&card_with(
            "caps",
            Value::Array(vec![Value::from("fetch"), Value::from("teleport")]),
        ))
        .unwrap();
        assert_eq!(card.caps, vec!["fetch".to_string(), "teleport".to_string()]);

        let card = maximal_card();
        let decoded = StatusCard::from_value(&card.to_value()).unwrap();
        assert_eq!(decoded.about, card.about);
        assert_eq!(decoded.caps, card.caps);
        assert_eq!(decoded, card);
    }

    #[test]
    fn build_card_sanitises_about_and_copies_caps_as_given() {
        let caps = vec!["fetch".to_string()];
        let card = build_card(
            None,
            None,
            None,
            None,
            Some("  Ask me about the \u{1b}[31mmesh\u{1b}[0m  "),
            &caps,
            now(),
        );
        assert_eq!(card.about.as_deref(), Some("Ask me about the mesh"));
        assert_eq!(card.caps, caps);

        let blank = build_card(None, None, None, None, Some(" \t "), &[], now());
        assert_eq!(blank.about, None);
        assert!(blank.caps.is_empty());
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
    fn card_text_goes_through_display_text_and_never_leaks_paths_or_session_data() {
        let snapshot = adversarial_snapshot();
        let display_name = format!("Zed\u{1b}[2J Quill{}", "\u{1f600}".repeat(70));
        let about = format!("help\u{1b}[31m with\u{200B} the mesh {}", "a".repeat(300));
        let caps = vec!["fetch".to_string()];
        let card = build_card(
            Some(&snapshot),
            None,
            None,
            Some(&display_name),
            Some(&about),
            &caps,
            now(),
        );

        let objective = card.objective.as_deref().unwrap();
        assert_eq!(objective.chars().count(), OBJECTIVE_MAX_CHARS);
        assert!(objective.chars().all(|c| c == 'x'));

        let name = card.display_name.as_deref().unwrap();
        assert_eq!(name.chars().count(), DISPLAY_NAME_MAX_CHARS);
        assert!(name.starts_with("Zed Quill"), "{name:?}");
        assert!(name.ends_with('\u{1f600}'), "{name:?}");

        let about = card.about.as_deref().unwrap();
        assert_eq!(about.chars().count(), ABOUT_MAX_CHARS);
        assert!(about.starts_with("help with the mesh aaa"), "{about:?}");
        assert_eq!(card.caps, caps);

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
    fn display_text_caps_on_a_character_boundary() {
        let sixty_five_accents = "\u{e9}".repeat(65);
        let capped = display_text(&sixty_five_accents, 64).unwrap();
        assert_eq!(capped.chars().count(), 64);
        assert_eq!(capped.len(), 128);
        assert_eq!(capped, "\u{e9}".repeat(64));

        let emoji_at_the_cap = format!("{}\u{1f600}\u{1f600}", "a".repeat(63));
        let capped = display_text(&emoji_at_the_cap, 64).unwrap();
        assert_eq!(capped.chars().count(), 64);
        assert!(capped.ends_with('\u{1f600}'));
        assert_eq!(capped.len(), 63 + 4);

        let space_at_the_cap = format!("{} {}", "a".repeat(63), "b".repeat(5));
        let capped = display_text(&space_at_the_cap, 64).unwrap();
        assert_eq!(capped, "a".repeat(63));

        assert_eq!(display_text("  \u{1b}[31m \r\n\t ", 64), None);
        assert_eq!(display_text("\u{200B}\u{FEFF}", 64), None);
        assert_eq!(display_text("  keep  ", 64).as_deref(), Some("keep"));
    }

    #[test]
    fn repo_name_is_the_root_basename_or_nothing() {
        let mut snapshot = snapshot_fixture();
        snapshot.repo = Some(RepoInfo {
            root: PathBuf::from("/home/u/proj"),
            branch: None,
        });
        let card = build_card(Some(&snapshot), None, None, None, None, &[], now());
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
        let card = build_card(Some(&snapshot), None, None, None, None, &[], now());
        assert_eq!(card.repo, None);
    }

    #[test]
    fn todo_is_absent_when_empty_and_counts_done_items() {
        let mut snapshot = snapshot_fixture();
        snapshot.todo = TodoList::default();
        assert_eq!(
            build_card(Some(&snapshot), None, None, None, None, &[], now()).todo,
            None
        );

        snapshot.todo.goal = "  finish  ".into();
        let card = build_card(Some(&snapshot), None, None, None, None, &[], now());
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
            about: Some("a".repeat(ABOUT_MAX_CHARS)),
            caps: (0..CAPS_MAX_ENTRIES)
                .map(|_| "c".repeat(CAP_MAX_CHARS))
                .collect(),
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
            "about",
            "caps",
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
        assert!(encoded(&build_card(None, None, None, None, None, &[], now())).len() < 100);
    }

    #[test]
    fn human_rendering_keeps_unknown_state_codes() {
        let mut card = build_card(None, None, None, None, None, &[], now());
        card.state.code = 7;
        let text = render_for_human(&card, now());
        assert!(text.contains("state: unknown (7)"), "{text}");
        assert!(text.contains("name: (none)"), "{text}");
        assert!(text.contains("objective: (none)"), "{text}");
        assert!(!text.contains("about:"), "{text}");
        assert!(!text.contains("caps:"), "{text}");
        assert!(!text.contains("since:"), "{text}");
        assert!(!text.contains("repo:"), "{text}");
        assert!(text.contains("served: 0s ago"), "{text}");
    }

    #[test]
    fn human_rendering_saturates_every_peer_supplied_instant() {
        let text = render_for_human(&maximal_card(), now());
        assert!(text.contains("state: working"), "{text}");
        assert!(
            text.contains("since: 0s ago"),
            "an instant past the end of time reads as now: {text}"
        );
        assert!(text.contains("served: 0s ago"), "{text}");
        assert!(
            text.contains(&format!("todo: {}/{}", u32::MAX, u32::MAX)),
            "{text}"
        );
        let early = render_for_human(&maximal_card(), UNIX_EPOCH);
        assert!(early.contains("since: 0s ago"), "{early}");
        let mut aged = build_card(
            Some(&snapshot_fixture()),
            None,
            None,
            None,
            None,
            &[],
            now(),
        );
        aged.state.since_secs = Some(unix_secs(now()) - 3 * 3600);
        let text = render_for_human(&aged, now() + Duration::from_secs(90));
        assert!(text.contains("since: 3h ago"), "{text}");
        assert!(text.contains("served: 1m ago"), "{text}");
    }

    #[test]
    fn human_rendering_shows_the_repo_plan_and_todo_when_present() {
        let text = render_for_human(&maximal_card(), now());
        let repo = "r".repeat(REPO_NAME_MAX_CHARS);
        let branch = "b".repeat(BRANCH_MAX_CHARS);
        assert!(text.contains("\nstate: working\n"), "{text}");
        assert!(
            text.contains(&format!("\nrepo: {repo} ({branch})\n")),
            "{text}"
        );
        assert!(
            text.contains(&format!("\nplan: {}\n", "p".repeat(PLAN_TITLE_MAX_CHARS))),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "\ntodo: {}/{} ({})\n",
                u32::MAX,
                u32::MAX,
                "g".repeat(TODO_GOAL_MAX_CHARS)
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("\nsnapshot: {}s old when served\n", u64::MAX)),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "\nobjective: {}\nabout: {}\ncaps: {}\nstate: working\n",
                "o".repeat(OBJECTIVE_MAX_CHARS),
                "a".repeat(ABOUT_MAX_CHARS),
                vec!["c".repeat(CAP_MAX_CHARS); CAPS_MAX_ENTRIES].join(", ")
            )),
            "{text}"
        );

        let card = build_card(
            Some(&snapshot_fixture()),
            None,
            None,
            Some("Ann"),
            None,
            &[],
            now(),
        );
        let text = render_for_human(&card, now() + Duration::from_secs(5));
        assert!(
            text.starts_with("name: Ann\nobjective: ship it\nstate: idle\n"),
            "{text}"
        );
        assert!(text.contains("served: 5s ago"), "{text}");

        let text = render_for_human(&build_card(None, None, None, None, None, &[], now()), now());
        assert!(text.contains("\nstate: unknown\n"), "{text}");
        let odd = StatusCard {
            state: CardState {
                code: 7,
                since_secs: None,
            },
            ..build_card(None, None, None, None, None, &[], now())
        };
        assert!(
            render_for_human(&odd, now()).contains("\nstate: unknown (7)\n"),
            "{text}"
        );
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
        assert!(
            transport.starts_with(&R3Error::NotRunning.to_string()),
            "{transport}"
        );
        assert!(
            transport.contains("answered live or not at all"),
            "{transport}"
        );
        let malformed = StatusError::Malformed("`v` is 0".into()).to_string();
        assert!(malformed.contains("status card"), "{malformed}");
    }

    /// Spec-first usage probe: `.mesh status <dest>` surfaces `StatusError` through `?`, so
    /// the four causes (transport, not served, unsupported card version, malformed) must
    /// read as four DISTINCT texts, each naming its own cause and remedy.
    #[test]
    fn status_error_texts_are_distinct_per_cause() {
        let transport = StatusError::Transport(R3Error::NotRunning).to_string();
        let not_served = StatusError::NotServed(DispatchError::NoProvider {
            path: STATUS_PATH.to_string(),
        })
        .to_string();
        let unsupported = StatusError::UnsupportedVersion {
            found: 9,
            supported: 1,
        }
        .to_string();
        let malformed = StatusError::Malformed("`v` is 0".into()).to_string();

        let texts = [&transport, &not_served, &unsupported, &malformed];
        for (i, a) in texts.iter().enumerate() {
            for b in texts.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        assert!(
            transport.contains("live or not at all"),
            "transport is live-only: {transport}"
        );
        assert!(
            !not_served.contains("live or not at all")
                && !unsupported.contains("live or not at all")
                && !malformed.contains("live or not at all"),
            "only the transport cause talks about live delivery"
        );
        assert!(not_served.contains(STATUS_PATH), "{not_served}");
        assert!(
            unsupported.contains("version 9") && unsupported.contains("version 1"),
            "both versions are named: {unsupported}"
        );
        assert!(
            unsupported.to_lowercase().contains("upgrade"),
            "the remedy is named: {unsupported}"
        );
        assert!(malformed.contains("`v` is 0"), "{malformed}");
        assert!(malformed.contains("could not be read"), "{malformed}");
        assert!(
            !malformed.contains("version 9") && !not_served.contains("version 9"),
            "no cause borrows another's detail"
        );
    }
}
