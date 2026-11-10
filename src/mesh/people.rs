//! The `people:` section of the trust list: the names shown for a person (an identity) and
//! for each of their sessions (a destination). Names are presentation only. Nothing that
//! decides access, admits a knock or dispatches a request reads them, nothing sent on the
//! wire carries one (the hashes alone identify a peer there), and no log line names one.
//!
//! A person and each of their sessions hold up to two names: the alias this node's owner
//! set, and the one the peer advertised on its status card. The owner's alias takes
//! precedence and is never replaced by a card; a later card replaces the advertised one.
//! Two names that read the same (ignoring case) would make an address ambiguous, so a name
//! that collides is kept but neither shown nor accepted as an address. An owner's alias
//! only gives way to another owner's alias; an advertised name gives way to any name.
#![cfg_attr(not(test), expect(dead_code))] // read once the people commands and display land

use crate::mesh::announce::is_control_or_invisible;
use crate::mesh::card::NAME_MAX_CHARS;
use crate::mesh::display_text;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// The two names `you` and `unnamed` stand for this node's owner and for a person with no
/// name, so neither can be anyone's name.
const RESERVED_NAMES: [&str; 2] = ["you", "unnamed"];

/// A run of this many hex digits or more reads as a hash prefix, which an address takes.
const HASH_LIKE_MIN_CHARS: usize = 8;

pub(crate) const NAME_TAKEN: &str = "Another person or session already has that name (names are compared ignoring case); nothing was changed. `.mesh people list` shows the names in use; pick another.";

/// One person's names, keyed in the trust list by their identity hash.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersonEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) advertised: Option<String>,
    /// Keyed by destination hash.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) sessions: BTreeMap<String, SessionNames>,
}

impl PersonEntry {
    fn is_empty(&self) -> bool {
        self.alias.is_none() && self.advertised.is_none() && self.sessions.is_empty()
    }
}

/// One session's names, under its person.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionNames {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) advertised: Option<String>,
}

impl SessionNames {
    fn is_empty(&self) -> bool {
        self.alias.is_none() && self.advertised.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AliasSource {
    /// Set by this node's owner.
    Owner,
    /// Read from the peer's status card.
    Advertised,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AliasUse {
    /// Shown, and accepted as an address.
    Addressable,
    /// Shown, but it breaks a naming rule, so it is never accepted as an address.
    DisplayOnly,
    /// Another name reads the same, so it is neither shown nor accepted as an address.
    Hidden,
}

/// The name that wins for one person or session, and what it may be used for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alias {
    pub(crate) text: String,
    pub(crate) source: AliasSource,
    pub(crate) status: AliasUse,
}

impl Alias {
    pub(crate) fn displayable(&self) -> bool {
        self.status != AliasUse::Hidden
    }

    pub(crate) fn addressable(&self) -> bool {
        self.status == AliasUse::Addressable
    }
}

/// A naming rule a name breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameRule {
    Empty,
    TooLong,
    Slash,
    Reserved,
    HashLike,
}

impl NameRule {
    fn sentence(self) -> String {
        let why = match self {
            Self::Empty => "A name cannot be empty".to_string(),
            Self::TooLong => {
                format!("A name is at most {NAME_MAX_CHARS} characters")
            }
            Self::Slash => {
                "A name cannot contain `/`, which separates a person from a session".to_string()
            }
            Self::Reserved => "`you` and `unnamed` are reserved and cannot be a name".to_string(),
            Self::HashLike => format!(
                "A name of {HASH_LIKE_MIN_CHARS} or more hex characters would read as a hash"
            ),
        };
        format!("{why}; nothing was changed. {RETRY}")
    }
}

const RETRY: &str = "Give the name again in another form.";

/// The first naming rule `name` breaks, judged on the text as given.
pub(crate) fn broken_rule(name: &str) -> Option<NameRule> {
    if name.trim().is_empty() {
        return Some(NameRule::Empty);
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Some(NameRule::TooLong);
    }
    if name.contains('/') {
        return Some(NameRule::Slash);
    }
    let folded = fold(name);
    if RESERVED_NAMES.contains(&folded.as_str()) {
        return Some(NameRule::Reserved);
    }
    if name.len() >= HASH_LIKE_MIN_CHARS && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(NameRule::HashLike);
    }
    None
}

/// An alias the owner typed, trimmed, or the sentence saying why it is refused. Uniqueness
/// is the caller's, which holds the other names.
pub(crate) fn owner_alias(raw: &str) -> Result<String> {
    if raw.chars().any(is_control_or_invisible) {
        bail!(
            "A name cannot contain control or invisible characters; nothing was changed. {RETRY}"
        );
    }
    let name = raw.trim();
    if let Some(rule) = broken_rule(name) {
        bail!(rule.sentence());
    }
    Ok(name.to_string())
}

/// A name a peer advertised, cleaned as every peer-supplied name is before it is kept;
/// `None` when nothing printable is left.
pub(crate) fn clean_advertised(raw: &str) -> Option<String> {
    display_text(raw, NAME_MAX_CHARS)
}

fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// What two names are compared on: the text as it would be shown, ignoring case. `None`
/// for a stored name with nothing printable left, which counts as unset.
fn name_key(raw: &str) -> Option<String> {
    clean_advertised(raw).map(|text| fold(&text))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Subject {
    Person(String),
    Session {
        identity: String,
        destination: String,
    },
}

impl Subject {
    fn identity(&self) -> &str {
        match self {
            Self::Person(identity) | Self::Session { identity, .. } => identity,
        }
    }
}

/// Two subjects whose names read the same collide, except two sessions of different
/// people: `<person>/<session>` tells those apart.
fn collide(a: &Subject, b: &Subject) -> bool {
    if a == b {
        return false;
    }
    match (a, b) {
        (Subject::Session { .. }, Subject::Session { .. }) => a.identity() == b.identity(),
        _ => true,
    }
}

/// The winning name of every person and session, in one pass so each collision is judged
/// against every other name. Own session names are the local ones this node's owner runs
/// under; a person's name must not read as one of them.
#[derive(Debug, Clone, Default)]
pub(crate) struct People {
    names: HashMap<Subject, Alias>,
}

struct Candidate {
    subject: Subject,
    alias: Alias,
    folded: String,
}

impl People {
    pub(crate) fn new(entries: &BTreeMap<String, PersonEntry>, own_sessions: &[&str]) -> Self {
        let mut candidates = Vec::new();
        for (identity, entry) in entries {
            if let Some(candidate) = candidate(
                Subject::Person(identity.clone()),
                entry.alias.as_deref(),
                entry.advertised.as_deref(),
            ) {
                candidates.push(candidate);
            }
            for (destination, names) in &entry.sessions {
                let subject = Subject::Session {
                    identity: identity.clone(),
                    destination: destination.clone(),
                };
                if let Some(candidate) =
                    candidate(subject, names.alias.as_deref(), names.advertised.as_deref())
                {
                    candidates.push(candidate);
                }
            }
        }
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, candidate) in candidates.iter().enumerate() {
            by_name.entry(&candidate.folded).or_default().push(index);
        }
        let own: Vec<String> = own_sessions
            .iter()
            .filter_map(|name| name_key(name))
            .collect();
        let hidden: Vec<bool> = candidates
            .iter()
            .map(|candidate| {
                let outranked = by_name[candidate.folded.as_str()].iter().any(|&other| {
                    let other = &candidates[other];
                    collide(&candidate.subject, &other.subject)
                        && (candidate.alias.source == AliasSource::Advertised
                            || other.alias.source == AliasSource::Owner)
                });
                let shadows_own = matches!(candidate.subject, Subject::Person(_))
                    && own.contains(&candidate.folded);
                outranked || shadows_own
            })
            .collect();
        let names = candidates
            .into_iter()
            .zip(hidden)
            .map(|(mut candidate, hidden)| {
                if hidden {
                    candidate.alias.status = AliasUse::Hidden;
                }
                (candidate.subject, candidate.alias)
            })
            .collect();
        Self { names }
    }

    /// The name `identity` goes by: the owner's alias when one is set, else the advertised
    /// one. `None` when neither is set.
    pub(crate) fn person(&self, identity: &str) -> Option<&Alias> {
        self.names
            .get(&Subject::Person(identity.to_ascii_lowercase()))
    }

    /// The name the session at `destination` of `identity` goes by, chosen as for a person.
    pub(crate) fn session(&self, identity: &str, destination: &str) -> Option<&Alias> {
        self.names.get(&Subject::Session {
            identity: identity.to_ascii_lowercase(),
            destination: destination.to_ascii_lowercase(),
        })
    }
}

/// The owner's alias shadows the advertised name even when it is itself hidden, so a card
/// can never step in for a name the owner chose. A stored name with nothing printable left
/// once cleaned counts as unset.
fn candidate(subject: Subject, alias: Option<&str>, advertised: Option<&str>) -> Option<Candidate> {
    let (raw, source) = match (alias, advertised) {
        (Some(alias), _) if clean_advertised(alias).is_some() => (alias, AliasSource::Owner),
        (_, Some(advertised)) => (advertised, AliasSource::Advertised),
        _ => return None,
    };
    let text = clean_advertised(raw)?;
    let status = if text == raw.trim() && broken_rule(&text).is_none() {
        AliasUse::Addressable
    } else {
        AliasUse::DisplayOnly
    };
    Some(Candidate {
        folded: fold(&text),
        subject,
        alias: Alias {
            text,
            source,
            status,
        },
    })
}

/// Whether an owner's alias `name` for the person `identity`, or for their session at
/// `destination`, reads the same as another owner's alias or, for a person, as one of the
/// owner's own sessions. Advertised names never block the owner: they give way instead.
pub(crate) fn owner_alias_taken(
    entries: &BTreeMap<String, PersonEntry>,
    identity: &str,
    destination: Option<&str>,
    name: &str,
    own_sessions: &[&str],
) -> bool {
    let Some(wanted) = name_key(name) else {
        return false;
    };
    let subject = subject_of(identity, destination);
    if matches!(subject, Subject::Person(_))
        && own_sessions
            .iter()
            .any(|own| name_key(own).as_ref() == Some(&wanted))
    {
        return true;
    }
    let same = |alias: &str| name_key(alias).as_ref() == Some(&wanted);
    entries.iter().any(|(other_identity, entry)| {
        let person = Subject::Person(other_identity.clone());
        let person_taken = entry
            .alias
            .as_deref()
            .is_some_and(|alias| same(alias) && collide(&subject, &person));
        person_taken
            || entry.sessions.iter().any(|(other_destination, names)| {
                let session = subject_of(other_identity, Some(other_destination));
                names
                    .alias
                    .as_deref()
                    .is_some_and(|alias| same(alias) && collide(&subject, &session))
            })
    })
}

fn subject_of(identity: &str, destination: Option<&str>) -> Subject {
    match destination {
        None => Subject::Person(identity.to_string()),
        Some(destination) => Subject::Session {
            identity: identity.to_string(),
            destination: destination.to_string(),
        },
    }
}

/// Which of a subject's two names a write sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
    Alias,
    Advertised,
}

/// Sets or clears one name of the person `identity` (or of their session at
/// `destination`), dropping whatever entry is left empty.
pub(crate) fn set_name(
    entries: &mut BTreeMap<String, PersonEntry>,
    identity: &str,
    destination: Option<&str>,
    slot: Slot,
    value: Option<String>,
) {
    let entry = entries.entry(identity.to_string()).or_default();
    match destination {
        None => {
            let target = match slot {
                Slot::Alias => &mut entry.alias,
                Slot::Advertised => &mut entry.advertised,
            };
            *target = value;
        }
        Some(destination) => {
            let names = entry.sessions.entry(destination.to_string()).or_default();
            let target = match slot {
                Slot::Alias => &mut names.alias,
                Slot::Advertised => &mut names.advertised,
            };
            *target = value;
            if names.is_empty() {
                entry.sessions.remove(destination);
            }
        }
    }
    if entry.is_empty() {
        entries.remove(identity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> String {
        format!("{byte:02x}").repeat(16)
    }

    fn person(alias: Option<&str>, advertised: Option<&str>) -> PersonEntry {
        PersonEntry {
            alias: alias.map(str::to_string),
            advertised: advertised.map(str::to_string),
            sessions: BTreeMap::new(),
        }
    }

    fn with_session(
        mut entry: PersonEntry,
        destination: &str,
        alias: Option<&str>,
        advertised: Option<&str>,
    ) -> PersonEntry {
        entry.sessions.insert(
            destination.to_string(),
            SessionNames {
                alias: alias.map(str::to_string),
                advertised: advertised.map(str::to_string),
            },
        );
        entry
    }

    #[test]
    fn the_naming_rules_refuse_each_shape_and_pass_ordinary_names() {
        let refused: [(&str, NameRule); 11] = [
            ("", NameRule::Empty),
            ("   ", NameRule::Empty),
            (&"x".repeat(NAME_MAX_CHARS + 1), NameRule::TooLong),
            ("alex/review", NameRule::Slash),
            ("you", NameRule::Reserved),
            ("YOU", NameRule::Reserved),
            ("Unnamed", NameRule::Reserved),
            ("deadbeef", NameRule::HashLike),
            ("DEADBEEF00", NameRule::HashLike),
            ("0123456789", NameRule::HashLike),
            (&"a".repeat(32), NameRule::HashLike),
        ];
        for (name, rule) in refused {
            assert_eq!(broken_rule(name), Some(rule), "{name:?}");
        }
        for name in [
            "Alex",
            "review",
            "deadbee",
            "beef cafe",
            "younger",
            "x".repeat(NAME_MAX_CHARS).as_str(),
            "\u{00e9}lodie",
        ] {
            assert_eq!(broken_rule(name), None, "{name:?}");
        }
    }

    #[test]
    fn an_owner_alias_is_trimmed_and_refused_with_a_sentence_that_never_repeats_it() {
        assert_eq!(owner_alias("  Alex  ").unwrap(), "Alex");
        let length = "\u{00e9}".repeat(NAME_MAX_CHARS);
        assert_eq!(
            owner_alias(&length).unwrap(),
            length,
            "the limit counts characters"
        );
        for (raw, needle) in [
            ("al\u{202e}ex", "control or invisible"),
            ("al\u{1b}[31mex", "control or invisible"),
            ("", "cannot be empty"),
            ("alex/review", "cannot contain `/`"),
            ("you", "reserved"),
            ("cafebabe", "read as a hash"),
        ] {
            let err = owner_alias(raw).unwrap_err().to_string();
            assert!(err.contains(needle), "{raw:?}: {err}");
            assert!(err.contains("; nothing was changed. "), "{err}");
            assert!(
                err.ends_with(RETRY),
                "the refusal says what to do next: {err}"
            );
            if !raw.is_empty() && broken_rule(raw) != Some(NameRule::Reserved) {
                assert!(!err.contains(raw), "the refusal repeats the name: {err}");
            }
        }
        let err = owner_alias(&"x".repeat(NAME_MAX_CHARS + 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at most 64 characters"), "{err}");
    }

    #[test]
    fn an_advertised_name_is_cleaned_and_cut_at_the_name_limit() {
        assert_eq!(
            clean_advertised(" \u{1b}[1mAlex\u{200b} ").as_deref(),
            Some("Alex")
        );
        assert_eq!(clean_advertised("\u{200b}  "), None);
        let long = "y".repeat(NAME_MAX_CHARS + 10);
        assert_eq!(
            clean_advertised(&long).unwrap().chars().count(),
            NAME_MAX_CHARS
        );
    }

    #[test]
    fn the_owner_alias_wins_over_the_advertised_name() {
        let (a, d) = (hash(1), hash(2));
        let mut entries = BTreeMap::new();
        entries.insert(
            a.clone(),
            with_session(
                person(Some("Alex"), Some("Alexandra")),
                &d,
                Some("review"),
                Some("temp"),
            ),
        );
        let people = People::new(&entries, &[]);
        let name = people.person(&a).unwrap();
        assert_eq!(name.text, "Alex");
        assert_eq!(name.source, AliasSource::Owner);
        assert!(name.addressable());
        let session = people.session(&a, &d).unwrap();
        assert_eq!(
            (session.text.as_str(), session.source),
            ("review", AliasSource::Owner)
        );

        entries.insert(a.clone(), person(None, Some("Alexandra")));
        let people = People::new(&entries, &[]);
        let name = people.person(&a.to_ascii_uppercase()).unwrap();
        assert_eq!(
            (name.text.as_str(), name.source),
            ("Alexandra", AliasSource::Advertised)
        );
        assert_eq!(people.session(&a, &d), None);
        assert_eq!(people.person(&hash(9)), None);
    }

    #[test]
    fn a_colliding_advertised_name_is_hidden_and_the_owner_alias_it_collides_with_is_not() {
        let (a, b, c) = (hash(1), hash(2), hash(3));
        let mut entries = BTreeMap::new();
        entries.insert(a.clone(), person(Some("Sam"), None));
        entries.insert(b.clone(), person(None, Some("sam")));
        entries.insert(c.clone(), person(None, Some("Kim")));
        let people = People::new(&entries, &[]);
        assert_eq!(people.person(&a).unwrap().status, AliasUse::Addressable);
        let hidden = people.person(&b).unwrap();
        assert_eq!(hidden.text, "sam", "the colliding name is kept");
        assert_eq!(hidden.status, AliasUse::Hidden);
        assert!(!hidden.displayable() && !hidden.addressable());
        assert!(people.person(&c).unwrap().addressable());
    }

    #[test]
    fn two_colliding_names_of_the_same_rank_are_both_hidden() {
        let (a, b, c, d) = (hash(1), hash(2), hash(3), hash(4));
        let mut entries = BTreeMap::new();
        entries.insert(a.clone(), person(Some("Robin"), None));
        entries.insert(b.clone(), person(Some("ROBIN"), None));
        entries.insert(c.clone(), person(None, Some("Kai")));
        entries.insert(d.clone(), person(None, Some("kai")));
        let people = People::new(&entries, &[]);
        for identity in [&a, &b, &c, &d] {
            assert_eq!(people.person(identity).unwrap().status, AliasUse::Hidden);
        }
    }

    #[test]
    fn session_names_collide_within_a_person_and_with_every_person_name() {
        let (a, b) = (hash(1), hash(2));
        let (d1, d2, d3) = (hash(11), hash(12), hash(13));
        let mut entries = BTreeMap::new();
        let alex = with_session(person(Some("Alex"), None), &d1, None, Some("review"));
        let alex = with_session(alex, &d2, None, Some("Review"));
        entries.insert(a.clone(), alex);
        entries.insert(
            b.clone(),
            with_session(person(Some("Bea"), None), &d3, None, Some("review")),
        );
        let people = People::new(&entries, &[]);
        assert_eq!(people.session(&a, &d1).unwrap().status, AliasUse::Hidden);
        assert_eq!(people.session(&a, &d2).unwrap().status, AliasUse::Hidden);
        assert!(
            people.session(&b, &d3).unwrap().addressable(),
            "another person's session may share the name"
        );

        entries.insert(
            b.clone(),
            with_session(person(Some("Bea"), None), &d3, None, Some("alex")),
        );
        let people = People::new(&entries, &[]);
        assert_eq!(people.session(&b, &d3).unwrap().status, AliasUse::Hidden);
        assert!(people.person(&a).unwrap().addressable());
    }

    #[test]
    fn a_person_name_reading_as_an_own_session_is_hidden() {
        let a = hash(1);
        let d = hash(11);
        let mut entries = BTreeMap::new();
        entries.insert(
            a.clone(),
            with_session(person(Some("Main"), None), &d, None, Some("main")),
        );
        let people = People::new(&entries, &["main"]);
        assert_eq!(people.person(&a).unwrap().status, AliasUse::Hidden);
        assert_eq!(
            people.session(&a, &d).unwrap().status,
            AliasUse::Hidden,
            "it still collides with the person name"
        );
        let people = People::new(&entries, &["other"]);
        assert!(people.person(&a).unwrap().addressable());
    }

    #[test]
    fn a_name_breaking_a_rule_is_shown_but_never_addressable() {
        let (a, b) = (hash(1), hash(2));
        let mut entries = BTreeMap::new();
        entries.insert(a.clone(), person(None, Some("you")));
        entries.insert(b.clone(), person(Some("bad\u{1b}[2Jname"), None));
        let people = People::new(&entries, &[]);
        let reserved = people.person(&a).unwrap();
        assert_eq!(reserved.status, AliasUse::DisplayOnly);
        assert!(reserved.displayable() && !reserved.addressable());
        let edited = people.person(&b).unwrap();
        assert_eq!(
            edited.text, "badname",
            "a hand-edited name is cleaned for display"
        );
        assert_eq!(edited.status, AliasUse::DisplayOnly);
    }

    #[test]
    fn a_hidden_owner_alias_still_shadows_the_advertised_name() {
        let (a, b) = (hash(1), hash(2));
        let mut entries = BTreeMap::new();
        entries.insert(a.clone(), person(Some("Jo"), Some("Joanna")));
        entries.insert(b.clone(), person(Some("jo"), None));
        let people = People::new(&entries, &[]);
        let name = people.person(&a).unwrap();
        assert_eq!(
            (name.text.as_str(), name.source, name.status),
            ("Jo", AliasSource::Owner, AliasUse::Hidden)
        );
    }

    #[test]
    fn an_owner_alias_is_taken_only_by_another_owner_alias_or_an_own_session() {
        let (a, b) = (hash(1), hash(2));
        let (d1, d2) = (hash(11), hash(12));
        let mut entries = BTreeMap::new();
        entries.insert(
            a.clone(),
            with_session(person(Some("Alex"), Some("Lex")), &d1, Some("review"), None),
        );
        let taken = |identity: &str, destination: Option<&str>, name: &str, own: &[&str]| {
            owner_alias_taken(&entries, identity, destination, name, own)
        };
        assert!(taken(&b, None, "ALEX", &[]));
        assert!(!taken(&b, None, "lex", &[]), "an advertised name gives way");
        assert!(
            !taken(&a, None, "alex", &[]),
            "a person may keep their own name"
        );
        assert!(
            taken(&b, None, "Review", &[]),
            "a person name against a session"
        );
        assert!(taken(&a, Some(&d2), "review", &[]), "a sibling session");
        assert!(
            !taken(&b, Some(&d2), "review", &[]),
            "another person's session"
        );
        assert!(
            taken(&a, Some(&d2), "alex", &[]),
            "a session against a person"
        );
        assert!(taken(&b, None, "main", &["Main"]));
        assert!(!taken(&a, Some(&d2), "main", &["Main"]));
        assert!(
            taken(&b, None, "alex\u{200b}", &[]),
            "names compare as they are shown"
        );
        assert!(NAME_TAKEN.contains("`.mesh people list`"), "{NAME_TAKEN}");
    }

    #[test]
    fn setting_and_clearing_names_drops_empty_entries() {
        let (a, d) = (hash(1), hash(11));
        let mut entries = BTreeMap::new();
        set_name(
            &mut entries,
            &a,
            Some(&d),
            Slot::Advertised,
            Some("temp".into()),
        );
        set_name(&mut entries, &a, None, Slot::Alias, Some("Alex".into()));
        assert_eq!(
            entries[&a],
            with_session(person(Some("Alex"), None), &d, None, Some("temp"))
        );
        set_name(
            &mut entries,
            &a,
            Some(&d),
            Slot::Advertised,
            Some("named".into()),
        );
        assert_eq!(
            entries[&a].sessions[&d].advertised.as_deref(),
            Some("named"),
            "a later card replaces the advertised name"
        );
        set_name(&mut entries, &a, Some(&d), Slot::Advertised, None);
        assert!(entries[&a].sessions.is_empty());
        set_name(&mut entries, &a, None, Slot::Alias, None);
        assert!(entries.is_empty());
        set_name(&mut entries, &a, None, Slot::Advertised, None);
        assert!(
            entries.is_empty(),
            "clearing nothing leaves no entry behind"
        );
    }
}
