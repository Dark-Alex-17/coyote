use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// A single named hook: an external command to run when its event fires.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HookDef {
    pub name: String,
    pub command: String,
}

/// Ordered mapping of event name -> hook definitions, shared by every config scope.
pub type HooksMap = IndexMap<String, Vec<HookDef>>;
