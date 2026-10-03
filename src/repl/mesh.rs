use crate::config::mesh_config::{
    MESH_INFO_LABEL_WIDTH, MeshBrief, MeshInterface, render_mesh_info,
};
use crate::config::{MeshConfig, RequestContext};
use crate::function::mesh::trust_label;
use crate::mesh::card::{CardSource, DISPLAY_NAME_MAX_CHARS, StatusHandler, render_for_human};
use crate::mesh::identity::{self, Predecessor, fingerprint};
use crate::mesh::idle::plural;
use crate::mesh::knock::{KnockIntro, KnockOutcome, KnockVia};
use crate::mesh::knocks::KnockRecord;
use crate::mesh::message::{
    BroadcastOutcome, Disposition, OutboundPeer, PEER_CONTENT_MAX_CHARS, Part, PeerKind,
    PeerMessage, PeerVia, RecipientOutcome,
};
use crate::mesh::pending::{
    Correlation, InboundKind, InboundRecord, PendingState, access_not_a_question,
};
use crate::mesh::shares::{
    BuiltinHit, DEFAULT_LIST_WALK_BOUND, GLOB_METACHARACTERS, LIST_PAGE_SIZE, Layer, MatchCount,
    Mutation, PeerRef, RawKind, ShareLocations, ShareSet, Verdict as ShareVerdict, WriteScope,
    canonical_peer, case_folding_hint, is_broad_pattern, is_canonical_peer, validate_override,
    validate_pattern,
};
use crate::mesh::trust::{
    Decision, KeyChange, LiveMesh, Rule, Tier, TrustChange, TrustOptions, TrustRecord, TrustStore,
    UntrustOutcome, Verdict, decode_name_hash, parse_hash,
};
use crate::mesh::wire_path::WIRE_PATH_MAX_BYTES;
use crate::mesh::{
    FetchError, FetchReport, LoggingInboundSink, MAX_WANTS_PER_FETCH, MESH_ALREADY_ON, MeshPaths,
    MeshRuntime, NodeOptions, PeerRecord, PropagationNodeRecord, age_text, canonical_hash,
    destination_address, display_text, parse_rfc3339, redact_hashes, short,
};
use crate::supervisor::mailbox::EnvelopePayload;
use crate::utils::{AbortSignal, drain_stale_tty_input, wait_user_interrupt};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use inquire::Confirm;
use log::debug;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// (verb, one-line description, example), in `.mesh` help order.
pub(super) const VERBS: &[(&str, &str, &str)] = &[
    (
        "on",
        "Join the mesh for this session only; config.yaml is not changed",
        ".mesh on [--yes] [--fresh]",
    ),
    (
        "off",
        "Leave the mesh and drop the mesh__* tools",
        ".mesh off [--yes]",
    ),
    ("peers", "List the nodes heard on the mesh", ".mesh peers"),
    (
        "knocks",
        "List the untrusted nodes that knocked",
        ".mesh knocks",
    ),
    (
        "info",
        "Show the mesh settings and this node, or one peer",
        ".mesh info [<destination>]",
    ),
    (
        "status",
        "Show this node's status card, set its objective, or fetch a peer's card",
        ".mesh status [\"objective\"|clear|<destination>]",
    ),
    (
        "brief",
        "Show the brief peers receive, or set its text or mode",
        ".mesh brief [set \"text\"|clear|auto|manual|off]",
    ),
    (
        "inbox",
        "Drain the peer messages waiting for this node",
        ".mesh inbox",
    ),
    (
        "pending",
        "List the questions this node asked and the ones peers escalated to you",
        ".mesh pending",
    ),
    (
        "answer",
        "Answer an escalated question, or follow up on one this node asked",
        ".mesh answer <id> \"text\"",
    ),
    (
        "reply",
        "Send your own text to one peer, bypassing the model",
        ".mesh reply <destination> [--yes] \"text\"",
    ),
    (
        "broadcast",
        "Send a bulletin to every trusted peer with a known path",
        ".mesh broadcast [--yes] \"text\"",
    ),
    (
        "trust",
        "Trust one instance, or every instance of an identity, or prune stale trusted instances",
        ".mesh trust <destination> [--yes] [--label \"text\"] | --identity <identity> [--yes] [--label \"text\"] | --prune [--older-than 30d] [--dry-run|--confirm prune-<N>]",
    ),
    (
        "untrust",
        "Forget a trusted instance, or an identity with every instance bound to it; `untrust` forgets (an instance of an identity trusted for all is refused instead), `block` remembers and refuses",
        ".mesh untrust <destination> [--yes|--dry-run] | --identity <identity> [--dry-run|--confirm untrust-<identity-short>]",
    ),
    (
        "forget",
        "alias of `untrust`: forget this peer",
        ".mesh forget <destination> [--yes|--dry-run] | --identity <identity> [--dry-run|--confirm untrust-<identity-short>]",
    ),
    (
        "block",
        "Refuse every instance of an identity, including ones you trusted, and drop its knocks; `block` remembers where `untrust` only forgets",
        ".mesh block <identity> [--yes] [--note \"text\"]",
    ),
    (
        "unblock",
        "Lift a block so the identity may knock again",
        ".mesh unblock <identity> [--yes]",
    ),
    (
        "rotate",
        "Mint a new mesh identity while the node is off; peers must re-trust the new one",
        ".mesh rotate [--dry-run|--confirm rotate-<identity-short>]",
    ),
    (
        "sync",
        "Sync the messages a propagation node holds for this node now",
        ".mesh sync",
    ),
    (
        "knock",
        "Ask an untrusted peer to trust this instance, with an optional intro",
        ".mesh knock <destination> [--yes] [--intro \"text\"]",
    ),
    (
        "allow",
        "Share files matching a pattern with every trusted peer, or with one peer",
        ".mesh allow docs/** [--peer <identity|destination>] [--force] [--global|--workspace] [--yes|--dry-run]",
    ),
    (
        "deny",
        "Never share files matching a pattern, whatever `allow` says; a share rule, not a peer refusal (that is `untrust`)",
        ".mesh deny \"src/vault/*\" [--global|--workspace] [--yes|--dry-run]",
    ),
    (
        "unshare",
        "Remove the allow or deny share rule that holds a pattern",
        ".mesh unshare docs/** [--global|--workspace] [--yes|--dry-run]",
    ),
    (
        "shares",
        "List the share rules and the file each came from, or the files a peer can fetch",
        ".mesh shares [--peer <identity|destination>] [--effective]",
    ),
];

pub(crate) const MESH_OFF: &str = "Mesh is off. Run `.mesh on` first.";
const ROTATE_NEEDS_OFF: &str = "Mesh is on. Run `.mesh off` first; the identity is rotated only while no session's node on this config dir is running, then `.mesh on` announces the new one.";
const BROADCAST_NOTICE: &str = "This sends a bulletin to every peer this node trusts that has a known path right now. Peers you have not trusted receive nothing.";
const REPLY_REFUSAL_TAIL: &str = "Nothing is sent to a destination this node does not trust.";
const STATUS_REFUSAL_TAIL: &str = "Status is only requested from trusted destinations.";
const KNOCK_REFUSAL_TAIL: &str = "A knock is not sent to a destination this node has denied or an identity it has blocked; `.mesh trust <destination>` / `.mesh unblock` lift that.";
const INBOX_CONTENT_MAX_CHARS: usize = 200;
const NOTHING_CHANGED: &str = "Nothing was changed.";
const DRY_RUN_NOTHING_CHANGED: &str = "This was a dry run; nothing changed.";
const SHARE_ROOT_UNKNOWN: &str =
    "The share root is unknown until a turn completes in this session; run one, then try again.";
/// Above this many matching files a share pattern is confirmed before it is written.
const BROAD_MATCH_LIMIT: usize = 100;
/// How long a trusted instance goes unheard before `.mesh trust --prune` lists it, when
/// `--older-than` is not given.
const PRUNE_DEFAULT_OLDER_THAN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub(crate) async fn run(
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
    args: Option<&str>,
) -> Result<()> {
    let Some((verb, rest)) = split_verb(args) else {
        out_text(&render_help());
        return Ok(());
    };
    match verb {
        "on" => turn_on(ctx, rest).await,
        "off" => turn_off(ctx, rest).await,
        "peers" => peers(ctx),
        "knocks" => knocks(ctx),
        "info" => info(ctx, rest),
        "status" => status(ctx, &abort_signal, rest).await,
        "brief" => brief(ctx, rest),
        "inbox" => inbox(ctx),
        "pending" => pending(ctx),
        "answer" => answer(ctx, rest).await,
        "reply" => reply(ctx, rest).await,
        "broadcast" => broadcast(ctx, rest).await,
        "trust" => trust(ctx, rest),
        "untrust" | "forget" => untrust(ctx, verb, rest),
        "block" => block(ctx, rest),
        "unblock" => unblock(ctx, rest),
        "rotate" => rotate(ctx, rest),
        "sync" => sync(ctx, &abort_signal, rest).await,
        "knock" => knock(ctx, &abort_signal, rest).await,
        "allow" => allow(ctx, rest),
        "deny" => deny(ctx, rest),
        "unshare" => unshare(ctx, rest),
        "shares" => shares(ctx, rest),
        other => bail!("Unknown .mesh command '{other}'. Type `.mesh` for the list."),
    }
}

async fn turn_on(ctx: &mut RequestContext, rest: Option<&str>) -> Result<()> {
    let args = parse_args(rest, &["--yes", "--fresh"], "on")?;
    join(
        ctx,
        JoinOptions {
            yes: args.has("--yes"),
            fresh: args.has("--fresh"),
        },
    )
    .await
}

/// Joins the mesh for this session because config.yaml asked for it; the only
/// difference from `.mesh on --yes` is the line saying where the decision came from.
pub(crate) async fn autostart(ctx: &mut RequestContext) -> Result<()> {
    out_text(
        "mesh.enabled is true in config.yaml: joining the mesh for this session (`.mesh off` leaves it).",
    );
    join(
        ctx,
        JoinOptions {
            yes: true,
            fresh: false,
        },
    )
    .await
}

struct JoinOptions {
    yes: bool,
    fresh: bool,
}

async fn join(ctx: &mut RequestContext, options: JoinOptions) -> Result<()> {
    let JoinOptions { yes, fresh } = options;
    if ctx.app.mesh.get().is_some() {
        bail!(MESH_ALREADY_ON);
    }
    let config = ctx.app.config.mesh.clone();
    let function_calling_support = ctx.app.config.function_calling_support;
    let mut enabled_view = config.clone();
    enabled_view.enabled = true;
    enabled_view.validate(function_calling_support)?;
    let Some(session_name) = ctx
        .session
        .as_ref()
        .map(|session| session.name().to_string())
    else {
        bail!(
            "Mesh needs a session: this node's destination is derived from an id kept in the session file. Run `.session <name>` first."
        );
    };
    let cwd = env::current_dir()?;
    out_text(&render_on_preview(
        &config,
        &session_name,
        fresh,
        shared_count(ctx),
    ));
    if let Some(warning) = cwd_warning(&cwd, dirs::home_dir().as_deref()) {
        err_text(&warning);
    }
    if !confirm_or_flag(&on_question(&config), "--yes", yes)? {
        out_text("Mesh stays off.");
        return Ok(());
    }
    let Some(session) = ctx.session.as_mut() else {
        bail!("The session went away before the mesh could start. Run `.mesh on` again.");
    };
    // The start runs against a copy so a refused start leaves the live session's id, and
    // the on-disk state keyed by it, exactly as it was. Only `mesh_instance_id` is copied
    // back, which relies on `MeshRuntime::start` touching nothing else on the trial.
    let mut trial = session.clone();
    if fresh {
        trial.remint_mesh_instance_id();
    }
    let runtime = MeshRuntime::start(
        &config,
        function_calling_support,
        &mut trial,
        MeshPaths::from_env(),
        NodeOptions {
            hooks: ctx.app.mesh.hooks(),
            ..NodeOptions::default()
        },
    )
    .await?;
    if let Some(id) = trial.mesh_instance_id() {
        session.set_mesh_instance_id(id.to_string());
    }
    ctx.app.mesh.install(Arc::clone(&runtime))?;
    ctx.set_mesh_enabled_for_session(true);
    let app = Arc::clone(&ctx.app.config);
    ctx.refresh_mesh_tools(&app);
    out_text(&render_on_summary(&runtime, fresh));
    Ok(())
}

async fn turn_off(ctx: &mut RequestContext, rest: Option<&str>) -> Result<()> {
    let args = parse_args(rest, &["--yes"], "off")?;
    if ctx.app.mesh.get().is_none() {
        ctx.set_mesh_enabled_for_session(false);
        let app = Arc::clone(&ctx.app.config);
        ctx.refresh_mesh_tools(&app);
        out_text("Mesh is already off for this session.");
        return Ok(());
    }
    let question =
        "Turn mesh off? Peers lose this node and the mesh__* tools leave the tool catalog.";
    if !confirm_or_flag(question, "--yes", args.has("--yes"))? {
        out_text("Mesh stays on.");
        return Ok(());
    }
    let stopped = ctx.app.mesh.stop().await;
    ctx.app.mesh.clear_digest_for_new_epoch();
    ctx.set_mesh_enabled_for_session(false);
    let app = Arc::clone(&ctx.app.config);
    ctx.refresh_mesh_tools(&app);
    stopped?;
    out_text("Mesh is off for this session. Run `.mesh on` to join again.");
    Ok(())
}

fn peers(ctx: &RequestContext) -> Result<()> {
    let runtime = live(ctx)?;
    let now = SystemTime::now();
    let mut records = runtime.peers().snapshot();
    records.sort_by_key(|peer| std::cmp::Reverse(peer.last_seen));
    let trust = runtime.trust();
    let heard: Vec<String> = records
        .iter()
        .map(|peer| peer.destination_hash.clone())
        .collect();
    let trust_records = trust.records();
    let mut rows: Vec<PeerRow> = records
        .iter()
        .map(|peer| {
            let label = trust_label(trust.authorize(&peer.identity_hash, &peer.destination_hash));
            let mark = key_change_mark(peer, &trust_records);
            PeerRow::Heard(peer.clone(), label, mark)
        })
        .collect();
    let successors: Vec<(String, &PeerRecord)> = records
        .iter()
        .filter_map(|peer| {
            let name_hash = decode_name_hash(&peer.name_hash)?;
            Some(
                trust
                    .binding_conflicts(&peer.identity_hash, &name_hash)
                    .into_iter()
                    .map(move |conflict| (conflict.destination_hash, peer)),
            )
        })
        .flatten()
        .collect();
    rows.extend(unheard_rows(trust_records, &heard, &successors, |record| {
        unheard_label(&trust, record)
    }));
    out_text(&render_peers(&rows, now));
    Ok(())
}

/// The trust column for a record no heard peer announces from: the store's verdict for the
/// identity it is bound to, so a stale record of a since-blocked identity reads `blocked`.
fn unheard_label(trust: &TrustStore, record: &TrustRecord) -> &'static str {
    trust_label(trust.authorize(record.identity.as_deref().unwrap_or_default(), &record.hash))
}

/// The key-change mark on `peer`'s trust record, paired with the destination the same
/// instance derives under the identity that caused the mark.
fn key_change_mark(peer: &PeerRecord, trust_records: &[TrustRecord]) -> Option<KeyChangeMark> {
    let change = trust_records
        .iter()
        .find(|record| record.hash == peer.destination_hash)?
        .key_changed
        .clone()?;
    let new_destination = decode_name_hash(&peer.name_hash)
        .zip(parse_hash(&change.seen_identity))
        .map(|(name_hash, seen)| destination_address(&name_hash, &seen).to_hex_string());
    Some(KeyChangeMark {
        change,
        new_destination,
    })
}

fn knocks(ctx: &RequestContext) -> Result<()> {
    let runtime = live(ctx)?;
    let now = SystemTime::now();
    let records = runtime.knock_gate().cache().list(now)?;
    out_text(&render_knocks(&records, now));
    Ok(())
}

fn info(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        let mut text = render_mesh_info(&ctx.app.config.mesh);
        text.push_str(&format!(
            "  {:<MESH_INFO_LABEL_WIDTH$}{}\n",
            "reach",
            reach_line(&ctx.app.config.mesh)
        ));
        match ctx.app.mesh.get() {
            Some(runtime) => {
                let predecessors = identity::predecessors(&MeshPaths::from_env().identity_path);
                text.push_str(&render_node_facts(
                    &runtime,
                    predecessors.as_deref(),
                    SystemTime::now(),
                ));
            }
            None => text.push_str(&format!("  {:<MESH_INFO_LABEL_WIDTH$}off\n", "node")),
        }
        out_text(text.trim_end());
        return Ok(());
    };
    let runtime = live(ctx)?;
    let Some(destination) = canonical_hash(rest.trim()) else {
        bail!(
            "'{}' is not a destination hash: expected 32 hex characters, as `.mesh peers` lists them.",
            rest.trim()
        );
    };
    let now = SystemTime::now();
    let peer = runtime.peers().get(&destination);
    let knock = match runtime.knock_gate().cache().list(now) {
        Ok(knocks) => match knocks
            .into_iter()
            .find(|knock| knock.destination_hash == destination)
        {
            Some(knock) => KnockLookup::Knocked(knock),
            None => KnockLookup::NotKnocked,
        },
        Err(err) if peer.is_some() => {
            debug!(
                "knock cache unreadable while describing a known peer: {}",
                redact_hashes(&format!("{err:#}"))
            );
            KnockLookup::Unreadable
        }
        Err(err) => return Err(err),
    };
    if peer.is_none() && !matches!(knock, KnockLookup::Knocked(_)) {
        bail!(
            "Destination {destination} has not been heard from: it is not in the peer table and has not knocked. Run `.mesh peers` or `.mesh knocks` to see who has."
        );
    }
    let trust = peer
        .as_ref()
        .map(|peer| trust_label(runtime.trust().authorize(&peer.identity_hash, &destination)));
    out_text(&render_peer_detail(
        &destination,
        peer.as_ref(),
        &knock,
        trust,
        now,
    ));
    if matches!(knock, KnockLookup::Unreadable) {
        err_text(
            "The knock cache could not be read, so whether this peer knocked is unknown; `.mesh knocks` reports the error.",
        );
    }
    Ok(())
}

async fn status(
    ctx: &RequestContext,
    abort_signal: &AbortSignal,
    rest: Option<&str>,
) -> Result<()> {
    match classify_status(rest) {
        StatusArg::Own => {
            live(ctx)?;
            let source: Arc<dyn CardSource> = ctx.app.mesh.clone();
            let card = StatusHandler::new(Arc::downgrade(&source)).card(SystemTime::now());
            out_text(&render_for_human(&card, SystemTime::now()));
        }
        StatusArg::Set(objective) => {
            ctx.app
                .mesh
                .set_objective_override(Some(objective.to_string()));
            out_text(&format!(
                "Objective set to \"{objective}\" for this session; peers see it in this node's status card and brief. `.mesh status clear` removes it."
            ));
        }
        StatusArg::Clear => {
            ctx.app.mesh.set_objective_override(None);
            out_text("Objective override cleared; the card shows the session's own objective.");
        }
        StatusArg::Fetch(destination) => {
            let runtime = live(ctx)?;
            peer_for_contact(&runtime, &destination, STATUS_REFUSAL_TAIL)?;
            let Some(desc) = runtime.resolve_destination(&destination).await else {
                bail!(
                    "Destination {destination} cannot be reached yet: its announce has not been heard since this node started. Wait for it to announce, or check `.mesh peers`."
                );
            };
            out_text(&format!(
                "Asking {} for its status card; Ctrl-C cancels...",
                short(&destination)
            ));
            let card = tokio::select! {
                outcome = runtime.request_status(&desc) => outcome?,
                _ = wait_user_interrupt(Some(abort_signal)) => {
                    out_text("Status request interrupted.");
                    return Ok(());
                }
            };
            out_text(&render_for_human(&card, SystemTime::now()));
        }
        StatusArg::Help => out_text(&render_verb_help("status")),
    }
    Ok(())
}

fn brief(ctx: &mut RequestContext, rest: Option<&str>) -> Result<()> {
    match classify_brief(rest) {
        BriefArg::Show => {
            let mode = ctx.app.config.mesh.brief;
            let text = match ctx.app.mesh.brief() {
                Some(brief) => format!("brief mode: {mode}\n{}", brief.render_for_human()),
                None => format!(
                    "brief mode: {mode}\nNo brief is being served yet: it is assembled at the first turn boundary after the mesh is on."
                ),
            };
            out_text(&text);
        }
        BriefArg::Set(text) => {
            ctx.app.mesh.set_user_brief(Some(text.to_string()));
            out_text("Brief text set for this session; peers receive it with the next request.");
        }
        BriefArg::Clear => {
            ctx.app.mesh.set_user_brief(None);
            out_text("Brief text cleared.");
        }
        BriefArg::Mode(mode) => {
            ctx.set_mesh_brief_for_session(mode);
            out_text(&format!(
                "Brief mode is {mode} for this session; config.yaml is not changed. It takes effect at the next turn boundary."
            ));
        }
        BriefArg::Help => out_text(&render_verb_help("brief")),
    }
    Ok(())
}

fn inbox(ctx: &RequestContext) -> Result<()> {
    let (envelopes, dropped) = ctx.app.mesh.peer_inbox().drain();
    let rows: Vec<InboxRow> = envelopes
        .into_iter()
        .filter_map(|envelope| match envelope.payload {
            EnvelopePayload::Peer(message) => Some(InboxRow {
                name: ctx.app.mesh.peer_name(&message),
                received: envelope.timestamp,
                message: *message,
            }),
            _ => None,
        })
        .collect();
    let awaiting_collect: Vec<String> = ctx
        .app
        .mesh
        .correlations()
        .list()
        .into_iter()
        .filter(|correlation| correlation.reply.is_some())
        .map(|correlation| correlation.record.id)
        .collect();
    out_text(&render_inbox(&rows, &awaiting_collect));
    if dropped > 0 {
        err_text(&dropped_warning(dropped));
    }
    Ok(())
}

fn pending(ctx: &RequestContext) -> Result<()> {
    live(ctx)?;
    let asked = ctx.app.mesh.correlations().list();
    let escalated = match ctx.app.mesh.inbound_store() {
        Some(store) => store.list(SystemTime::now())?,
        None => Vec::new(),
    };
    out_text(&render_pending(&asked, &escalated));
    Ok(())
}

async fn answer(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some((id, text)) = split_id_and_text(rest) else {
        out_text(&render_verb_help("answer"));
        return Ok(());
    };
    let runtime = live(ctx)?;
    let inbound = match ctx.app.mesh.inbound_store() {
        Some(store) => store.get(id)?,
        None => None,
    };
    let outbound = ctx.app.mesh.correlations().get(id);
    let route = answer_route(inbound.is_some(), outbound.is_some());
    match (route, inbound, outbound) {
        (AnswerRoute::Inbound, Some(record), _) => {
            if record.kind == InboundKind::Access {
                bail!(access_not_a_question(id));
            }
            out_text(&sending_notice(&record.peer_destination));
            ctx.app.mesh.answer_inbound(id, text).await?;
            out_text(&format!("Answered {}.", short(id)));
        }
        (AnswerRoute::Outbound, _, Some(correlation)) => {
            let destination = &correlation.record.peer_destination;
            out_text(&sending_notice(destination));
            let out = outbound_answer(&correlation, text)?;
            let outcome = runtime.send_peer(destination, &out).await?;
            out_text(&format!(
                "Sent {} to {} as a reply to {} (via {}).",
                short(&outcome.id),
                short(destination),
                short(id),
                via_text(outcome.via)
            ));
        }
        _ => bail!(
            "No open question has id {id}. `.mesh pending` lists the ones this node knows about."
        ),
    }
    Ok(())
}

/// The human's reply into one of this node's own open questions, in that question's
/// thread, worded as answered like a reply from `answer_inbound`.
fn outbound_answer(correlation: &Correlation, text: &str) -> Result<OutboundPeer> {
    let record = &correlation.record;
    Ok(
        OutboundPeer::new(PeerKind::Reply, text, None, Some(&record.id), None)?
            .with_thread(Some(record.thread.clone()))?
            .with_disposition(Disposition::Answered, None),
    )
}

async fn reply(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("reply"));
        return Ok(());
    };
    let (yes_first, rest) = take_flag(rest, "--yes");
    let Some((target, rest)) = rest.split_once(char::is_whitespace) else {
        out_text(&render_verb_help("reply"));
        return Ok(());
    };
    let Some(destination) = canonical_hash(target) else {
        bail!(
            "'{target}' is not a destination hash: expected 32 hex characters, as `.mesh peers` lists them."
        );
    };
    let Some(outgoing) = parse_outgoing(rest) else {
        out_text(&render_verb_help("reply"));
        return Ok(());
    };
    let runtime = live(ctx)?;
    let peer = peer_for_contact(&runtime, &destination, REPLY_REFUSAL_TAIL)?;
    let verdict = runtime.trust().authorize(&peer.identity_hash, &destination);
    out_text(&format!(
        "This sends your text to {} ({}, trust: {}) over the mesh.",
        name_label(peer.display_name.as_deref()),
        short(&destination),
        trust_label(verdict)
    ));
    if !confirm_or_flag("Send it?", "--yes", yes_first || outgoing.yes)? {
        out_text("Nothing was sent.");
        return Ok(());
    }
    let out = OutboundPeer::new(PeerKind::Message, outgoing.text, None, None, None)?;
    let outcome = runtime.send_peer(&destination, &out).await?;
    out_text(&format!(
        "Sent {} to {} (via {}).",
        short(&outcome.id),
        short(&destination),
        via_text(outcome.via)
    ));
    Ok(())
}

/// The pre-flight every verb that contacts one peer runs: a destination the trust list
/// denies is refused whether or not it has been heard; an unheard one is a not-heard
/// error; a heard one is refused on any verdict short of Allow, `only` being the verb's
/// own rule.
fn peer_for_contact(runtime: &MeshRuntime, destination: &str, only: &str) -> Result<PeerRecord> {
    let Some(peer) = runtime.peers().get(destination) else {
        return Err(unheard_refusal(runtime, destination, only));
    };
    let verdict = runtime.trust().authorize(&peer.identity_hash, destination);
    if let Some(refusal) = trust_refusal(destination, verdict, only) {
        bail!(refusal);
    }
    Ok(peer)
}

/// Why a destination not in the peer table is refused: the trust list's denial of the
/// destination itself when there is one, otherwise that it has not been heard.
fn unheard_refusal(runtime: &MeshRuntime, destination: &str, tail: &str) -> anyhow::Error {
    let verdict = runtime.trust().authorize("", destination);
    if verdict.rule == Rule::DestinationDenied
        && let Some(refusal) = trust_refusal(destination, verdict, tail)
    {
        return anyhow!(refusal);
    }
    anyhow!(
        "Destination {destination} has not been heard from: it is not in the peer table. Only peers this node has heard announce can be contacted; check `.mesh peers`."
    )
}

async fn broadcast(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(outgoing) = rest.and_then(parse_outgoing) else {
        out_text(&render_verb_help("broadcast"));
        return Ok(());
    };
    let runtime = live(ctx)?;
    out_text(BROADCAST_NOTICE);
    if !confirm_or_flag("Send the bulletin?", "--yes", outgoing.yes)? {
        out_text("Nothing was sent.");
        return Ok(());
    }
    let out = OutboundPeer::new(PeerKind::Bulletin, outgoing.text, None, None, None)?;
    let outcome = runtime.broadcast(&out).await?;
    out_text(&render_broadcast(&outcome));
    Ok(())
}

/// The automatic sync runs on the idle-time driver's interval; this runs one now. A
/// fetch already running, here or in another process of this identity, is reported and
/// left to finish rather than treated as a failure.
async fn sync(ctx: &RequestContext, abort_signal: &AbortSignal, rest: Option<&str>) -> Result<()> {
    parse_args(rest, &[], "sync")?;
    let runtime = live(ctx)?;
    out_text("Asking the nearest propagation node for held messages; Ctrl-C cancels...");
    let outcome = tokio::select! {
        outcome = runtime.fetch_propagated(&LoggingInboundSink) => outcome,
        _ = wait_user_interrupt(Some(abort_signal)) => {
            out_text("Sync interrupted.");
            return Ok(());
        }
    };
    match outcome {
        Ok(report) => out_text(&render_sync(&report)),
        Err(FetchError::AlreadyRunning) => {
            out_text("A sync is already running; wait for it to finish.")
        }
        Err(err @ FetchError::HeldByOtherProcess { .. }) => out_text(&err.to_string()),
        Err(err) => bail!(err.to_string()),
    }
    Ok(())
}

/// Unlike the other one-peer verbs, a knock is not gated on this node trusting the
/// destination: it is what a peer that has not trusted us is asked with. It is still
/// refused for a destination this node has denied or an identity it has blocked.
async fn knock(ctx: &RequestContext, abort_signal: &AbortSignal, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("knock"));
        return Ok(());
    };
    let args = parse_mutation_args(rest, "knock", &["--yes", "--intro"])?;
    let Some(target) = args.positional.first() else {
        out_text(&render_verb_help("knock"));
        return Ok(());
    };
    let destination = destination_hash(target)?;
    let runtime = live(ctx)?;
    let Some(peer) = runtime.peers().get(&destination) else {
        return Err(unheard_refusal(&runtime, &destination, KNOCK_REFUSAL_TAIL));
    };
    let verdict = runtime.trust().authorize(&peer.identity_hash, &destination);
    if matches!(
        verdict.rule,
        Rule::DestinationDenied | Rule::IdentityBlocked
    ) && let Some(refusal) = trust_refusal(&destination, verdict, KNOCK_REFUSAL_TAIL)
    {
        bail!(refusal);
    }
    let intro = KnockIntro::new(args.intro.as_deref().unwrap_or(""))?;
    let intro_label = if intro.as_str().is_empty() {
        "without an intro".to_string()
    } else {
        format!("with intro \"{}\"", intro.as_str())
    };
    out_text(&format!(
        "This knocks on {} ({}, trust: {}) so it can trust this instance, {intro_label}; if the peer is unreachable the knock is stored with a propagation node.",
        name_label(peer.display_name.as_deref()),
        short(&destination),
        trust_label(verdict)
    ));
    if !confirm_or_flag("Knock?", "--yes", args.yes)? {
        out_text("Nothing was sent.");
        return Ok(());
    }
    let Some(desc) = runtime.resolve_destination(&destination).await else {
        bail!(
            "Destination {destination} cannot be reached yet: its announce has not been heard since this node started. Wait for it to announce, or check `.mesh peers`."
        );
    };
    out_text(&format!(
        "Knocking on {}; Ctrl-C cancels...",
        short(&destination)
    ));
    let outcome = tokio::select! {
        outcome = runtime.knock(&desc, &intro) => outcome,
        _ = wait_user_interrupt(Some(abort_signal)) => {
            out_text("Knock interrupted.");
            return Ok(());
        }
    };
    match outcome {
        Ok(KnockOutcome {
            via: KnockVia::Direct,
        }) => out_text(&format!(
            "Knocked on {} directly; the peer decides whether to trust this instance.",
            short(&destination)
        )),
        Ok(KnockOutcome {
            via: KnockVia::StoreAndForward,
        }) => out_text(&format!(
            "Knocked on {} via store-and-forward; a propagation node holds the knock until the peer fetches it.",
            short(&destination)
        )),
        Err(err) => bail!(err.to_string()),
    }
    Ok(())
}

fn render_sync(report: &FetchReport) -> String {
    let node = short(&report.node);
    if report.listed > 0 && report.wanted == 0 {
        return format!(
            "Nothing new held for this node at {node}: {} listed, all already processed.",
            report.listed
        );
    }
    if report.wanted > 0 && report.received == 0 {
        let mut line = format!(
            "{node} lists {} for this node and {} were asked for, but none were served; run .mesh sync again or check the node's logs.",
            plural(report.listed, "message", "messages"),
            report.wanted
        );
        if report.wanted == MAX_WANTS_PER_FETCH {
            line.push_str(" More may be held.");
        }
        return line;
    }
    if report.received == 0 {
        return format!("Nothing held for this node at {node}.");
    }
    let mut line = format!(
        "Fetched from {node}: {} listed, {} wanted, {} received, {} delivered, {} duplicates, {} discarded, {} deferred.",
        report.listed,
        report.wanted,
        report.received,
        report.delivered,
        report.duplicates,
        report.discarded,
        report.deferred
    );
    if report.wanted == MAX_WANTS_PER_FETCH {
        line.push_str(" More may be held; run .mesh sync again.");
    }
    line
}

fn trust(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("trust"));
        return Ok(());
    };
    let args = parse_mutation_args(
        rest,
        "trust",
        &[
            "--yes",
            "--label",
            "--identity",
            "--prune",
            "--older-than",
            "--dry-run",
            "--confirm",
        ],
    )?;
    let Some(arg) = classify_trust(args)? else {
        out_text(&render_verb_help("trust"));
        return Ok(());
    };
    let store = trust_store(ctx)?;
    let mesh: &dyn LiveMesh = ctx.app.mesh.as_ref();
    let now = SystemTime::now();
    match arg {
        TrustArg::Destination { target, label, yes } => {
            let destination = destination_hash(&target)?;
            let refused = store.records().iter().any(|record| {
                record.tier == Tier::Destination && record.hash == destination && record.denied
            });
            let mut preview = format!(
                "This trusts instance {}: a peer you trust can message this node and ask it questions.",
                short(&destination)
            );
            if refused {
                preview.push_str(" This also lifts the refusal on it.");
            }
            out_text(&preview);
            if !confirm_or_flag(&format!("Trust {}?", short(&destination)), "--yes", yes)? {
                out_text(NOTHING_CHANGED);
                return Ok(());
            }
            let outcome = store.trust_destination(
                mesh,
                &destination,
                TrustOptions { label, note: None },
                now,
            )?;
            let mut lines = vec![format!(
                "Trusted {} (identity {}) - {}.",
                short(&outcome.destination_hash),
                short(&outcome.identity_hash),
                change_text(outcome.change)
            )];
            if outcome.deny_lifted {
                lines.push("The refusal on this instance is lifted.".to_string());
            }
            lines.extend(outcome.superseded.iter().map(|old| {
                format!(
                    "  This instance was trusted before as {old} under another identity; that record's key-change mark is cleared. The old record stays trusted for the old key until you run .mesh untrust {old}."
                )
            }));
            out_text(&lines.join("\n"));
        }
        TrustArg::Identity { target, label, yes } => {
            let identity = identity_hash(&target)?;
            let mut preview = format!(
                "This trusts identity {} and every instance it announces, now or later: each of them can message this node and ask it questions. A rotation of this identity is not detected; trust its instances with .mesh trust <destination> instead if you want a key-change notice.",
                short(&identity)
            );
            let refused = store
                .records()
                .iter()
                .filter(|record| {
                    record.tier == Tier::Destination
                        && record.denied
                        && record.identity.as_deref() == Some(&identity)
                })
                .count();
            if refused > 0 {
                preview.push_str(&format!(
                    " {refused} instance(s) of this identity stay refused; `.mesh trust <destination>` lifts each."
                ));
            }
            out_text(&preview);
            let question = format!("Trust every instance of {}?", short(&identity));
            if !confirm_or_flag(&question, "--yes", yes)? {
                out_text(NOTHING_CHANGED);
                return Ok(());
            }
            let change =
                store.trust_identity(mesh, &identity, TrustOptions { label, note: None }, now)?;
            out_text(&format!(
                "Trusted identity {}, all destinations - {}.",
                short(&identity),
                change_text(change)
            ));
        }
        TrustArg::Prune {
            older_than,
            confirm,
        } => {
            prune(ctx, &store, mesh, older_than, confirm, now)?;
        }
    }
    Ok(())
}

/// Without `--confirm` this is a dry run: it lists what would go and prints the token that
/// removes exactly that many. The token, not `--yes`, is the consent, and it is checked
/// against a fresh count so a list that moved since the dry run is never pruned blind.
fn prune(
    ctx: &RequestContext,
    store: &TrustStore,
    mesh: &dyn LiveMesh,
    older_than: Option<Duration>,
    confirm: Option<String>,
    now: SystemTime,
) -> Result<()> {
    let threshold = older_than.unwrap_or(PRUNE_DEFAULT_OLDER_THAN);
    let threshold_text = older_than_text(threshold);
    let expected = confirm.as_deref().map(prune_token_count).transpose()?;
    let stale = store.prune_destinations(mesh, threshold, now, true)?;
    if let Some(expected) = expected
        && expected != stale.len()
    {
        bail!(
            "The stale list has changed since the dry run (now {}, token names {expected}); run .mesh trust --prune again for a fresh token.",
            stale.len()
        );
    }
    if stale.is_empty() {
        out_text(&format!(
            "No trusted instance is older than {threshold_text}; nothing to prune."
        ));
        return Ok(());
    }
    if expected.is_none() {
        let records = store.records();
        let mut lines = vec![format!(
            "Trusted instances not heard from in the last {threshold_text}:"
        )];
        let mut key_changed = 0;
        for hash in &stale {
            let who = records
                .iter()
                .find(|record| record.hash == *hash)
                .map(|record| {
                    let mut who = format!(
                        "{}  last seen {}",
                        record_label(record),
                        age_text(now, record.last_seen_at)
                    );
                    if let Some(change) = &record.key_changed {
                        key_changed += 1;
                        who.push_str(&format!(
                            " key changed: announced under identity {} {}",
                            short(&change.seen_identity),
                            age_text(now, change.at)
                        ));
                    }
                    who
                })
                .unwrap_or_default();
            lines.push(format!("  {hash}  {who}"));
        }
        let older_than_flag = older_than
            .map(|_| format!(" --older-than {threshold_text}"))
            .unwrap_or_default();
        let marked = match key_changed {
            0 => String::new(),
            n => format!(", {n} of them marked key-changed"),
        };
        lines.push(format!(
            "{DRY_RUN_NOTHING_CHANGED} To remove these {} instance(s){marked}, run: .mesh trust --prune{older_than_flag} --confirm prune-{}",
            stale.len(),
            stale.len()
        ));
        out_text(&lines.join("\n"));
        return Ok(());
    }
    let removed = store.prune_destinations(mesh, threshold, now, false)?;
    let mut lines = vec![format!("Removed {} trusted instance(s):", removed.len())];
    lines.extend(removed.iter().map(|hash| format!("  {hash}")));
    out_text(&lines.join("\n"));
    match live(ctx)?.knock_gate().cache().prune(now) {
        Ok(0) => {}
        Ok(dropped) => out_text(&format!(
            "Dropped {dropped} expired knock(s) from the cache."
        )),
        Err(err) => err_text(&format!("The knock cache could not be pruned: {err:#}")),
    }
    Ok(())
}

/// `verb` is `untrust` or its alias `forget`, echoed in the usage, the prompt and the
/// confirm hint; the confirm token is `untrust-<short>` under both. The preview and the
/// write are two store calls, so a change between them shows in the final line.
fn untrust(ctx: &RequestContext, verb: &str, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help(verb));
        return Ok(());
    };
    let args = parse_mutation_args(
        rest,
        verb,
        &["--yes", "--identity", "--dry-run", "--confirm"],
    )?;
    let Some(arg) = classify_untrust(args, verb)? else {
        out_text(&render_verb_help(verb));
        return Ok(());
    };
    let store = trust_store(ctx)?;
    let mesh: &dyn LiveMesh = ctx.app.mesh.as_ref();
    let now = SystemTime::now();
    match arg {
        UntrustArg::Destination {
            target,
            yes,
            dry_run,
        } => {
            let destination = destination_hash(&target)?;
            let outcome = store.untrust_destination(mesh, &destination, now, true)?;
            let bookkeeping = match &outcome {
                UntrustOutcome::Forgotten { .. } => {
                    out_text(&format!(
                        "This forgets trusted instance {}; the record for its identity stays.",
                        short(&destination)
                    ));
                    false
                }
                UntrustOutcome::Refused {
                    identity,
                    already_refused: false,
                    ..
                } => {
                    out_text(&format!(
                        "{}'s identity stays trusted; this instance is refused until `.mesh trust {destination}`.",
                        identity_name(&store, mesh, identity, &destination)
                    ));
                    false
                }
                UntrustOutcome::Refused {
                    already_refused: true,
                    record_missing: false,
                    ..
                } => {
                    out_text(&format!(
                        "{} is already refused; its identity stays trusted.",
                        short(&destination)
                    ));
                    out_text(if dry_run {
                        DRY_RUN_NOTHING_CHANGED
                    } else {
                        NOTHING_CHANGED
                    });
                    return Ok(());
                }
                UntrustOutcome::Refused {
                    identity,
                    already_refused: true,
                    record_missing: true,
                } => {
                    out_text(&format!(
                        "{} is already refused; its record is bound to {} again.",
                        short(&destination),
                        identity_name(&store, mesh, identity, &destination)
                    ));
                    true
                }
            };
            if dry_run {
                out_text(DRY_RUN_NOTHING_CHANGED);
                return Ok(());
            }
            // Re-binding a record grants nothing and fires no hook, so it is not asked about.
            if !bookkeeping {
                let question = format!("{} {}?", title_case(verb), short(&destination));
                if !confirm_or_flag(&question, "--yes", yes)? {
                    out_text(NOTHING_CHANGED);
                    return Ok(());
                }
            }
            match store.untrust_destination(mesh, &destination, now, false)? {
                UntrustOutcome::Forgotten { .. } => {
                    out_text(&format!("Untrusted {}.", short(&destination)));
                }
                UntrustOutcome::Refused {
                    identity,
                    already_refused: true,
                    record_missing: true,
                } => out_text(&format!(
                    "Bound {}'s record to {}; it stays refused.",
                    short(&destination),
                    identity_name(&store, mesh, &identity, &destination)
                )),
                UntrustOutcome::Refused { .. } => out_text(&format!(
                    "Refused {}; `.mesh trust {destination}` lifts that.",
                    short(&destination)
                )),
            }
        }
        UntrustArg::Identity { target, confirm } => {
            let identity = identity_hash(&target)?;
            untrust_identity(&store, mesh, verb, &identity, confirm, now)?;
        }
    }
    Ok(())
}

/// The name an untrust sentence calls an identity: its record's label, else the
/// destination record's, else the peer table's display name, else the short hash.
fn identity_name(
    store: &TrustStore,
    mesh: &dyn LiveMesh,
    identity: &str,
    destination: &str,
) -> String {
    let records = store.records();
    let label_of = |tier: Tier, hash: &str| {
        records
            .iter()
            .find(|record| record.tier == tier && record.hash == hash)
            .and_then(|record| record.label.as_deref())
            .and_then(|label| display_text(label, DISPLAY_NAME_MAX_CHARS))
    };
    label_of(Tier::Identity, identity)
        .or_else(|| label_of(Tier::Destination, destination))
        .or_else(|| {
            mesh.peers()
                .and_then(|peers| peers.get(destination))
                .filter(|peer| peer.identity_hash == identity)
                .and_then(|peer| peer.display_name)
                .and_then(|name| display_text(&name, DISPLAY_NAME_MAX_CHARS))
        })
        .unwrap_or_else(|| short(identity).to_string())
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Like `prune`, a dry run until the printed token comes back; the listing is the store's
/// own records, since `untrust_identity` has no dry-run form.
fn untrust_identity(
    store: &TrustStore,
    mesh: &dyn LiveMesh,
    verb: &str,
    identity: &str,
    confirm: Option<String>,
    now: SystemTime,
) -> Result<()> {
    let records = store.records();
    let identity_record = records
        .iter()
        .find(|record| record.tier == Tier::Identity && record.hash == identity);
    let bound: Vec<&TrustRecord> = records
        .iter()
        .filter(|record| {
            record.tier == Tier::Destination && record.identity.as_deref() == Some(identity)
        })
        .collect();
    if identity_record.is_none() && bound.is_empty() {
        bail!("Identity {identity} is not in the trust list, so there is nothing to untrust.");
    }
    let expected = format!("untrust-{}", short(identity));
    match confirm {
        None => {
            let mut lines = vec![format!("Untrusting identity {identity} forgets:")];
            if let Some(record) = identity_record {
                let scope = if record.all_destinations {
                    "all destinations"
                } else {
                    "listed instances only"
                };
                lines.push(format!(
                    "  identity {}  {}  {scope}",
                    short(identity),
                    record_label(record)
                ));
            }
            for record in &bound {
                lines.push(format!(
                    "  instance {}  {}  last seen {}{}",
                    record.hash,
                    record_label(record),
                    age_text(now, record.last_seen_at),
                    if record.denied { "  refused" } else { "" }
                ));
            }
            let refused = bound.iter().filter(|record| record.denied).count();
            let refused_clause = if refused > 0 {
                format!(", {refused} of them refused (the refusal goes with the record)")
            } else {
                String::new()
            };
            lines.push(format!(
                "{DRY_RUN_NOTHING_CHANGED} To forget this identity and its {} instance(s){refused_clause}, run: .mesh {verb} --identity {identity} --confirm {expected}",
                bound.len()
            ));
            out_text(&lines.join("\n"));
        }
        Some(token) if token != expected => bail!(
            "'{token}' is not the token for identity {}; run .mesh {verb} --identity {identity} --confirm {expected}",
            short(identity)
        ),
        Some(_) => {
            let removed = store.untrust_identity(mesh, identity)?;
            let mut lines = vec![format!(
                "Untrusted identity {} and {} instance(s).",
                short(identity),
                removed.len()
            )];
            lines.extend(removed.iter().map(|hash| format!("  {hash}")));
            out_text(&lines.join("\n"));
        }
    }
    Ok(())
}

fn block(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("block"));
        return Ok(());
    };
    let args = parse_mutation_args(rest, "block", &["--yes", "--note"])?;
    let Some(target) = args.positional.first() else {
        out_text(&render_verb_help("block"));
        return Ok(());
    };
    let store = trust_store(ctx)?;
    let identity = identity_hash(target)?;
    out_text(&format!(
        "This blocks identity {}: its knocks are dropped without a word, and every trust record for it, identity and instances alike, is removed.",
        short(&identity)
    ));
    if !confirm_or_flag(&format!("Block {}?", short(&identity)), "--yes", args.yes)? {
        out_text(NOTHING_CHANGED);
        return Ok(());
    }
    let removed = store.block_identity(
        ctx.app.mesh.as_ref(),
        &identity,
        args.note,
        SystemTime::now(),
    )?;
    let mut lines = vec![format!("Blocked {}.", short(&identity))];
    if !removed.is_empty() {
        lines.push(format!(
            "Removed {} trust record(s) for its instance(s):",
            removed.len()
        ));
        lines.extend(removed.iter().map(|hash| format!("  {hash}")));
    }
    out_text(&lines.join("\n"));
    Ok(())
}

fn unblock(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("unblock"));
        return Ok(());
    };
    let args = parse_mutation_args(rest, "unblock", &["--yes"])?;
    let Some(target) = args.positional.first() else {
        out_text(&render_verb_help("unblock"));
        return Ok(());
    };
    let store = trust_store(ctx)?;
    let identity = identity_hash(target)?;
    out_text(&format!(
        "This lifts the block on identity {}, so it may knock again; trust is not restored.",
        short(&identity)
    ));
    if !confirm_or_flag(&format!("Unblock {}?", short(&identity)), "--yes", args.yes)? {
        out_text(NOTHING_CHANGED);
        return Ok(());
    }
    store.unblock_identity(ctx.app.mesh.as_ref(), &identity)?;
    out_text(&format!(
        "Unblocked {}. It is not trusted; `.mesh trust` grants that.",
        short(&identity)
    ));
    Ok(())
}

/// Like `untrust --identity`, a dry run until the printed token comes back. Bare
/// `.mesh rotate` is the dry run itself: there is no target to prompt help for. The
/// identity lock is taken here so a node in another process refuses the dry run as well,
/// after the key is named so a config dir without one is left untouched; `rotate_identity`
/// re-reads the key under its own lock, so the guard is released before the confirm.
fn rotate(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let args = parse_mutation_args(
        rest.unwrap_or(""),
        "rotate",
        &["--dry-run", "--confirm", "--yes"],
    )?;
    if args.yes {
        bail!(
            "`.mesh rotate` takes `--confirm rotate-<identity-short>` from a dry run, not `--yes`."
        );
    }
    if let Some(token) = args.positional.first() {
        return Err(unexpected(token, "rotate"));
    }
    if args.dry_run && args.confirm.is_some() {
        return Err(unexpected("--dry-run", "rotate"));
    }
    if ctx.app.mesh.get().is_some() {
        bail!(ROTATE_NEEDS_OFF);
    }
    let path = MeshPaths::from_env().identity_path;
    let old = fingerprint(&identity::current_identity(&path)?);
    let lock = identity::IdentityLock::exclusive(&path)?;
    let expected = format!("rotate-{}", short(&old));
    match args.confirm {
        None => {
            let recorded = identity::predecessors(&path)?.len();
            let lines = [
                format!("Rotating the mesh identity replaces {old}:"),
                "  a new identity is minted and written over mesh/identity.key; the old private key is not kept".to_string(),
                format!(
                    "  the old identity hash is appended to mesh/{} ({recorded} recorded so far)",
                    identity::PREDECESSORS_FILE
                ),
                "  the instance id is unchanged, so the node announces a new destination hash under the new identity".to_string(),
                "  rotation is refused while any session's node on this config dir is running: each running node holds mesh/identity.key.lock".to_string(),
                "  every peer that trusted this identity or its instances now sees a stranger and must run .mesh trust again after verifying the new hash out of band; your own trust list is unchanged".to_string(),
                format!(
                    "{DRY_RUN_NOTHING_CHANGED} To rotate, run: .mesh rotate --confirm {expected}"
                ),
            ];
            out_text(&lines.join("\n"));
        }
        Some(token) if token != expected => bail!(
            "'{token}' is not the token for identity {}; run .mesh rotate --confirm {expected}",
            short(&old)
        ),
        Some(_) => {
            drop(lock);
            let rotation = identity::rotate_identity(&path, &old, SystemTime::now())?;
            out_text(&format!(
                "Rotated the mesh identity: {} -> {}.\nPredecessors recorded: {}. Run .mesh on to announce the new identity; peers must re-trust it.",
                rotation.old_fingerprint, rotation.new_fingerprint, rotation.predecessors
            ));
        }
    }
    Ok(())
}

/// `.mesh allow <pattern>`: an allow rule for every trusted peer or for one. `--force`
/// adds the override that lifts the built-in deny for one exact file, which only the
/// global file may carry.
fn allow(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("allow"));
        return Ok(());
    };
    let args = parse_mutation_args(
        rest,
        "allow",
        &[
            "--peer",
            "--force",
            "--global",
            "--workspace",
            "--yes",
            "--dry-run",
        ],
    )?;
    let Some(args) = classify_share(args, "allow")? else {
        out_text(&render_verb_help("allow"));
        return Ok(());
    };
    let ShareArgs {
        pattern,
        scope,
        yes,
        dry_run,
        peer,
        force,
    } = args;
    if force && peer.is_some() {
        bail!(
            "`--force` lifts the built-in deny for every peer whose allow names `{pattern}`, so it cannot be scoped with `--peer`; drop `--peer` (every trusted peer with a matching allow will see the file)."
        );
    }
    if let Some(destination) = canonical_hash(&pattern) {
        bail!(
            "`.mesh allow` takes a file pattern, not a peer; to trust the instance {} run `.mesh trust {destination}`, or scope a share to it with `--peer {destination}`.",
            short(&destination)
        );
    }
    refuse_directory_pattern(&pattern, "share what is under it with")?;
    validate_pattern(&pattern)?;
    let peer = peer.as_deref().map(canonical_peer).transpose()?;
    if force {
        validate_override(&pattern)?;
    }
    let (root, mut set, case_insensitive) = writable_share_set(ctx)?;
    if let Some(head) = set.protected_head(&pattern) {
        bail!(
            "`{pattern}` is under `{head}/`, which is never shared, not even with `--force`; nothing was written."
        );
    }
    let target = ShareTarget::of(&set, scope);
    if force {
        if target.layer == Layer::Workspace {
            bail!(
                "Overrides are honoured from the global share file only, so `--force` cannot be written to {}; pass `--global`.",
                set.locations().workspace.display()
            );
        }
        match set.builtin_denies(&pattern, case_insensitive)? {
            Some(BuiltinHit::ByText) => {}
            Some(BuiltinHit::ByResolution { resolved }) => bail!(
                "`{pattern}` resolves to `{resolved}`, which the built-in deny names; `.mesh allow {resolved} --force --global` lifts that file."
            ),
            None => bail!(
                "`{pattern}` is not under the built-in deny, so there is nothing for `--force` to lift; drop `--force`."
            ),
        }
    } else if !pattern.contains(GLOB_METACHARACTERS)
        && set.builtin_denies(&pattern, case_insensitive)?.is_some()
    {
        bail!(
            "`{pattern}` is under the built-in deny, so an allow alone would share nothing; `.mesh allow {pattern} --force --global` lifts it for this one file."
        );
    }
    let matches = describe_matches(&set, &pattern, &root, case_insensitive)?;
    let audience = share_audience(peer.as_deref());
    let already = format!(
        "`{pattern}` is already allowed for {audience} in {}; nothing was changed.",
        target.path.display()
    );
    let held = layer_holds(
        &set,
        target.layer,
        &RawKind::Allow {
            pattern: pattern.clone(),
            peer: peer.clone(),
        },
    ) && (!force
        || layer_holds(
            &set,
            target.layer,
            &RawKind::Override {
                path: pattern.clone(),
            },
        ));
    if held {
        out_text(&already);
        return Ok(());
    }
    let lift = if force {
        ", and an override lifting the built-in deny for it"
    } else {
        ""
    };
    out_text(&format!(
        "{} write to {}: allow `{pattern}` for {audience}{lift}.",
        intent_verb(dry_run),
        target.written_to()
    ));
    if dry_run {
        out_text(DRY_RUN_NOTHING_CHANGED);
        return Ok(());
    }
    if matches.broad {
        let question = format!(
            "Share {} files matching `{pattern}` with {audience}?",
            matches.words
        );
        if !confirm_or_flag(&question, "--yes", yes)? {
            out_text(NOTHING_CHANGED);
            return Ok(());
        }
        set = loaded_for_writing(ctx)?.1;
    }
    let mut changed = set
        .apply(
            Mutation::Allow {
                pattern: pattern.clone(),
                peer,
            },
            target.write_scope(),
        )?
        .changed;
    if force {
        changed |= set
            .apply(
                Mutation::Override {
                    path: pattern.clone(),
                },
                target.write_scope(),
            )?
            .changed;
    }
    if !changed {
        out_text(&already);
        return Ok(());
    }
    out_text(&format!(
        "Allowed `{pattern}` for {audience}; written to {}.",
        target.written_to()
    ));
    if force {
        out_text(&format!(
            "The built-in deny for `{pattern}` is lifted by an override in the same file."
        ));
    }
    Ok(())
}

/// `.mesh deny <pattern>`: a deny rule, which no allow in either file gets past. A hash
/// in the pattern's place is the withdrawn peer verb's spelling and is sent to `untrust`.
fn deny(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("deny"));
        return Ok(());
    };
    let args = parse_mutation_args(
        rest,
        "deny",
        &["--global", "--workspace", "--yes", "--dry-run"],
    )?;
    let Some(args) = classify_share(args, "deny")? else {
        out_text(&render_verb_help("deny"));
        return Ok(());
    };
    let ShareArgs {
        pattern,
        scope,
        yes,
        dry_run,
        ..
    } = args;
    if let Some(destination) = canonical_hash(&pattern) {
        bail!(
            "`.mesh deny` takes a file pattern, not a peer; to refuse the instance {} run `.mesh untrust {destination}`.",
            short(&destination)
        );
    }
    refuse_directory_pattern(&pattern, "keep what is under it back with")?;
    validate_pattern(&pattern)?;
    let (root, mut set, case_insensitive) = writable_share_set(ctx)?;
    if let Some(head) = set.protected_head(&pattern) {
        bail!(
            "`{pattern}` is under `{head}/`, which is never shared, so no deny is needed; nothing was written."
        );
    }
    let target = ShareTarget::of(&set, scope);
    let matches = describe_matches(&set, &pattern, &root, case_insensitive)?;
    let already = format!(
        "`{pattern}` is already denied to every peer in {}; nothing was changed.",
        target.path.display()
    );
    if layer_holds(
        &set,
        target.layer,
        &RawKind::Deny {
            pattern: pattern.clone(),
        },
    ) {
        out_text(&already);
        return Ok(());
    }
    out_text(&format!(
        "{} write to {}: deny `{pattern}` to every peer.",
        intent_verb(dry_run),
        target.written_to()
    ));
    if dry_run {
        out_text(DRY_RUN_NOTHING_CHANGED);
        return Ok(());
    }
    if matches.broad {
        let question = format!(
            "Deny {} files matching `{pattern}` to every peer?",
            matches.words
        );
        if !confirm_or_flag(&question, "--yes", yes)? {
            out_text(NOTHING_CHANGED);
            return Ok(());
        }
        set = loaded_for_writing(ctx)?.1;
    }
    let applied = set.apply(
        Mutation::Deny {
            pattern: pattern.clone(),
        },
        target.write_scope(),
    )?;
    if applied.changed {
        out_text(&format!(
            "Denied `{pattern}` to every peer; written to {}.",
            target.written_to()
        ));
    } else {
        out_text(&already);
    }
    Ok(())
}

/// `.mesh unshare <pattern>`: removes every rule whose text is `pattern`, allow, deny or
/// override alike, from the file that holds it. Under the write rule a pattern held
/// only by the other file is refused with the flag that reaches it; when both files hold
/// it, both are named and the removal confirmed before either is edited.
fn unshare(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let Some(rest) = rest else {
        out_text(&render_verb_help("unshare"));
        return Ok(());
    };
    let args = parse_mutation_args(
        rest,
        "unshare",
        &["--global", "--workspace", "--yes", "--dry-run"],
    )?;
    let Some(args) = classify_share(args, "unshare")? else {
        out_text(&render_verb_help("unshare"));
        return Ok(());
    };
    let ShareArgs {
        pattern,
        scope,
        yes,
        dry_run,
        ..
    } = args;
    refuse_directory_pattern(&pattern, "name what is under it with")?;
    validate_pattern(&pattern)?;
    let (root, mut set, case_insensitive) = writable_share_set(ctx)?;
    let locations = set.locations().clone();
    let path_of = |layer: Layer| match layer {
        Layer::Global => locations.global.display(),
        Layer::Workspace => locations.workspace.display(),
    };
    let flag_of = |layer: Layer| match layer {
        Layer::Global => "--global",
        Layer::Workspace => "--workspace",
    };
    let holders = share_holders(&set, &pattern);
    let mut targets: Vec<(Layer, Vec<&'static str>)> = [Layer::Global, Layer::Workspace]
        .into_iter()
        .filter_map(|layer| {
            let kinds: Vec<&'static str> = holders
                .iter()
                .filter(|(held, _)| *held == layer)
                .map(|(_, kind)| *kind)
                .collect();
            (!kinds.is_empty()).then_some((layer, kinds))
        })
        .collect();
    let Some((first_layer, first_kinds)) = targets.first().cloned() else {
        bail!(
            "No share rule has the pattern `{pattern}` in {} or {}; `.mesh shares` lists them.",
            path_of(Layer::Global),
            path_of(Layer::Workspace)
        );
    };
    match scope {
        WriteScope::Global | WriteScope::Workspace => {
            let picked = match scope {
                WriteScope::Workspace => Layer::Workspace,
                _ => Layer::Global,
            };
            if !targets.iter().any(|(held, _)| *held == picked) {
                bail!(
                    "No share rule in {} has the pattern `{pattern}`; {} holds it — pass `{}` instead.",
                    path_of(picked),
                    path_of(first_layer),
                    flag_of(first_layer)
                );
            }
            targets.retain(|(held, _)| *held == picked);
        }
        WriteScope::Auto => {
            let reach = set.write_target(WriteScope::Auto);
            if !targets.iter().any(|(held, _)| *held == reach) {
                bail!(
                    "`{pattern}` is held by {} ({}); the write rule reaches {} — pass `{}` to remove it from {}.",
                    path_of(first_layer),
                    kind_list(&first_kinds),
                    path_of(reach),
                    flag_of(first_layer),
                    path_of(first_layer)
                );
            }
        }
    }
    let total: usize = targets.iter().map(|(_, kinds)| kinds.len()).sum();
    let matches = describe_matches(&set, &pattern, &root, case_insensitive)?;
    if total > 1 {
        out_text(&format!("`{pattern}` is held by:"));
        for (layer, kinds) in &targets {
            for kind in kinds {
                out_text(&format!("  {kind} in {}", path_of(*layer)));
            }
        }
    }
    for (layer, kinds) in &targets {
        out_text(&format!(
            "{} remove the {} for `{pattern}` from {}.",
            intent_verb(dry_run),
            kind_list(kinds),
            path_of(*layer)
        ));
    }
    if dry_run {
        out_text(DRY_RUN_NOTHING_CHANGED);
        return Ok(());
    }
    if total > 1 || matches.broad {
        let question = match targets.as_slice() {
            [(layer, kinds)] if total == 1 => format!(
                "Remove the {} for `{pattern}` from {}?",
                kind_list(kinds),
                path_of(*layer)
            ),
            _ => format!("Remove all {total} rules for `{pattern}`?"),
        };
        if !confirm_or_flag(&question, "--yes", yes)? {
            out_text(NOTHING_CHANGED);
            return Ok(());
        }
        set = loaded_for_writing(ctx)?.1;
        if share_holders(&set, &pattern) != holders {
            bail!("The share list changed while the prompt was open; run `.mesh unshare` again.");
        }
    }
    for (layer, kinds) in &targets {
        let scope = match layer {
            Layer::Global => WriteScope::Global,
            Layer::Workspace => WriteScope::Workspace,
        };
        set.apply(
            Mutation::Unshare {
                pattern: pattern.clone(),
            },
            scope,
        )?;
        out_text(&format!(
            "Removed the {} for `{pattern}` from {}.",
            kind_list(kinds),
            path_of(*layer)
        ));
    }
    Ok(())
}

/// `.mesh shares`: the rules as the two files hold them, or with `--effective` the files
/// they resolve to under the root with the verdict a fetch of each would get. Inspection
/// only, so it works while the mesh is off and never prints a file's contents.
fn shares(ctx: &RequestContext, rest: Option<&str>) -> Result<()> {
    let args = parse_mutation_args(rest.unwrap_or(""), "shares", &["--peer", "--effective"])?;
    if let Some(token) = args.positional.first() {
        return Err(unexpected(token, "shares"));
    }
    let peer = args.peer.as_deref().map(canonical_peer).transpose()?;
    let (root, set) = share_set(ctx)?;
    if let Some(refusal) = set.refusal() {
        err_text(&shown_refusal(refusal));
        out_text("Nothing is shared until it is fixed.");
        return Ok(());
    }
    let peer_ref = match peer.as_deref() {
        Some(hash) => PeerRef {
            identity: hash,
            destination: hash,
        },
        None => PeerRef::unscoped(),
    };
    let text = if args.effective {
        let case = root_case(ctx, &root)?;
        render_effective_shares(
            &set,
            &peer_ref,
            &share_audience(peer.as_deref()),
            &root,
            case,
        )
    } else {
        render_share_rules(&set, &peer_ref, peer.as_deref())
    };
    out_text(&text);
    Ok(())
}

fn render_share_rules(set: &ShareSet, peer: &PeerRef<'_>, scoped_to: Option<&str>) -> String {
    let locations = set.locations();
    let effective = scoped_to.is_some().then(|| set.effective(peer));
    let rows: Vec<String> = set
        .entries()
        .into_iter()
        .filter_map(|entry| {
            let layer = match entry.layer {
                Layer::Global => "global",
                Layer::Workspace => "workspace",
            };
            let (kind, pattern, note) = match entry.kind {
                RawKind::Allow { pattern, peer } => {
                    if let Some(effective) = &effective
                        && !effective.iter().any(|applied| {
                            applied.pattern == pattern
                                && applied.layer == entry.layer
                                && applied.peer == peer
                        })
                    {
                        return None;
                    }
                    let note = match &peer {
                        None => "every trusted peer".to_string(),
                        Some(hash) if is_canonical_peer(hash) => format!("peer {}", short(hash)),
                        Some(text) => format!(
                            "scoped to no peer (`{}` is not a peer hash)",
                            shown_pattern(text)
                        ),
                    };
                    ("allow", pattern, note)
                }
                RawKind::Deny { pattern } => ("deny", pattern, String::new()),
                RawKind::Override { path } => {
                    let note = match entry.layer {
                        Layer::Global => "lifts the built-in deny for this file",
                        Layer::Workspace => {
                            "ignored: overrides are honoured from the global file only"
                        }
                    };
                    ("override", path, note.to_string())
                }
            };
            Some(
                format!(
                    "  {kind:<9} {:<40} {layer:<10} {note}",
                    shown_pattern(&pattern)
                )
                .trim_end()
                .to_string(),
            )
        })
        .collect();
    if rows.is_empty() {
        if let Some(hash) = scoped_to {
            return format!(
                "No share rule applies to peer {}; `.mesh shares` lists every rule.",
                short(hash)
            );
        }
        return format!(
            "No share rules in {} or {}. `.mesh allow docs/**` shares docs/ with every trusted peer.",
            locations.global.display(),
            locations.workspace.display()
        );
    }
    let presence = |path: &Path| if path.exists() { "" } else { " (absent)" };
    let for_peer = scoped_to
        .map(|hash| format!(" for peer {}", short(hash)))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "Share rules (global {}{}; workspace {}{}){for_peer}:",
        locations.global.display(),
        presence(&locations.global),
        locations.workspace.display(),
        presence(&locations.workspace)
    )];
    lines.extend(rows);
    lines.join("\n")
}

fn render_effective_shares(
    set: &ShareSet,
    peer: &PeerRef<'_>,
    audience: &str,
    root: &Path,
    case: Option<bool>,
) -> String {
    let case_insensitive = case.unwrap_or(false);
    let resolved = set.resolve(
        peer,
        case_insensitive,
        DEFAULT_LIST_WALK_BOUND,
        LIST_PAGE_SIZE,
    );
    let mut lines = Vec::new();
    if case.is_none() {
        lines.push(
            "The share root's case folding could not be read; patterns were matched case-sensitively."
                .to_string(),
        );
    }
    if resolved.entries.is_empty() {
        lines.push(format!(
            "Nothing resolves: no allow rule names an existing file under {}.",
            root.display()
        ));
        return lines.join("\n");
    }
    lines.push(format!(
        "Files {audience} can fetch from {}:",
        root.display()
    ));
    for entry in &resolved.entries {
        let mark = match entry.verdict {
            ShareVerdict::Shared => "",
            ShareVerdict::BuiltinDenied => "  (built-in deny)",
            ShareVerdict::Denied => "  (denied)",
            ShareVerdict::Protected => "  (protected)",
            ShareVerdict::NotAllowed => "  (not allowed)",
        };
        lines.push(format!("  {}{mark}", entry.path));
    }
    if resolved.capped {
        lines.push(format!(
            "… listing capped at {LIST_PAGE_SIZE} entries; narrow the patterns or use mesh__list."
        ));
    }
    if resolved.truncated {
        lines.push("The walk stopped at its bound, so files may be missing.".to_string());
    }
    lines.join("\n")
}

/// A pattern as a share file holds it, shown the way peer text is: escapes stripped.
fn shown_pattern(pattern: &str) -> String {
    display_text(pattern, WIRE_PATH_MAX_BYTES).unwrap_or_default()
}

/// A load refusal as the human sees it: it quotes the file's own text, so escapes are
/// stripped the way a pattern's are.
fn shown_refusal(refusal: &str) -> String {
    display_text(refusal, 4096).unwrap_or_default()
}

struct ShareArgs {
    pattern: String,
    scope: WriteScope,
    yes: bool,
    dry_run: bool,
    peer: Option<String>,
    force: bool,
}

/// `None` when the pattern is missing, which the verb answers with its usage.
fn classify_share(args: MutationArgs, verb: &str) -> Result<Option<ShareArgs>> {
    if args.dry_run && args.yes {
        return Err(unexpected("--yes", verb));
    }
    let scope = match (args.global, args.workspace) {
        (true, true) => bail!(
            "`--global` and `--workspace` name different files; pass one. {}",
            render_verb_help(verb)
        ),
        (true, false) => WriteScope::Global,
        (false, true) => WriteScope::Workspace,
        (false, false) => WriteScope::Auto,
    };
    Ok(args.positional.into_iter().next().map(|pattern| ShareArgs {
        pattern,
        scope,
        yes: args.yes,
        dry_run: args.dry_run,
        peer: args.peer,
        force: args.force,
    }))
}

/// The completer offers a directory as `docs/`, which no share rule can hold, so the
/// human hears what to type rather than that the pattern has an empty segment. Slashes
/// alone name the filesystem root, which `validate_pattern` has the sentence for.
fn refuse_directory_pattern(pattern: &str, does: &str) -> Result<()> {
    if pattern.ends_with('/') && !pattern.starts_with('/') {
        bail!("`{pattern}` names a directory; {does} `{pattern}**`, or one file by its path.");
    }
    Ok(())
}

/// The share root and where its two files live, with the configured inbox protected
/// whether or not a node is running; `None` before a snapshot names the root.
fn share_locations(ctx: &RequestContext) -> Option<(PathBuf, ShareLocations)> {
    ctx.app
        .mesh
        .share_locations(ctx.app.config.mesh.fetch.inbox_dir.as_deref())
}

/// The share root and the two share files, loaded; the verbs' one way in. Each verb
/// reads `refusal` itself, so the load's warning is not repeated here.
fn share_set(ctx: &RequestContext) -> Result<(PathBuf, ShareSet)> {
    let Some((root, locations)) = share_locations(ctx) else {
        bail!(SHARE_ROOT_UNKNOWN);
    };
    let (set, _warning) = ShareSet::load_quietly(locations);
    Ok((root, set))
}

/// `share_set` for a mutation: the mesh gate first, so an unattended verb never prompts
/// about a node that is not there, then a refused file stops everything.
fn writable_share_set(ctx: &RequestContext) -> Result<(PathBuf, ShareSet, bool)> {
    live(ctx)?;
    let (root, set) = loaded_for_writing(ctx)?;
    let case_insensitive = root_case(ctx, &root)?.unwrap_or(false);
    Ok((root, set, case_insensitive))
}

/// `share_set` with a refused file stopping everything. A verb that asked a question
/// reads the files again through this before it writes, since the copy it showed may be
/// stale by the time the human answers; otherwise its write would drop whatever changed
/// while the prompt was open.
fn loaded_for_writing(ctx: &RequestContext) -> Result<(PathBuf, ShareSet)> {
    let (root, set) = share_set(ctx)?;
    if let Some(refusal) = set.refusal() {
        bail!("{} Nothing was written.", shown_refusal(refusal));
    }
    Ok((root, set))
}

/// Whether the share root folds case, as the rules must be judged: the node's memoised
/// probe while the mesh is on, which is an error when the probe failed, since the node
/// then serves nothing from the root and a verb that guessed would describe shares that
/// do not exist. While the mesh is off, the read-only hint, since the probe writes a
/// temp file into the root and nothing may touch the tree before the user consents to
/// serving it; `Ok(None)` when the hint cannot tell, for the caller to say so.
fn root_case(ctx: &RequestContext, root: &Path) -> Result<Option<bool>> {
    match ctx.app.mesh.get() {
        Some(runtime) => runtime
            .serving()
            .case_insensitive_for(root)
            .map(Some)
            .ok_or_else(|| {
                anyhow!(
                    "The share root {} could not be probed, so this node serves nothing from it; fix the directory's permissions and run `.mesh off` then `.mesh on`.",
                    root.display()
                )
            }),
        None => Ok(case_folding_hint(root)),
    }
}

/// Where a mutation lands, taken before it is applied so `created` is truthful.
struct ShareTarget {
    layer: Layer,
    path: PathBuf,
    created: bool,
}

impl ShareTarget {
    fn of(set: &ShareSet, scope: WriteScope) -> Self {
        let layer = set.write_target(scope);
        let path = match layer {
            Layer::Global => set.locations().global.clone(),
            Layer::Workspace => set.locations().workspace.clone(),
        };
        Self {
            layer,
            path,
            created: layer == Layer::Workspace && !set.workspace_exists(),
        }
    }

    /// The announced layer as a scope `apply` cannot re-resolve, so a workspace file
    /// that appears while the prompt stands does not move the write.
    fn write_scope(&self) -> WriteScope {
        match self.layer {
            Layer::Global => WriteScope::Global,
            Layer::Workspace => WriteScope::Workspace,
        }
    }

    fn written_to(&self) -> String {
        let created = if self.created { " (created)" } else { "" };
        format!("{}{created}", self.path.display())
    }
}

struct Breadth {
    words: String,
    broad: bool,
}

/// Prints how many files `pattern` reaches under the root and says whether that is broad
/// enough to confirm: `**` at the head, more than `BROAD_MATCH_LIMIT` files, or a walk
/// that stopped before it could tell.
fn describe_matches(
    set: &ShareSet,
    pattern: &str,
    root: &Path,
    case_insensitive: bool,
) -> Result<Breadth> {
    let count = set.count_matches(
        pattern,
        case_insensitive,
        DEFAULT_LIST_WALK_BOUND,
        BROAD_MATCH_LIMIT + 1,
    )?;
    let words = breadth_words(&count);
    out_text(&format!(
        "`{pattern}` matches {words} file(s) under {}.",
        root.display()
    ));
    Ok(Breadth {
        broad: is_broad_pattern(pattern) || count.files > BROAD_MATCH_LIMIT || count.truncated,
        words,
    })
}

/// A walk that stopped at its bound before reaching the cap has seen a floor, not a
/// count, and must not claim the limit was passed.
fn breadth_words(count: &MatchCount) -> String {
    if count.capped {
        format!("more than {BROAD_MATCH_LIMIT}")
    } else if count.truncated {
        format!("at least {}", count.files)
    } else {
        count.files.to_string()
    }
}

/// `(layer, kind)` for every entry whose text is exactly `pattern`, global first, in
/// file order.
fn share_holders(set: &ShareSet, pattern: &str) -> Vec<(Layer, &'static str)> {
    set.entries()
        .into_iter()
        .filter_map(|entry| {
            let kind = match &entry.kind {
                RawKind::Allow { pattern: held, .. } if held == pattern => "allow",
                RawKind::Deny { pattern: held } if held == pattern => "deny",
                RawKind::Override { path } if path == pattern => "override",
                _ => return None,
            };
            Some((entry.layer, kind))
        })
        .collect()
}

/// Whether `layer` already holds exactly `kind`, so a verb can say nothing needs writing
/// before it announces a write.
fn layer_holds(set: &ShareSet, layer: Layer, kind: &RawKind) -> bool {
    set.entries()
        .iter()
        .any(|entry| entry.layer == layer && entry.kind == *kind)
}

/// `allow`, `allow and override`, `allow, deny and override`; a kind held twice is
/// named once.
fn kind_list(kinds: &[&str]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for kind in kinds {
        if !names.contains(kind) {
            names.push(kind);
        }
    }
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_string(),
        Some((last, head)) => format!("{} and {last}", head.join(", ")),
    }
}

fn intent_verb(dry_run: bool) -> &'static str {
    if dry_run { "Would" } else { "Will" }
}

fn share_audience(peer: Option<&str>) -> String {
    match peer {
        None => "every trusted peer".to_string(),
        Some(peer) => format!("peer {}", short(peer)),
    }
}

/// The store behind every trust mutation, refused with the store's own teaching text while
/// the mesh is off; the store checks again through `LiveMesh` on the call itself.
fn trust_store(ctx: &RequestContext) -> Result<Arc<TrustStore>> {
    ctx.app
        .mesh
        .get()
        .map(|runtime| runtime.trust())
        .ok_or_else(|| anyhow!(crate::mesh::trust::MESH_OFF))
}

fn destination_hash(token: &str) -> Result<String> {
    canonical_hash(token).ok_or_else(|| {
        anyhow!(
            "'{token}' is not a destination hash: expected 32 hex characters, as `.mesh peers` lists them."
        )
    })
}

fn identity_hash(token: &str) -> Result<String> {
    canonical_hash(token).ok_or_else(|| {
        anyhow!(
            "'{token}' is not an identity hash: expected 32 hex characters, as `.mesh knocks` and `.mesh peers` list them."
        )
    })
}

fn change_text(change: TrustChange) -> &'static str {
    match change {
        TrustChange::Added => "added",
        TrustChange::Updated => "updated",
    }
}

/// A trust record as one word: its label, else the short identity it is bound to.
fn record_label(record: &TrustRecord) -> String {
    record
        .label
        .as_deref()
        .and_then(|label| display_text(label, DISPLAY_NAME_MAX_CHARS))
        .or_else(|| record.identity.as_deref().map(|id| short(id).to_string()))
        .unwrap_or_else(|| "-".to_string())
}

#[derive(Debug)]
enum TrustArg {
    Destination {
        target: String,
        label: Option<String>,
        yes: bool,
    },
    Identity {
        target: String,
        label: Option<String>,
        yes: bool,
    },
    Prune {
        older_than: Option<Duration>,
        confirm: Option<String>,
    },
}

/// `None` when no target was named: the caller prints the usage.
fn classify_trust(args: MutationArgs) -> Result<Option<TrustArg>> {
    if args.prune {
        if args.yes {
            bail!("`.mesh trust --prune` takes `--confirm prune-<N>` from a dry run, not `--yes`.");
        }
        if let Some(target) = args.positional.first() {
            return Err(unexpected(target, "trust"));
        }
        if args.identity.is_some() {
            return Err(unexpected("--identity", "trust"));
        }
        if args.label.is_some() {
            return Err(unexpected("--label", "trust"));
        }
        if args.dry_run && args.confirm.is_some() {
            return Err(unexpected("--dry-run", "trust"));
        }
        return Ok(Some(TrustArg::Prune {
            older_than: args.older_than,
            confirm: args.confirm,
        }));
    }
    for (given, flag) in [
        (args.older_than.is_some(), "--older-than"),
        (args.dry_run, "--dry-run"),
        (args.confirm.is_some(), "--confirm"),
    ] {
        if given {
            return Err(unexpected(flag, "trust"));
        }
    }
    if let Some(identity) = args.identity {
        if let Some(target) = args.positional.first() {
            return Err(unexpected(target, "trust"));
        }
        return Ok(Some(TrustArg::Identity {
            target: identity,
            label: args.label,
            yes: args.yes,
        }));
    }
    Ok(args
        .positional
        .into_iter()
        .next()
        .map(|target| TrustArg::Destination {
            target,
            label: args.label,
            yes: args.yes,
        }))
}

#[derive(Debug)]
enum UntrustArg {
    Destination {
        target: String,
        yes: bool,
        dry_run: bool,
    },
    Identity {
        target: String,
        confirm: Option<String>,
    },
}

fn classify_untrust(args: MutationArgs, verb: &str) -> Result<Option<UntrustArg>> {
    if let Some(identity) = args.identity {
        if args.yes {
            bail!(
                "`.mesh {verb} --identity` takes `--confirm untrust-<identity-short>` from a dry run, not `--yes`."
            );
        }
        if let Some(target) = args.positional.first() {
            return Err(unexpected(target, verb));
        }
        if args.dry_run && args.confirm.is_some() {
            return Err(unexpected("--dry-run", verb));
        }
        return Ok(Some(UntrustArg::Identity {
            target: identity,
            confirm: args.confirm,
        }));
    }
    if args.dry_run && args.yes {
        return Err(unexpected("--yes", verb));
    }
    if args.confirm.is_some() {
        return Err(unexpected("--confirm", verb));
    }
    Ok(args
        .positional
        .into_iter()
        .next()
        .map(|target| UntrustArg::Destination {
            target,
            yes: args.yes,
            dry_run: args.dry_run,
        }))
}

fn unexpected(token: &str, verb: &str) -> anyhow::Error {
    anyhow!("Unexpected '{token}'. {}", render_verb_help(verb))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MutationArgs {
    positional: Vec<String>,
    yes: bool,
    dry_run: bool,
    prune: bool,
    identity: Option<String>,
    label: Option<String>,
    note: Option<String>,
    intro: Option<String>,
    older_than: Option<Duration>,
    confirm: Option<String>,
    peer: Option<String>,
    force: bool,
    global: bool,
    workspace: bool,
    effective: bool,
}

/// The trust verbs take one hash and valued flags, so every token is parsed and a flag may
/// sit anywhere; `reply` differs, honouring `--yes` as the leading word only, because the
/// rest of its line is free text.
fn parse_mutation_args(rest: &str, verb: &str, allowed: &[&str]) -> Result<MutationArgs> {
    let mut args = MutationArgs::default();
    let mut tokens = split_tokens(rest).into_iter();
    while let Some(token) = tokens.next() {
        if !token.starts_with("--") {
            if !args.positional.is_empty() {
                return Err(unexpected(&token, verb));
            }
            args.positional.push(token);
            continue;
        }
        if !allowed.contains(&token.as_str()) {
            return Err(unexpected(&token, verb));
        }
        let mut value = || match tokens.next() {
            Some(value) if !value.starts_with("--") => Ok(value),
            _ => Err(anyhow!(
                "'{token}' needs a value. {}",
                render_verb_help(verb)
            )),
        };
        match token.as_str() {
            "--yes" => args.yes = true,
            "--dry-run" => args.dry_run = true,
            "--prune" => args.prune = true,
            "--identity" => args.identity = Some(value()?),
            "--label" => args.label = Some(value()?),
            "--note" => args.note = Some(value()?),
            "--intro" => args.intro = Some(value()?),
            "--older-than" => args.older_than = Some(parse_older_than(&value()?)?),
            "--confirm" => args.confirm = Some(value()?),
            "--peer" => args.peer = Some(value()?),
            "--force" => args.force = true,
            "--global" => args.global = true,
            "--workspace" => args.workspace = true,
            "--effective" => args.effective = true,
            _ => return Err(unexpected(&token, verb)),
        }
    }
    Ok(args)
}

/// Whitespace-separated tokens, a double-quoted run being one token with its quotes
/// stripped; an unclosed quote runs to the end of the line.
fn split_tokens(rest: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut open = false;
    for c in rest.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            open = true;
        } else if c.is_whitespace() && !in_quotes {
            if open {
                tokens.push(std::mem::take(&mut current));
                open = false;
            }
        } else {
            current.push(c);
            open = true;
        }
    }
    if open {
        tokens.push(current);
    }
    tokens
}

/// `<N>d`, `<N>h` or `<N>m` (minutes), N a whole number above zero.
fn parse_older_than(text: &str) -> Result<Duration> {
    let teaching = || {
        anyhow!(
            "'{text}' is not a duration: use a whole number of days, hours or minutes, such as 30d, 12h or 90m."
        )
    };
    let unit = text.chars().last().ok_or_else(teaching)?;
    let secs_per_unit = match unit {
        'd' => 86_400,
        'h' => 3_600,
        'm' => 60,
        _ => return Err(teaching()),
    };
    let count: u64 = text[..text.len() - unit.len_utf8()]
        .parse()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(teaching)?;
    count
        .checked_mul(secs_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(teaching)
}

/// `parse_older_than` in reverse, in the coarsest unit that divides it.
fn older_than_text(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}m", secs / 60)
    }
}

fn prune_token_count(token: &str) -> Result<usize> {
    token
        .strip_prefix("prune-")
        .and_then(|count| count.parse().ok())
        .ok_or_else(|| {
            anyhow!(
                "'{token}' is not a prune token: run .mesh trust --prune for a dry run and pass the prune-<N> it prints."
            )
        })
}

fn live(ctx: &RequestContext) -> Result<Arc<MeshRuntime>> {
    match ctx.app.mesh.get() {
        Some(runtime) => Ok(runtime),
        None => bail!(MESH_OFF),
    }
}

fn split_verb(args: Option<&str>) -> Option<(&str, Option<&str>)> {
    let args = args?.trim();
    if args.is_empty() {
        return None;
    }
    let mut parts = args.splitn(2, char::is_whitespace);
    let verb = parts.next()?;
    let rest = parts.next().map(str::trim).filter(|rest| !rest.is_empty());
    Some((verb, rest))
}

#[derive(Debug)]
struct Flags<'a> {
    flags: Vec<&'a str>,
}

impl Flags<'_> {
    fn has(&self, flag: &str) -> bool {
        self.flags.contains(&flag)
    }
}

/// Every token must be one of `allowed`; anything else is answered with the verb's usage.
fn parse_args<'a>(rest: Option<&'a str>, allowed: &[&str], verb: &str) -> Result<Flags<'a>> {
    let mut flags = Vec::new();
    for token in rest.unwrap_or("").split_whitespace() {
        if !allowed.contains(&token) {
            bail!("Unexpected '{token}'. {}", render_verb_help(verb));
        }
        flags.push(token);
    }
    Ok(Flags { flags })
}

enum StatusArg<'a> {
    Own,
    Set(&'a str),
    Clear,
    Fetch(String),
    Help,
}

fn classify_status(rest: Option<&str>) -> StatusArg<'_> {
    let Some(rest) = rest else {
        return StatusArg::Own;
    };
    if rest == "clear" {
        return StatusArg::Clear;
    }
    if let Some(objective) = quoted(rest) {
        return match objective.trim() {
            "" => StatusArg::Clear,
            text => StatusArg::Set(text),
        };
    }
    match canonical_hash(rest) {
        Some(destination) => StatusArg::Fetch(destination),
        None => StatusArg::Help,
    }
}

enum BriefArg<'a> {
    Show,
    Set(&'a str),
    Clear,
    Mode(MeshBrief),
    Help,
}

fn classify_brief(rest: Option<&str>) -> BriefArg<'_> {
    let Some(rest) = rest else {
        return BriefArg::Show;
    };
    match rest {
        "auto" => return BriefArg::Mode(MeshBrief::Auto),
        "manual" => return BriefArg::Mode(MeshBrief::Manual),
        "off" => return BriefArg::Mode(MeshBrief::Off),
        "clear" => return BriefArg::Clear,
        _ => {}
    }
    let Some(text) = rest.strip_prefix("set") else {
        return BriefArg::Help;
    };
    if !text.is_empty() && !text.starts_with(char::is_whitespace) {
        return BriefArg::Help;
    }
    let text = text.trim();
    let text = quoted(text).unwrap_or(text).trim();
    if text.is_empty() {
        BriefArg::Help
    } else {
        BriefArg::Set(text)
    }
}

/// The text between a leading and a trailing double quote, when both are present.
fn quoted(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner)
}

/// The words to send, unquoted and trimmed; `None` when nothing is left.
fn message_text(text: &str) -> Option<&str> {
    let text = text.trim();
    let text = quoted(text).unwrap_or(text).trim();
    (!text.is_empty()).then_some(text)
}

/// `flag` counts only as the first word of `text`; anywhere else, a trailing one
/// included, it is text to send.
fn take_flag<'a>(text: &'a str, flag: &str) -> (bool, &'a str) {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix(flag)
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return (true, rest.trim());
    }
    (false, text)
}

struct Outgoing<'a> {
    yes: bool,
    text: &'a str,
}

fn parse_outgoing(rest: &str) -> Option<Outgoing<'_>> {
    let (yes, rest) = take_flag(rest, "--yes");
    let text = message_text(rest)?;
    Some(Outgoing { yes, text })
}

/// `.mesh answer`'s id and the answer after it; `None` when either is missing.
fn split_id_and_text(rest: Option<&str>) -> Option<(&str, &str)> {
    let (id, text) = rest?.split_once(char::is_whitespace)?;
    Some((id, message_text(text)?))
}

#[derive(Debug, PartialEq, Eq)]
enum AnswerRoute {
    /// A peer's escalated question: the answer goes back to the peer as its reply.
    Inbound,
    /// A question this node asked: the text goes to the peer as a follow-up.
    Outbound,
    Unknown,
}

/// An id held by both stores answers the peer: their question is the one waiting on a human.
fn answer_route(inbound: bool, outbound: bool) -> AnswerRoute {
    match (inbound, outbound) {
        (true, _) => AnswerRoute::Inbound,
        (false, true) => AnswerRoute::Outbound,
        (false, false) => AnswerRoute::Unknown,
    }
}

fn via_text(via: PeerVia) -> &'static str {
    match via {
        PeerVia::Direct => "direct",
        PeerVia::StoreAndForward => "store-and-forward",
    }
}

fn sending_notice(destination: &str) -> String {
    format!(
        "Sending your answer to {} over the mesh...",
        short(destination)
    )
}

fn dropped_warning(dropped: usize) -> String {
    format!("WARNING: {dropped} peer message(s) were dropped because the inbox was full.")
}

/// Asks the human when nothing already said yes: `flag_given` skips the prompt, no
/// terminal on stdin is an error naming the flag rather than a hang, and any prompt
/// failure (Ctrl-C, EOF, a broken terminal) counts as no.
pub(crate) fn confirm_or_flag(question: &str, flag: &str, flag_given: bool) -> Result<bool> {
    #[cfg(test)]
    if let Some(forced_tty) = prompt_script::forced_tty() {
        return confirm_or_flag_with(question, flag, flag_given, forced_tty, |question| {
            Some(prompt_script::next_answer(question))
        });
    }
    confirm_or_flag_with(
        question,
        flag,
        flag_given,
        std::io::stdin().is_terminal(),
        |question| {
            drain_stale_tty_input();
            Confirm::new(question).with_default(false).prompt().ok()
        },
    )
}

pub(crate) fn confirm_or_flag_with(
    question: &str,
    flag: &str,
    flag_given: bool,
    stdin_is_tty: bool,
    prompt: impl FnOnce(&str) -> Option<bool>,
) -> Result<bool> {
    if flag_given {
        return Ok(true);
    }
    if !stdin_is_tty {
        bail!(
            "{question} Standard input is not a terminal, so there is no prompt to answer; pass {flag} to confirm."
        );
    }
    Ok(prompt(question).unwrap_or(false))
}

pub(crate) fn colour_allowed(stream_is_tty: bool, no_color: bool) -> bool {
    stream_is_tty && !no_color
}

/// The `NO_COLOR` opt-out is the shared `utils` parse, so `.mesh` output honours the
/// variable exactly as the rest of the binary does (one parse path).
fn err_text_coloured() -> bool {
    colour_allowed(
        std::io::stderr().is_terminal(),
        crate::utils::no_color_env_set(),
    )
}

pub(crate) fn out_text(text: &str) {
    #[cfg(test)]
    capture::push(capture::Stream::Out, text);
    println!("{text}");
}

pub(crate) fn err_text(text: &str) {
    #[cfg(test)]
    capture::push(capture::Stream::Err, text);
    if err_text_coloured() {
        eprintln!("{}", nu_ansi_term::Color::Yellow.paint(text));
    } else {
        eprintln!("{text}");
    }
}

#[cfg(test)]
pub(crate) mod prompt_script {
    //! Scripted stand-in for the `.mesh` confirmations. `inquire::Confirm` cannot run
    //! under the test harness, so while a guard is installed `confirm_or_flag` takes its
    //! terminal state from the guard and answers each prompt from a queue, or from a
    //! closure that runs while the question stands, counting every prompt asked. The
    //! script is process-global: tests using it must be `#[serial]`.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const INACTIVE: usize = 0;
    const TTY: usize = 1;
    const NON_TTY: usize = 2;

    type Answerer = Box<dyn Fn(&str) -> bool + Send + Sync>;

    static STATE: AtomicUsize = AtomicUsize::new(INACTIVE);
    static ASKED: AtomicUsize = AtomicUsize::new(0);
    static ANSWERS: Mutex<Vec<bool>> = Mutex::new(Vec::new());
    static ANSWERER: Mutex<Option<Answerer>> = Mutex::new(None);

    /// Forces a terminal on stdin and answers the prompts from `answers`, front to back;
    /// a prompt beyond the scripted answers panics.
    #[must_use]
    pub fn install(answers: &[bool]) -> ScriptGuard {
        install_with_state(TTY, answers)
    }

    /// Forces a terminal on stdin and puts every question to `answer`, which runs
    /// between the verb's prompt and its write, so a test can change the files it is
    /// about to write while the question stands. Its callers start a node, which the
    /// tests only do on unix.
    #[cfg(unix)]
    #[must_use]
    pub fn install_answering(answer: impl Fn(&str) -> bool + Send + Sync + 'static) -> ScriptGuard {
        *ANSWERER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(answer));
        install_with_state(TTY, &[])
    }

    /// Forces stdin to be no terminal, so the flag-naming refusal is pinned wherever the
    /// tests run.
    #[must_use]
    pub fn install_non_interactive() -> ScriptGuard {
        install_with_state(NON_TTY, &[])
    }

    fn install_with_state(state: usize, answers: &[bool]) -> ScriptGuard {
        *ANSWERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = answers.to_vec();
        ASKED.store(0, Ordering::SeqCst);
        STATE.store(state, Ordering::SeqCst);
        ScriptGuard
    }

    pub struct ScriptGuard;

    impl Drop for ScriptGuard {
        fn drop(&mut self) {
            STATE.store(INACTIVE, Ordering::SeqCst);
            ANSWERS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            ANSWERER
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        }
    }

    /// Number of prompts asked since the script was installed.
    pub fn prompts_asked() -> usize {
        ASKED.load(Ordering::SeqCst)
    }

    pub(super) fn forced_tty() -> Option<bool> {
        match STATE.load(Ordering::SeqCst) {
            TTY => Some(true),
            NON_TTY => Some(false),
            _ => None,
        }
    }

    pub(super) fn next_answer(question: &str) -> bool {
        ASKED.fetch_add(1, Ordering::SeqCst);
        if let Some(answer) = ANSWERER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            return answer(question);
        }
        let mut answers = ANSWERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            !answers.is_empty(),
            "mesh prompt asked with no scripted answer left: {question}"
        );
        answers.remove(0)
    }
}

#[cfg(test)]
pub(crate) mod capture {
    //! Records what `out_text` and `err_text` print while a guard is installed, in order,
    //! so a test can assert what the human saw and in which sequence. Process-global like
    //! `prompt_script`: tests using it must be `#[serial]`.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Stream {
        Out,
        Err,
    }

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static LINES: Mutex<Vec<(Stream, String)>> = Mutex::new(Vec::new());

    #[must_use]
    pub fn install() -> CaptureGuard {
        LINES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        ACTIVE.store(true, Ordering::SeqCst);
        CaptureGuard
    }

    pub struct CaptureGuard;

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            ACTIVE.store(false, Ordering::SeqCst);
            LINES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
    }

    /// Everything printed since the guard was installed, oldest first.
    pub fn lines() -> Vec<(Stream, String)> {
        LINES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn push(stream: Stream, text: &str) {
        if !ACTIVE.load(Ordering::SeqCst) {
            return;
        }
        LINES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((stream, text.to_string()));
    }
}

fn render_help() -> String {
    let mut lines = vec![
        "Mesh commands (nothing here writes config.yaml; trust, rotate and share rules persist under the mesh/ directory, share rules also in the workspace file):".to_string(),
    ];
    for (verb, description, example) in VERBS {
        lines.push(format!("  .mesh {verb:<10} {description}"));
        lines.push(format!("  {:<16} e.g. {example}", ""));
    }
    lines.join("\n")
}

fn render_verb_help(verb: &str) -> String {
    match VERBS.iter().find(|(name, _, _)| *name == verb) {
        Some((_, description, example)) => format!("Usage: {example}\n  {description}"),
        None => render_help(),
    }
}

fn is_public(config: &MeshConfig) -> bool {
    config
        .interfaces
        .iter()
        .any(|interface| matches!(interface, MeshInterface::Public { .. }))
}

/// How far this node is visible, from the configured interfaces alone.
fn reach_line(config: &MeshConfig) -> String {
    let public = is_public(config);
    let private = config
        .interfaces
        .iter()
        .any(|interface| matches!(interface, MeshInterface::Private { .. }));
    match (public, private) {
        (true, true) => {
            "world-visible via a community node (public interface), plus the configured relay and the peers it reaches".to_string()
        }
        (true, false) => "world-visible via a community node (public interface)".to_string(),
        (false, true) => "the configured relay and the peers it reaches".to_string(),
        (false, false) => "this link-local segment only".to_string(),
    }
}

/// Why a verb declines to reach a peer that is in the table but not trusted, `only`
/// being the verb's own rule; `None` for a trusted one.
fn trust_refusal(destination: &str, verdict: Verdict, only: &str) -> Option<String> {
    if verdict.decision == Decision::Allow {
        return None;
    }
    Some(format!(
        "{} is {} in this node's trust list; `.mesh peers` shows the standing. {only}",
        short(destination),
        trust_label(verdict)
    ))
}

fn audience(interface: &MeshInterface) -> &'static str {
    match interface {
        MeshInterface::Lan => "every node on this LAN",
        MeshInterface::Private { .. } => "the nodes joined to that relay",
        MeshInterface::Public { .. } => "anyone on the internet who reaches that relay",
    }
}

/// How many paths the share files already hand every trusted peer, for the `.mesh on`
/// preview: the peer-less entries only, after the built-in and user denies, so a
/// peer-scoped allow is not counted and neither is a file a deny holds back. Counted up
/// to `LIST_PAGE_SIZE`, with the read-only case hint in place of the probe, since nothing
/// may touch the tree before the user consents to serving it. `None` before a snapshot
/// names the share root.
fn shared_count(ctx: &RequestContext) -> Option<MatchCount> {
    let (root, locations) = share_locations(ctx)?;
    let (set, _warning) = ShareSet::load_quietly(locations);
    let count = set.count_shared(
        &PeerRef::unscoped(),
        case_folding_hint(&root).unwrap_or(false),
        DEFAULT_LIST_WALK_BOUND,
        LIST_PAGE_SIZE + 1,
    );
    // The walk caps at `cap` files seen, so one past the page tells a full page from more.
    Some(MatchCount {
        files: count.files.min(LIST_PAGE_SIZE),
        capped: count.files > LIST_PAGE_SIZE,
        truncated: count.truncated,
    })
}

/// What `.mesh on` prints before anything leaves the machine.
fn render_on_preview(
    config: &MeshConfig,
    session_name: &str,
    fresh: bool,
    shared: Option<MatchCount>,
) -> String {
    let mut lines = vec![
        format!(
            "Joining the mesh for session '{session_name}' (this session only; config.yaml is not changed)."
        ),
        "What leaves this machine:".to_string(),
    ];
    lines.push(if config.announce {
        "  announce: this node's destination hash and protocol version, repeated while the mesh is on".to_string()
    } else {
        "  announce: nothing until you contact a peer (announce: false)".to_string()
    });
    lines.push(match (config.announce, config.propagation_sync_interval_secs) {
        (false, _) => "  propagation sync: off (announce: false); .mesh sync runs one".to_string(),
        (true, 0) => {
            "  propagation sync: off (propagation_sync_interval_secs: 0); .mesh sync runs one"
                .to_string()
        }
        (true, interval) => format!(
            "  propagation sync: this node identifies itself to the nearest propagation node heard, at join and every {interval} s; .mesh sync runs one now"
        ),
    });
    lines.push(match &config.display_name {
        Some(name) if config.display_name_on_public => {
            format!("  display name: '{name}', on every interface including public ones")
        }
        Some(name) => format!(
            "  display name: '{name}' on lan and private interfaces; withheld on public ones"
        ),
        None => "  display name: none".to_string(),
    });
    lines.push(
        "  status card and brief: this session's objective, state, repo and todo, served to trusted peers on request".to_string(),
    );
    if let Some(shared) = shared.filter(|shared| shared.files > 0) {
        let n = match shared {
            MatchCount { capped: true, .. } => format!("{LIST_PAGE_SIZE} or more"),
            MatchCount {
                truncated: true,
                files,
                ..
            } => format!("at least {files}"),
            MatchCount { files, .. } => files.to_string(),
        };
        lines.push(format!(
            "  files: {n} path(s) are shared with trusted peers (`.mesh shares`)"
        ));
    }
    if fresh {
        lines.push(
            "  fresh id: this session gets a new mesh id and destination; peers that trusted the old destination must trust the new one".to_string(),
        );
    }
    lines.push("To whom:".to_string());
    for interface in &config.interfaces {
        lines.push(format!("  {interface}: {}", audience(interface)));
    }
    lines.push("Turn it off again with `.mesh off`.".to_string());
    lines.join("\n")
}

fn on_question(config: &MeshConfig) -> String {
    if is_public(config) {
        let relays: Vec<String> = config
            .interfaces
            .iter()
            .filter(|interface| matches!(interface, MeshInterface::Public { .. }))
            .map(ToString::to_string)
            .collect();
        format!(
            "This node will be WORLD-VISIBLE through {}: anyone reaching that relay can see its announce and knock on it. Join anyway?",
            relays.join(", ")
        )
    } else {
        format!(
            "Join the mesh on {} interface(s), visible to the nodes there?",
            config.interfaces.len()
        )
    }
}

/// The envoy's read-only tools are confined to the working directory, so joining from
/// `$HOME` or a filesystem root leaves the name deny-list as the only barrier to dotfiles.
/// Home is compared by canonical path when both resolve, so a symlinked home still warns.
fn cwd_warning(cwd: &Path, home: Option<&Path>) -> Option<String> {
    let is_home = home.is_some_and(|home| {
        home == cwd
            || matches!(
                (fs::canonicalize(cwd), fs::canonicalize(home)),
                (Ok(cwd), Ok(home)) if cwd == home
            )
    });
    if is_home || cwd.parent().is_none() {
        Some(format!(
            "Warning: this session's working directory is {}. A peer's question is answered by the envoy with read-only tools confined to it, so every file under it that the name deny-list does not cover is one question away from a peer. Start Coyote from a project directory before joining the mesh.",
            cwd.display()
        ))
    } else {
        None
    }
}

fn render_on_summary(runtime: &MeshRuntime, fresh: bool) -> String {
    let suffix = if fresh { " (fresh id)" } else { "" };
    format!(
        "Mesh is on for this session{suffix}.\n  identity: {}\n  destination: {}\n  instance: {}\n  interfaces: {}\nTurn it off with `.mesh off`.",
        runtime.fingerprint(),
        runtime.current_destination_hash(),
        runtime.current_instance_id(),
        runtime.interfaces().join(", ")
    )
}

fn name_label(name: Option<&str>) -> String {
    name.and_then(|name| display_text(name, DISPLAY_NAME_MAX_CHARS))
        .unwrap_or_else(|| "(no name)".to_string())
}

/// One `.mesh peers` line: a node heard on the mesh with its trust label and the key-change
/// mark on its trust record; a denied destination nothing has announced from yet, listed
/// so the deny (and any mark it carries) is visible; or a marked record whose destination
/// has aged out of the peer table, listed so the mark outlives the row it was first shown on.
enum PeerRow {
    Heard(PeerRecord, &'static str, Option<KeyChangeMark>),
    DenyOnly(TrustRecord, Option<KeyChangeMark>),
    MarkedOnly(TrustRecord, &'static str, KeyChangeMark),
}

/// A trust record's key-change mark with, when the peer table has heard it, the destination
/// the same instance announces under the new identity, so the marker can name what to trust.
struct KeyChangeMark {
    change: KeyChange,
    new_destination: Option<String>,
}

/// The rows for trust records no heard peer announces from, at most one per record: a
/// denied destination, with its mark when it carries one, else a marked record labelled by
/// `label_of`. `successors` pairs a superseded trusted destination with the peer now
/// announcing the same instance under another identity.
fn unheard_rows(
    records: Vec<TrustRecord>,
    heard: &[String],
    successors: &[(String, &PeerRecord)],
    label_of: impl Fn(&TrustRecord) -> &'static str,
) -> Vec<PeerRow> {
    records
        .into_iter()
        .filter(|record| !heard.contains(&record.hash))
        .filter_map(|record| {
            let mark = record.key_changed.clone().map(|change| {
                let new_destination = successors
                    .iter()
                    .find(|(superseded, peer)| {
                        *superseded == record.hash && peer.identity_hash == change.seen_identity
                    })
                    .map(|(_, peer)| peer.destination_hash.clone());
                KeyChangeMark {
                    change,
                    new_destination,
                }
            });
            if record.denied && record.tier == Tier::Destination {
                return Some(PeerRow::DenyOnly(record, mark));
            }
            let mark = mark?;
            let label = label_of(&record);
            Some(PeerRow::MarkedOnly(record, label, mark))
        })
        .collect()
}

fn render_peers(rows: &[PeerRow], now: SystemTime) -> String {
    if rows.is_empty() {
        return "No peers heard yet. Nodes appear here as their announces arrive.".to_string();
    }
    let mut lines = vec![format!(
        "{:<20} {:<10} {:<10} {:<10} {:>4}  {}",
        "name", "dest", "identity", "trust", "hops", "last seen"
    )];
    for row in rows {
        match row {
            PeerRow::Heard(peer, trust, key_changed) => {
                let stale = if peer.is_stale(now) { " (stale)" } else { "" };
                lines.push(format!(
                    "{:<20} {:<10} {:<10} {:<10} {:>4}  {}{stale}",
                    name_label(peer.display_name.as_deref()),
                    short(&peer.destination_hash),
                    short(&peer.identity_hash),
                    trust,
                    peer.hops,
                    age_text(now, peer.last_seen),
                ));
                if let Some(line) = peer.compatibility_line() {
                    lines.push(format!("{:<20} {line}", ""));
                }
                if let Some(mark) = key_changed {
                    lines.push(key_change_line(
                        mark,
                        &peer.identity_hash,
                        &peer.destination_hash,
                        now,
                    ));
                }
            }
            PeerRow::DenyOnly(record, key_changed) => {
                let bound = record.identity.as_deref().unwrap_or("-");
                lines.push(format!(
                    "{:<20} {:<10} {:<10} {:<10} {:>4}  {}",
                    "-",
                    short(&record.hash),
                    short(bound),
                    "denied",
                    "-",
                    "never"
                ));
                if let Some(mark) = key_changed {
                    lines.push(key_change_line(mark, bound, &record.hash, now));
                }
            }
            PeerRow::MarkedOnly(record, trust, mark) => {
                let bound = record.identity.as_deref().unwrap_or("-");
                let label = record
                    .label
                    .as_deref()
                    .and_then(|label| display_text(label, DISPLAY_NAME_MAX_CHARS))
                    .unwrap_or_else(|| "(not heard)".to_string());
                lines.push(format!(
                    "{:<20} {:<10} {:<10} {:<10} {:>4}  {}",
                    label,
                    short(&record.hash),
                    short(bound),
                    trust,
                    "-",
                    age_text(now, record.last_seen_at),
                ));
                lines.push(key_change_line(mark, bound, &record.hash, now));
            }
        }
    }
    lines.push(format!(
        "{} peer(s). Names are peer-supplied text. Full hashes: `.mesh info <dest>`.",
        rows.len()
    ));
    lines.join("\n")
}

/// The indented marker under a row whose trust record was marked `key_changed`, ending with
/// the same exits as the notification: trust the instance under its new key if the peer
/// rotated, otherwise block the identity that announced it; and forget the old key.
fn key_change_line(
    mark: &KeyChangeMark,
    bound_identity: &str,
    old_destination: &str,
    now: SystemTime,
) -> String {
    let trust = match &mark.new_destination {
        Some(destination) => format!(".mesh trust {destination}"),
        None => ".mesh trust its new destination once heard".to_string(),
    };
    format!(
        "{:<20} key changed: announced under identity {} {}; the grant stays with {}. If the peer rotated, verify out of band, then {trust}; otherwise .mesh block {}; .mesh untrust {old_destination} forgets the old key",
        "",
        short(&mark.change.seen_identity),
        age_text(now, mark.change.at),
        short(bound_identity),
        mark.change.seen_identity,
    )
}

fn render_knocks(records: &[KnockRecord], now: SystemTime) -> String {
    if records.is_empty() {
        return "No knocks. An untrusted node that wants in appears here.".to_string();
    }
    let mut lines = vec![format!(
        "{:<20} {:<10} {:<10} {:<9} {}",
        "name", "identity", "dest", "age", "intro"
    )];
    for knock in records {
        let age = parse_rfc3339(&knock.received_at)
            .map(|then| age_text(now, then))
            .unwrap_or_else(|| "unknown".to_string());
        lines.push(format!(
            "{:<20} {:<10} {:<10} {:<9} {}",
            name_label(knock.display_name.as_deref()),
            short(&knock.identity_hash),
            short(&knock.destination_hash),
            age,
            peer_line(knock.intro.as_deref(), INBOX_CONTENT_MAX_CHARS)
                .unwrap_or_else(|| "(no intro)".to_string()),
        ));
    }
    lines.push(format!(
        "{} knock(s). Names and intros are peer-supplied text.",
        records.len()
    ));
    lines.join("\n")
}

fn render_node_facts(
    runtime: &MeshRuntime,
    predecessors: Result<&[Predecessor], &anyhow::Error>,
    now: SystemTime,
) -> String {
    let mut output = String::new();
    let mut row = |name: &str, value: String| {
        output.push_str(&format!("  {name:<MESH_INFO_LABEL_WIDTH$}{value}\n"))
    };
    row("node", "on".to_string());
    row("identity", runtime.fingerprint().to_string());
    row(
        "identity predecessors",
        predecessors_text(predecessors, now),
    );
    row("destination", runtime.current_destination_hash());
    row("instance", runtime.current_instance_id());
    row("joined", runtime.interfaces().join(", "));
    let key_changes = runtime
        .trust()
        .records()
        .iter()
        .filter(|record| record.key_changed.is_some())
        .count();
    row(
        "key changes",
        match key_changes {
            0 => "none".to_string(),
            n => {
                format!("{n} trusted instance(s) announced under another identity; see .mesh peers")
            }
        },
    );
    output.push_str(&render_propagation_nodes(
        runtime.propagation_nodes().snapshot(),
        now,
    ));
    output
}

fn predecessors_text(
    predecessors: Result<&[Predecessor], &anyhow::Error>,
    now: SystemTime,
) -> String {
    match predecessors {
        Err(err) => format!("unreadable: {err:#}"),
        Ok([]) => "none".to_string(),
        Ok(all) => {
            let latest = &all[all.len() - 1];
            let when = parse_rfc3339(&latest.rotated_at)
                .map(|then| age_text(now, then))
                .or_else(|| display_text(&latest.rotated_at, DISPLAY_NAME_MAX_CHARS))
                .unwrap_or_else(|| "-".to_string());
            format!(
                "{} (latest {} rotated {when})",
                all.len(),
                short(&latest.identity_hash)
            )
        }
    }
}

/// Nearest first, the most recently heard breaking ties, so the row the node would
/// pick for store-and-forward is the top one.
fn render_propagation_nodes(mut nodes: Vec<PropagationNodeRecord>, now: SystemTime) -> String {
    nodes.sort_by_key(|node| (node.hops, std::cmp::Reverse(node.last_seen)));
    let mut output = String::new();
    if nodes.is_empty() {
        output.push_str(&format!(
            "  {:<MESH_INFO_LABEL_WIDTH$}none heard yet\n",
            "propagation_nodes"
        ));
    }
    for (i, record) in nodes.iter().enumerate() {
        let name = format!("propagation_nodes[{i}]");
        output.push_str(&format!(
            "  {name:<MESH_INFO_LABEL_WIDTH$}{} ({} hop(s), {})\n",
            record.node.destination.address_hash.to_hex_string(),
            record.hops,
            age_text(now, record.last_seen),
        ));
    }
    output
        .push_str("  selection: nearest by hops (an operator-pinned node is not supported yet)\n");
    output
}

/// What `.mesh info` found in the knock cache for a destination.
enum KnockLookup {
    Knocked(KnockRecord),
    NotKnocked,
    Unreadable,
}

/// Peer-supplied text as one printable line: line breaks and tabs flattened to spaces,
/// then cleaned and capped like every other peer string. Blank text is `None`.
fn peer_line(text: Option<&str>, max_chars: usize) -> Option<String> {
    let flat: String = text?
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    display_text(&flat, max_chars)
}

fn render_peer_detail(
    destination: &str,
    peer: Option<&PeerRecord>,
    knock: &KnockLookup,
    trust: Option<&str>,
    now: SystemTime,
) -> String {
    let mut lines = vec![format!("destination: {destination}")];
    if let Some(peer) = peer {
        lines.push(format!("identity: {}", peer.identity_hash));
        lines.push(format!(
            "name: {}",
            name_label(peer.display_name.as_deref())
        ));
        lines.push(format!("trust: {}", trust.unwrap_or("unknown")));
        lines.push(format!("protocol: {}", peer.protocol_version));
        if let Some(line) = peer.compatibility_line() {
            lines.push(line);
        }
        lines.push(format!("hops: {}", peer.hops));
        lines.push(format!(
            "last seen: {}{}",
            age_text(now, peer.last_seen),
            if peer.is_stale(now) { " (stale)" } else { "" }
        ));
    }
    match knock {
        KnockLookup::Knocked(knock) => {
            if peer.is_none() {
                lines.push(format!("identity: {}", knock.identity_hash));
                lines.push(format!(
                    "name: {}",
                    name_label(knock.display_name.as_deref())
                ));
                lines.push("trust: untrusted (known only from its knock)".to_string());
            }
            let age = parse_rfc3339(&knock.received_at)
                .map(|then| age_text(now, then))
                .unwrap_or_else(|| "unknown".to_string());
            lines.push(format!(
                "knocked: {age}, intro: {}",
                peer_line(knock.intro.as_deref(), INBOX_CONTENT_MAX_CHARS)
                    .unwrap_or_else(|| "(none)".to_string())
            ));
        }
        KnockLookup::NotKnocked => lines.push("knocked: no".to_string()),
        KnockLookup::Unreadable => {
            lines.push("knocked: unknown (knock cache unreadable)".to_string())
        }
    }
    lines.push("Names and intros are peer-supplied text.".to_string());
    lines.join("\n")
}

/// One `.mesh inbox` line: a peer message with its sender's name and when it landed here.
struct InboxRow {
    name: String,
    received: DateTime<Utc>,
    message: PeerMessage,
}

/// The inbox drain is destructive, so each message's content is rendered in full: it
/// was capped at `PEER_CONTENT_MAX_CHARS` on decode and is flattened to one line here.
fn render_inbox(rows: &[InboxRow], awaiting_collect: &[String]) -> String {
    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push("Inbox is empty.".to_string());
    }
    for row in rows {
        let message = &row.message;
        let title = peer_line(message.title.as_deref(), INBOX_CONTENT_MAX_CHARS)
            .map(|title| format!("{title}: "))
            .unwrap_or_default();
        let content = peer_line(Some(&message.content), PEER_CONTENT_MAX_CHARS).unwrap_or_default();
        let reply = message
            .in_reply_to
            .as_deref()
            .map(|id| format!(" (reply to {})", short(id)))
            .unwrap_or_default();
        lines.push(format!(
            "[{}] from {} ({}) via {} at {}: {title}{content}{reply}",
            message.kind,
            row.name,
            short(&message.source_destination),
            via_text(message.via),
            row.received.format("%H:%M UTC"),
        ));
        for part in &message.parts {
            match part {
                Part::Text { text } => {
                    let text = peer_line(Some(text), PEER_CONTENT_MAX_CHARS).unwrap_or_default();
                    lines.push(format!("  text: {text}"));
                }
                Part::Data { data } => {
                    lines.push(format!("  data: {} bytes", data.to_string().len()));
                }
                Part::File {
                    name,
                    size,
                    staged,
                    reference,
                    ..
                } => {
                    let location = match (staged, reference) {
                        (Some(path), _) => format!("staged at {}", path.display()),
                        (None, Some(path)) => format!("fetchable as {path}"),
                        (None, None) => "not kept".to_string(),
                    };
                    lines.push(format!("  file: {name} ({size} B) {location}"));
                }
            }
        }
        if message.dropped_parts > 0 {
            lines.push(format!(
                "  ({} dropped)",
                plural(message.dropped_parts as usize, "part", "parts")
            ));
        }
    }
    if !awaiting_collect.is_empty() {
        let ids: Vec<&str> = awaiting_collect.iter().map(|id| short(id)).collect();
        lines.push(format!("answered awaiting collect: {}", ids.join(", ")));
    }
    lines.join("\n")
}

fn render_pending(asked: &[Correlation], escalated: &[InboundRecord]) -> String {
    let mut lines = vec!["Questions this node asked (awaiting a reply):".to_string()];
    if asked.is_empty() {
        lines.push("  none".to_string());
    }
    for correlation in asked {
        let record = &correlation.record;
        let state = match record.state {
            PendingState::Open => "open",
            PendingState::Escalated => "escalated: the peer's human has been asked",
            PendingState::Answered => "answered, awaiting collect",
        };
        lines.push(format!(
            "  {}  {}  {state}  sent {}  timeout {}  {}",
            record.id,
            short(&record.peer_destination),
            record.sent_at,
            record.timeout_at,
            peer_line(Some(&record.question), INBOX_CONTENT_MAX_CHARS).unwrap_or_default()
        ));
    }
    lines.push("Questions peers asked (escalated to you):".to_string());
    let (access, questions): (Vec<&InboundRecord>, Vec<&InboundRecord>) = escalated
        .iter()
        .partition(|record| record.kind == InboundKind::Access);
    if questions.is_empty() {
        lines.push("  none".to_string());
    }
    for record in questions {
        lines.push(format!(
            "  {}  {}  received {}  {}",
            record.id,
            short(&record.peer_destination),
            record.received_at,
            peer_line(Some(&record.question), INBOX_CONTENT_MAX_CHARS).unwrap_or_default()
        ));
        if let Some(question) = peer_line(Some(&record.envoy_question), INBOX_CONTENT_MAX_CHARS) {
            lines.push(format!("    envoy asks: {question}"));
        }
    }
    lines.push("answer one with `.mesh answer <id> <text>`".to_string());
    if !access.is_empty() {
        lines.push("Access requests (decide with grant or refuse):".to_string());
    }
    for record in access {
        lines.push(format!(
            "  {}  {}  received {}  {} · grant: .mesh grant {} | refuse: .mesh refuse {}",
            record.id,
            short(&record.peer_destination),
            record.received_at,
            plural(record.paths.len(), "path", "paths"),
            record.id,
            record.id
        ));
    }
    lines.join("\n")
}

fn render_broadcast(outcome: &BroadcastOutcome) -> String {
    if outcome.recipients.is_empty() {
        return "No trusted peer has a known path right now; nothing was sent. `.mesh peers` lists what this node has heard from.".to_string();
    }
    let (mut delivered, mut stored, mut unreachable, mut refused) = (0, 0, 0, 0);
    let mut lines = Vec::with_capacity(outcome.recipients.len() + 1);
    for recipient in &outcome.recipients {
        let status = match &recipient.outcome {
            RecipientOutcome::Delivered => {
                delivered += 1;
                "delivered".to_string()
            }
            RecipientOutcome::StoreAndForward => {
                stored += 1;
                "store-and-forward".to_string()
            }
            RecipientOutcome::Unreachable { reason } => {
                unreachable += 1;
                format!("unreachable: {reason}")
            }
            RecipientOutcome::Refused { reason } => {
                refused += 1;
                format!("refused: {reason}")
            }
        };
        lines.push(format!(
            "  {} ({}): {status}",
            name_label(recipient.display_name.as_deref()),
            short(&recipient.destination)
        ));
    }
    lines.push(format!(
        "Bulletin {}: {delivered} delivered, {stored} store-and-forward, {unreachable} unreachable, {refused} refused.",
        short(&outcome.id)
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::knocks::KNOCK_RECORD_VERSION;
    use crate::mesh::message::{RawPeerMessage, RecipientReport};
    use crate::mesh::pending::{
        INBOUND_RECORD_VERSION, InboundKind, PENDING_RECORD_VERSION, PendingRecord,
    };
    use crate::mesh::test_support::{Compatibility, PropagationNode, private_config};
    use rand_core::OsRng;
    use rns_transport::destination::{DestinationName, SingleOutputDestination};
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use std::path::PathBuf;
    use std::time::Duration;

    fn peer(name: Option<&str>, seen_secs_ago: u64, now: SystemTime) -> PeerRecord {
        PeerRecord {
            destination_hash: "ab".repeat(16),
            identity_hash: "cd".repeat(16),
            name_hash: String::new(),
            display_name: name.map(str::to_string),
            protocol_version: 1,
            compatibility: Compatibility::Compatible,
            hops: 2,
            first_seen: now - Duration::from_secs(3600),
            last_seen: now - Duration::from_secs(seen_secs_ago),
        }
    }

    fn knock(name: Option<&str>, intro: Option<&str>, received_at: &str) -> KnockRecord {
        KnockRecord {
            version: KNOCK_RECORD_VERSION,
            received_at: received_at.to_string(),
            identity_hash: "ef".repeat(16),
            destination_hash: "12".repeat(16),
            name_hash: String::new(),
            display_name: name.map(str::to_string),
            intro: intro.map(str::to_string),
            hops: 1,
        }
    }

    fn public_config() -> MeshConfig {
        MeshConfig {
            interfaces: vec![
                MeshInterface::Lan,
                MeshInterface::Public {
                    host: "relay.example.com".into(),
                    port: 4242,
                },
            ],
            ..MeshConfig::default()
        }
    }

    #[test]
    fn flag_given_confirms_without_prompting() {
        for tty in [true, false] {
            let ok = confirm_or_flag_with("Go?", "--yes", true, tty, |_| {
                panic!("the prompt must not run")
            })
            .unwrap();
            assert!(ok);
        }
    }

    #[test]
    fn no_terminal_and_no_flag_is_an_error_naming_the_flag() {
        let err = confirm_or_flag_with("Go?", "--yes", false, false, |_| Some(true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--yes"), "{err}");
        assert!(err.contains("not a terminal"), "{err}");
        assert!(err.starts_with("Go?"), "{err}");
    }

    #[test]
    fn a_terminal_answer_is_taken_as_given() {
        assert!(confirm_or_flag_with("Go?", "--yes", false, true, |_| Some(true)).unwrap());
        assert!(!confirm_or_flag_with("Go?", "--yes", false, true, |_| Some(false)).unwrap());
    }

    #[test]
    fn an_interrupted_prompt_counts_as_no() {
        assert!(!confirm_or_flag_with("Go?", "--yes", false, true, |_| None).unwrap());
    }

    #[test]
    fn colour_needs_a_terminal_and_no_opt_out() {
        assert!(colour_allowed(true, false));
        assert!(!colour_allowed(true, true));
        assert!(!colour_allowed(false, false));
        assert!(!colour_allowed(false, true));
    }

    #[test]
    fn no_color_opt_out_is_the_shared_utils_parse() {
        // Mesh has no NO_COLOR parser of its own: the stderr decision is the shared
        // `utils::no_color_env_set` combined with the tty check, nothing else.
        assert_eq!(
            err_text_coloured(),
            std::io::stderr().is_terminal() && !crate::utils::no_color_env_set()
        );
        // The shared parse is `parse_bool`: NO_COLOR=0 keeps colour, unlike "any non-empty".
        assert_eq!(crate::utils::parse_bool("0"), Some(false));
        assert_eq!(crate::utils::parse_bool("1"), Some(true));
        assert_eq!(crate::utils::parse_bool(""), None);
    }

    #[test]
    fn help_lists_every_verb_with_an_example() {
        let help = render_help();
        for (verb, description, example) in VERBS {
            assert!(help.contains(&format!(".mesh {verb}")), "{help}");
            assert!(help.contains(description), "{help}");
            assert!(help.contains(example), "{help}");
        }
        assert!(help.contains("config.yaml"), "{help}");
    }

    #[test]
    fn verb_help_shows_the_example_and_falls_back_to_the_full_list() {
        let status = render_verb_help("status");
        assert!(status.starts_with("Usage: .mesh status"), "{status}");
        assert_eq!(render_verb_help("nope"), render_help());
    }

    #[test]
    fn untrust_and_block_help_contrast_each_other() {
        for verb in ["trust", "untrust", "forget", "block", "unblock"] {
            assert!(VERBS.iter().any(|(name, _, _)| *name == verb), "{verb}");
        }
        assert!(render_verb_help("untrust").contains("`block` remembers"));
        assert!(render_verb_help("block").contains("`untrust` only forgets"));
        assert!(render_verb_help("forget").contains("alias of `untrust`"));
    }

    /// Usage probe: the help a user reads is the agreed literal, word for word, in both
    /// places it is spelled (the `.mesh` verb table and the REPL command list), and the
    /// withdrawn verbs appear in neither. The `untrust` line names what the trusted-all
    /// branch does; `forget` says it is the alias; the `untrust` usage reads
    /// `[--yes|--dry-run]`, the grammar the parser takes.
    #[test]
    fn usage_probe_help_literals_match_in_the_verb_table_and_the_command_list() {
        const UNTRUST: &str = "Forget a trusted instance, or an identity with every instance bound to it; `untrust` forgets (an instance of an identity trusted for all is refused instead), `block` remembers and refuses";
        const FORGET: &str = "alias of `untrust`: forget this peer";

        let verb_description = |verb: &str| -> &str {
            VERBS
                .iter()
                .find(|(name, _, _)| *name == verb)
                .map(|(_, description, _)| *description)
                .unwrap_or_else(|| panic!("no VERBS row for {verb}"))
        };
        let command_description = |verb: &str| -> &str {
            let name = format!(".mesh {verb}");
            crate::repl::REPL_COMMANDS
                .iter()
                .find(|command| command.name == name)
                .map(|command| command.description)
                .unwrap_or_else(|| panic!("no REPL_COMMANDS row for {name}"))
        };

        assert_eq!(verb_description("untrust"), UNTRUST);
        assert_eq!(command_description("untrust"), UNTRUST);
        assert_eq!(verb_description("forget"), FORGET);
        assert_eq!(command_description("forget"), FORGET);
        assert!(
            render_verb_help("untrust").starts_with("Usage: .mesh untrust <destination> [--yes|--dry-run] | --identity <identity> [--dry-run|--confirm untrust-<identity-short>]"),
            "{}",
            render_verb_help("untrust")
        );
        assert!(
            render_verb_help("forget").starts_with("Usage: .mesh forget <destination> [--yes|--dry-run] | --identity <identity> [--dry-run|--confirm untrust-<identity-short>]"),
            "{}",
            render_verb_help("forget")
        );

        let help = render_help();
        for present in [
            ".mesh sync ",
            ".mesh forget ",
            ".mesh untrust ",
            ".mesh trust ",
        ] {
            assert!(help.contains(present), "{present:?} missing from\n{help}");
        }
        // `deny` is a file verb now; the peer refusal it once spelled is `untrust`.
        assert!(
            verb_description("deny").contains("share rule"),
            "{}",
            verb_description("deny")
        );
        let withdrawn = ".mesh undeny";
        assert!(!help.contains(withdrawn), "{withdrawn:?} still in\n{help}");
        assert!(
            !crate::repl::REPL_COMMANDS
                .iter()
                .any(|command| command.name == withdrawn),
            "{withdrawn:?} still listed"
        );
        // A later file verb named `fetch` would get its own row; only the row is pinned absent.
        assert!(!VERBS.iter().any(|(name, _, _)| *name == "fetch"));
        assert!(
            !crate::repl::REPL_COMMANDS
                .iter()
                .any(|command| command.name == ".mesh fetch")
        );
    }

    #[test]
    fn split_tokens_keeps_a_quoted_run_as_one_token() {
        assert_eq!(
            split_tokens("--label \"two words\" abc"),
            vec!["--label", "two words", "abc"]
        );
        assert_eq!(
            split_tokens("abc \"runs to the end"),
            vec!["abc", "runs to the end"]
        );
        assert_eq!(split_tokens("--label \"\" abc"), vec!["--label", "", "abc"]);
        assert!(split_tokens("   ").is_empty());
    }

    const TRUST_FLAGS: &[&str] = &[
        "--yes",
        "--label",
        "--identity",
        "--prune",
        "--older-than",
        "--dry-run",
        "--confirm",
    ];

    #[test]
    fn parse_mutation_args_reads_flags_anywhere_and_refuses_the_rest() {
        let hash = "ab".repeat(16);
        let args = parse_mutation_args(&format!("{hash} --yes"), "trust", TRUST_FLAGS).unwrap();
        assert!(args.yes);
        assert_eq!(args.positional, vec![hash.clone()]);

        let err = parse_mutation_args(&format!("{hash} --bogus"), "trust", TRUST_FLAGS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unexpected '--bogus'"), "{err}");
        assert!(err.contains(&render_verb_help("trust")), "{err}");

        let err = parse_mutation_args(&format!("{hash} --label"), "trust", TRUST_FLAGS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'--label' needs a value"), "{err}");
        let err = parse_mutation_args("--label --yes", "trust", TRUST_FLAGS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'--label' needs a value"), "{err}");

        let err = parse_mutation_args(&format!("{hash} {hash}"), "trust", TRUST_FLAGS)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("Unexpected '{hash}'")), "{err}");
    }

    #[test]
    fn older_than_accepts_days_hours_minutes_only() {
        for (text, secs) in [("30d", 30 * 86_400), ("12h", 12 * 3_600), ("90m", 90 * 60)] {
            let duration = parse_older_than(text).unwrap();
            assert_eq!(duration, Duration::from_secs(secs), "{text}");
            assert_eq!(older_than_text(duration), text);
        }
        for text in ["abc", "0d", "5w", ""] {
            let err = parse_older_than(text).unwrap_err().to_string();
            assert!(err.contains("30d, 12h or 90m"), "{text:?}: {err}");
        }
    }

    #[test]
    fn prune_token_names_the_count_or_is_refused() {
        assert_eq!(prune_token_count("prune-3").unwrap(), 3);
        for token in ["prune-x", "3", "untrust-3"] {
            let err = prune_token_count(token).unwrap_err().to_string();
            assert!(err.contains("prune-<N>"), "{token}: {err}");
        }
    }

    #[test]
    fn trust_argument_shapes_are_classified() {
        let hash = "ab".repeat(16);
        let trust_arg =
            |rest: &str| parse_mutation_args(rest, "trust", TRUST_FLAGS).and_then(classify_trust);
        let untrust_arg = |rest: &str| {
            parse_mutation_args(
                rest,
                "untrust",
                &["--yes", "--identity", "--dry-run", "--confirm"],
            )
            .and_then(|args| classify_untrust(args, "untrust"))
        };

        let err = trust_arg("--prune --yes").unwrap_err().to_string();
        assert!(err.contains("--confirm prune-<N>"), "{err}");
        let err = trust_arg(&format!("--prune {hash}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("Unexpected '{hash}'")), "{err}");
        let err = trust_arg(&format!("{hash} --dry-run"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unexpected '--dry-run'"), "{err}");
        match trust_arg(&format!("--identity {hash} --label x --yes")).unwrap() {
            Some(TrustArg::Identity { target, label, yes }) => {
                assert_eq!(target, hash);
                assert_eq!(label.as_deref(), Some("x"));
                assert!(yes);
            }
            _ => panic!("--identity is the identity shape"),
        }
        assert!(matches!(
            trust_arg("--prune --older-than 12h --confirm prune-2").unwrap(),
            Some(TrustArg::Prune {
                older_than: Some(older_than),
                confirm: Some(confirm),
            }) if older_than == Duration::from_secs(12 * 3_600) && confirm == "prune-2"
        ));
        assert!(trust_arg("--yes").unwrap().is_none());

        let err = untrust_arg(&format!("--identity {hash} --yes"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--confirm untrust-"), "{err}");
        let err = untrust_arg(&format!("{hash} --confirm x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unexpected '--confirm'"), "{err}");
        let err = untrust_arg(&format!("{hash} --dry-run --yes"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unexpected '--yes'"), "{err}");
        assert!(matches!(
            untrust_arg(&format!("{hash} --yes")).unwrap(),
            Some(UntrustArg::Destination {
                target,
                yes: true,
                dry_run: false
            }) if target == hash
        ));
        assert!(matches!(
            untrust_arg(&format!("{hash} --dry-run")).unwrap(),
            Some(UntrustArg::Destination {
                target,
                yes: false,
                dry_run: true
            }) if target == hash
        ));
    }

    #[test]
    fn split_verb_separates_the_verb_from_the_rest() {
        assert_eq!(split_verb(None), None);
        assert_eq!(split_verb(Some("   ")), None);
        assert_eq!(split_verb(Some("on")), Some(("on", None)));
        assert_eq!(
            split_verb(Some("status  \"ship it\" ")),
            Some(("status", Some("\"ship it\"")))
        );
    }

    #[test]
    fn parse_args_accepts_known_flags_and_refuses_the_rest() {
        let args = parse_args(Some("--yes --fresh"), &["--yes", "--fresh"], "on").unwrap();
        assert!(args.has("--yes"));
        assert!(args.has("--fresh"));
        let none = parse_args(None, &["--yes"], "off").unwrap();
        assert!(!none.has("--yes"));
        let err = parse_args(Some("--force"), &["--yes"], "off")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert!(err.contains("Usage: .mesh off"), "{err}");
    }

    #[test]
    fn status_argument_is_classified_by_shape() {
        assert!(matches!(classify_status(None), StatusArg::Own));
        assert!(matches!(classify_status(Some("clear")), StatusArg::Clear));
        assert!(matches!(classify_status(Some("\"\"")), StatusArg::Clear));
        assert!(matches!(
            classify_status(Some("\"ship it\"")),
            StatusArg::Set("ship it")
        ));
        let hex = "AB".repeat(16);
        match classify_status(Some(&hex)) {
            StatusArg::Fetch(destination) => assert_eq!(destination, "ab".repeat(16)),
            _ => panic!("a 32-hex argument fetches"),
        }
        assert!(matches!(classify_status(Some("ship it")), StatusArg::Help));
        assert!(matches!(classify_status(Some("abc")), StatusArg::Help));
    }

    #[test]
    fn brief_argument_is_classified_by_shape() {
        assert!(matches!(classify_brief(None), BriefArg::Show));
        assert!(matches!(
            classify_brief(Some("auto")),
            BriefArg::Mode(MeshBrief::Auto)
        ));
        assert!(matches!(
            classify_brief(Some("manual")),
            BriefArg::Mode(MeshBrief::Manual)
        ));
        assert!(matches!(
            classify_brief(Some("off")),
            BriefArg::Mode(MeshBrief::Off)
        ));
        assert!(matches!(classify_brief(Some("clear")), BriefArg::Clear));
        assert!(matches!(
            classify_brief(Some("set \"we ship on Friday\"")),
            BriefArg::Set("we ship on Friday")
        ));
        assert!(matches!(
            classify_brief(Some("set plain words")),
            BriefArg::Set("plain words")
        ));
        assert!(matches!(classify_brief(Some("set")), BriefArg::Help));
        assert!(matches!(classify_brief(Some("set \"\"")), BriefArg::Help));
        assert!(matches!(classify_brief(Some("settle")), BriefArg::Help));
        assert!(matches!(classify_brief(Some("loud")), BriefArg::Help));
    }

    #[test]
    fn public_and_private_confirmations_are_worded_apart() {
        let public = on_question(&public_config());
        let private = on_question(&MeshConfig::default());
        assert!(public.contains("WORLD-VISIBLE"), "{public}");
        assert!(public.contains("relay.example.com:4242"), "{public}");
        assert!(!private.contains("WORLD-VISIBLE"), "{private}");
        assert_ne!(public, private);
    }

    #[test]
    fn on_preview_names_what_leaves_to_whom_and_the_off_command() {
        let config = MeshConfig {
            display_name: Some("Ann".into()),
            ..public_config()
        };
        let text = render_on_preview(&config, "work", false, None);
        assert!(text.contains("session 'work'"), "{text}");
        assert!(text.contains("config.yaml is not changed"), "{text}");
        assert!(text.contains("announce:"), "{text}");
        assert!(
            text.contains(
                "\n  propagation sync: this node identifies itself to the nearest propagation node heard, at join and every 300 s; .mesh sync runs one now\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "display name: 'Ann' on lan and private interfaces; withheld on public ones"
            ),
            "{text}"
        );
        assert!(text.contains("lan: every node on this LAN"), "{text}");
        assert!(text.contains("world-visible"), "{text}");
        assert!(text.contains("anyone on the internet"), "{text}");
        assert!(
            text.ends_with("Turn it off again with `.mesh off`."),
            "{text}"
        );
        assert!(!text.contains("fresh id:"), "{text}");

        let quiet = MeshConfig {
            announce: false,
            ..MeshConfig::default()
        };
        let text = render_on_preview(&quiet, "work", true, None);
        assert!(text.contains("announce: nothing until"), "{text}");
        assert!(
            text.contains("\n  propagation sync: off (announce: false); .mesh sync runs one\n"),
            "{text}"
        );
        assert!(text.contains("display name: none"), "{text}");
        assert!(
            text.contains("fresh id: this session gets a new mesh id and destination"),
            "{text}"
        );

        let manual = MeshConfig {
            propagation_sync_interval_secs: 0,
            ..MeshConfig::default()
        };
        let text = render_on_preview(&manual, "work", false, None);
        assert!(
            text.contains(
                "\n  propagation sync: off (propagation_sync_interval_secs: 0); .mesh sync runs one\n"
            ),
            "{text}"
        );
        let quiet_and_manual = MeshConfig {
            announce: false,
            ..manual
        };
        let text = render_on_preview(&quiet_and_manual, "work", false, None);
        assert!(
            text.contains("propagation sync: off (announce: false)"),
            "announce: false is the reason given when both are off: {text}"
        );
    }

    #[test]
    fn reach_line_follows_the_configured_interfaces() {
        let lan_only = reach_line(&MeshConfig::default());
        assert_eq!(lan_only, "this link-local segment only");

        let private = reach_line(&private_config(4242));
        assert_eq!(private, "the configured relay and the peers it reaches");

        let public = reach_line(&public_config());
        assert!(
            public.starts_with("world-visible via a community node"),
            "{public}"
        );
        assert!(!public.contains("configured relay"), "{public}");

        let mut both = public_config();
        both.interfaces.push(MeshInterface::Private {
            host: "relay.internal".into(),
            port: 4242,
        });
        let both = reach_line(&both);
        assert!(
            both.contains("world-visible via a community node"),
            "{both}"
        );
        assert!(
            both.contains("the configured relay and the peers it reaches"),
            "{both}"
        );
    }

    #[test]
    fn cwd_warning_fires_for_home_and_roots_only() {
        let home = PathBuf::from("/Users/ann");
        assert!(cwd_warning(&home, Some(&home)).is_some());
        assert!(cwd_warning(Path::new("/"), Some(&home)).is_some());
        assert!(cwd_warning(&home.join("code").join("app"), Some(&home)).is_none());
        assert!(cwd_warning(Path::new("/srv/app"), None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cwd_warning_sees_through_a_symlinked_home() {
        let tmp = crate::mesh::test_support::TempDir::new("cwd-symlink-home");
        let home = tmp.path.join("home");
        let link = tmp.path.join("home-link");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(cwd_warning(&link, Some(&home)).is_some());
        assert!(cwd_warning(&home, Some(&link)).is_some());
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert!(cwd_warning(&project, Some(&link)).is_none());
    }

    #[test]
    fn peer_listing_marks_stale_incompatible_and_denied_peers() {
        let now = SystemTime::now();
        assert!(render_peers(&[], now).starts_with("No peers heard yet"));
        let mut old = peer(Some("Old"), 3600, now);
        old.compatibility = Compatibility::Incompatible { found: 9 };
        let rows = vec![
            PeerRow::Heard(peer(Some("Ann"), 5, now), "trusted", None),
            PeerRow::Heard(old, "denied", None),
            PeerRow::Heard(peer(None, 30, now), "untrusted", None),
        ];
        let text = render_peers(&rows, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("name"), "{text}");
        assert!(
            lines[1].contains("Ann") && lines[1].contains("trusted"),
            "{text}"
        );
        assert!(
            lines[1].contains("5s ago") && !lines[1].contains("stale"),
            "{text}"
        );
        assert!(
            lines[2].contains("denied") && lines[2].contains("(stale)"),
            "{text}"
        );
        assert!(lines[2].contains("1h ago"), "{text}");
        assert!(
            lines[3].contains("incompatible: speaks protocol 9"),
            "{text}"
        );
        assert!(lines[4].contains("(no name)"), "{text}");
        assert!(
            text.ends_with(
                "3 peer(s). Names are peer-supplied text. Full hashes: `.mesh info <dest>`."
            ),
            "{text}"
        );
    }

    fn deny_record(hash: &str, identity: Option<&str>) -> TrustRecord {
        let now = SystemTime::now();
        TrustRecord {
            tier: Tier::Destination,
            hash: hash.to_string(),
            identity: identity.map(str::to_string),
            label: None,
            note: None,
            added_at: now,
            last_seen_at: now,
            all_destinations: false,
            denied: true,
            session: false,
            key_changed: None,
        }
    }

    #[test]
    fn deny_only_destinations_are_appended_as_denied_rows() {
        let now = SystemTime::now();
        let heard = peer(Some("Ann"), 5, now);
        let unseen = "77".repeat(16);
        let bound = "88".repeat(16);
        let records = vec![
            deny_record(&unseen, None),
            deny_record(&heard.destination_hash, None),
            deny_record(&bound, Some(&"99".repeat(16))),
        ];
        let heard_hashes = vec![heard.destination_hash.clone()];
        let mut rows = vec![PeerRow::Heard(heard.clone(), "trusted", None)];
        rows.extend(unheard_rows(records, &heard_hashes, &[], |_| "trusted"));
        assert_eq!(
            rows.len(),
            3,
            "the heard destination's deny is not a second row"
        );
        let text = render_peers(&rows, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[2].starts_with('-'), "{text}");
        assert!(lines[2].contains(&"77".repeat(4)), "{text}");
        assert!(lines[2].contains("denied"), "{text}");
        assert!(lines[2].contains("never"), "{text}");
        assert!(!lines[2].contains("trusted"), "{text}");
        assert!(lines[3].contains(&"88".repeat(4)), "{text}");
        assert!(
            lines[3].contains(&"99".repeat(4)),
            "a deny bound to an identity shows it: {text}"
        );
        assert!(
            text.ends_with(
                "3 peer(s). Names are peer-supplied text. Full hashes: `.mesh info <dest>`."
            ),
            "{text}"
        );
    }

    #[test]
    fn peers_marks_a_record_whose_instance_was_announced_under_another_identity() {
        let now = SystemTime::now();
        let seen_identity = "77".repeat(16);
        let new_destination = "88".repeat(16);
        let rows = vec![
            PeerRow::Heard(
                peer(Some("Ann"), 5, now),
                "trusted",
                Some(KeyChangeMark {
                    change: KeyChange {
                        seen_identity: seen_identity.clone(),
                        at: now - Duration::from_secs(120),
                    },
                    new_destination: Some(new_destination.clone()),
                }),
            ),
            PeerRow::Heard(
                peer(Some("Bob"), 5, now),
                "trusted",
                Some(KeyChangeMark {
                    change: KeyChange {
                        seen_identity: seen_identity.clone(),
                        at: now,
                    },
                    new_destination: None,
                }),
            ),
            PeerRow::Heard(peer(Some("Cy"), 5, now), "trusted", None),
        ];
        let text = render_peers(&rows, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("Ann"), "{text}");
        assert!(lines[2].starts_with(&" ".repeat(20)), "{text}");
        assert!(
            lines[2].contains(&format!(
                "key changed: announced under identity {} 2m ago",
                short(&seen_identity)
            )),
            "{text}"
        );
        assert!(
            lines[2].contains(&format!("the grant stays with {}", "cd".repeat(4))),
            "{text}"
        );
        assert!(
            lines[2].contains(&format!("then .mesh trust {new_destination}; ")),
            "{text}"
        );
        assert!(
            lines[2].contains(&format!("; otherwise .mesh block {seen_identity}; ")),
            "the block exit names the full seen identity: {text}"
        );
        assert!(
            lines[2].ends_with(&format!(
                ".mesh untrust {} forgets the old key",
                "ab".repeat(16)
            )),
            "{text}"
        );
        assert!(lines[3].contains("Bob"), "{text}");
        assert!(
            lines[4].contains(&format!(
                "then .mesh trust its new destination once heard; otherwise .mesh block {seen_identity}; .mesh untrust "
            )),
            "a mark without a heard new destination still says what to do: {text}"
        );
        assert!(lines[5].contains("Cy"), "{text}");
        assert!(
            lines[6].starts_with("3 peer(s)."),
            "an unmarked row has no marker line: {text}"
        );
    }

    #[test]
    fn peers_lists_a_marked_record_after_its_row_aged_out() {
        let now = SystemTime::now();
        let old_destination = "77".repeat(16);
        let bound_identity = "88".repeat(16);
        let seen_identity = "99".repeat(16);
        let mut successor = peer(Some("Tia again"), 5, now);
        successor.identity_hash = seen_identity.clone();
        let mut record = deny_record(&old_destination, Some(&bound_identity));
        record.denied = false;
        record.label = Some("Tia".to_string());
        record.last_seen_at = now - Duration::from_secs(7200);
        record.key_changed = Some(KeyChange {
            seen_identity: seen_identity.clone(),
            at: now - Duration::from_secs(3600),
        });
        let mut unlabeled = record.clone();
        unlabeled.hash = "66".repeat(16);
        unlabeled.label = None;
        let mut unmarked = deny_record(&"55".repeat(16), None);
        unmarked.denied = false;
        let heard = vec![successor.destination_hash.clone()];
        let successors = vec![(old_destination.clone(), &successor)];

        let mut rows = vec![PeerRow::Heard(successor.clone(), "untrusted", None)];
        rows.extend(unheard_rows(
            vec![record.clone(), unlabeled, unmarked],
            &heard,
            &successors,
            |_| "trusted",
        ));
        assert_eq!(rows.len(), 3, "a record without a mark is not a row");

        let text = render_peers(&rows, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[2].starts_with("Tia "), "{text}");
        assert!(lines[2].contains(&"77".repeat(4)), "{text}");
        assert!(lines[2].contains(&"88".repeat(4)), "{text}");
        assert!(lines[2].contains("trusted"), "{text}");
        assert!(
            lines[2].contains("   -  2h ago"),
            "no hops for a row nobody heard: {text}"
        );
        assert!(lines[3].starts_with(&" ".repeat(20)), "{text}");
        assert!(
            lines[3].contains(&format!(
                "key changed: announced under identity {} 1h ago; the grant stays with {}",
                short(&seen_identity),
                short(&bound_identity)
            )),
            "{text}"
        );
        assert!(
            lines[3].contains(&format!(
                "then .mesh trust {}; otherwise .mesh block {seen_identity}; .mesh untrust {old_destination} forgets the old key",
                successor.destination_hash
            )),
            "the exits name the heard successor, the full seen identity and the full old hash: {text}"
        );
        assert!(lines[4].starts_with("(not heard) "), "{text}");
        assert!(
            lines[5].contains(&format!(
                "then .mesh trust its new destination once heard; otherwise .mesh block {seen_identity}; .mesh untrust "
            )),
            "a mark whose successor is not heard still says what to do: {text}"
        );
        assert!(lines[6].starts_with("3 peer(s)."), "{text}");

        let heard_again = vec![old_destination.clone()];
        assert!(
            unheard_rows(vec![record], &heard_again, &successors, |_| "trusted").is_empty(),
            "a heard destination carries its mark on its own row"
        );
    }

    #[test]
    fn an_aged_out_marked_record_of_a_blocked_identity_is_labelled_blocked() {
        let tmp = crate::mesh::test_support::TempDir::new("repl-peers-marked-blocked");
        let now = SystemTime::now();
        let old_destination = "77".repeat(16);
        let bound_identity = "88".repeat(16);
        crate::mesh::test_support::TrustList::default()
            .destination(&old_destination, &bound_identity)
            .block(&bound_identity)
            .write(&tmp.path);
        let trust = TrustStore::open(&tmp.path).unwrap();
        let mut records = trust.records();
        let record = records
            .iter_mut()
            .find(|record| record.hash == old_destination)
            .unwrap();
        record.key_changed = Some(KeyChange {
            seen_identity: "99".repeat(16),
            at: now - Duration::from_secs(3600),
        });

        let rows = unheard_rows(records, &[], &[], |record| unheard_label(&trust, record));
        assert_eq!(rows.len(), 1);

        let text = render_peers(&rows, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains(&"77".repeat(4)), "{text}");
        assert!(lines[1].contains(" blocked "), "{text}");
        assert!(
            !lines[1].contains("trusted"),
            "the row is labelled by the store's verdict, not by the record's existence: {text}"
        );
        assert!(lines[2].contains("key changed"), "{text}");
    }

    #[test]
    fn a_marked_records_label_is_sanitised_like_any_peer_name() {
        let now = SystemTime::now();
        let label = "Tia\u{1b}[31m\nX";
        let mut record = deny_record(&"77".repeat(16), Some(&"88".repeat(16)));
        record.denied = false;
        record.label = Some(label.to_string());
        record.key_changed = Some(KeyChange {
            seen_identity: "99".repeat(16),
            at: now - Duration::from_secs(60),
        });

        let rows = unheard_rows(vec![record], &[], &[], |_| "trusted");
        let text = render_peers(&rows, now);

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text:?}");
        let shown = display_text(label, DISPLAY_NAME_MAX_CHARS).unwrap();
        assert!(lines[1].starts_with(&format!("{shown:<20} ")), "{text:?}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains("[31m"), "{text:?}");
    }

    #[test]
    fn predecessors_text_sanitises_an_unparsable_rotated_at() {
        let now = SystemTime::now();
        let raw = "yesterday\u{1b}[31m\nish";
        let all = [Predecessor {
            version: identity::PREDECESSOR_RECORD_VERSION,
            identity_hash: "ab".repeat(16),
            rotated_at: raw.to_string(),
            reason: "rotate".to_string(),
        }];

        let text = predecessors_text(Ok(&all), now);

        let shown = display_text(raw, DISPLAY_NAME_MAX_CHARS).unwrap();
        assert_eq!(
            text,
            format!(
                "1 (latest {} rotated {shown})",
                short(&all[0].identity_hash)
            )
        );
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\n'), "{text:?}");
    }

    #[test]
    fn peers_lists_a_denied_and_marked_unheard_record_once_as_denied() {
        let now = SystemTime::now();
        let old_destination = "77".repeat(16);
        let seen_identity = "99".repeat(16);
        let mut record = deny_record(&old_destination, Some(&"88".repeat(16)));
        record.key_changed = Some(KeyChange {
            seen_identity: seen_identity.clone(),
            at: now - Duration::from_secs(120),
        });

        let rows = unheard_rows(vec![record], &[], &[], |_| "trusted");
        assert_eq!(
            rows.len(),
            1,
            "a denied record with a mark is one row, not two"
        );

        let text = render_peers(&rows, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].starts_with('-'), "{text}");
        assert!(lines[1].contains("denied"), "{text}");
        assert!(!lines[1].contains("trusted"), "{text}");
        assert!(lines[2].starts_with(&" ".repeat(20)), "{text}");
        assert!(
            lines[2].contains(&format!(
                "key changed: announced under identity {} 2m ago; the grant stays with {}",
                short(&seen_identity),
                "88".repeat(4)
            )),
            "the deny row still shows the mark: {text}"
        );
        assert!(
            lines[2].ends_with(&format!(
                "otherwise .mesh block {seen_identity}; .mesh untrust {old_destination} forgets the old key"
            )),
            "{text}"
        );
        assert!(lines[3].starts_with("1 peer(s)."), "{text}");
    }

    #[test]
    fn knock_listing_shows_label_identity_age_and_intro() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_120);
        assert!(render_knocks(&[], now).starts_with("No knocks"));
        let records = vec![
            knock(Some("Bea"), Some("hello there"), "2026-09-21T14:13:20Z"),
            knock(None, None, "not a time"),
        ];
        let text = render_knocks(&records, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("Bea"), "{text}");
        assert!(lines[1].contains(&"ef".repeat(4)), "{text}");
        assert!(lines[1].contains("2m ago"), "{text}");
        assert!(lines[1].ends_with("hello there"), "{text}");
        assert!(lines[2].contains("(no name)"), "{text}");
        assert!(lines[2].contains("unknown"), "{text}");
        assert!(lines[2].ends_with("(no intro)"), "{text}");
        assert!(
            text.ends_with("2 knock(s). Names and intros are peer-supplied text."),
            "{text}"
        );
    }

    #[test]
    fn peer_detail_covers_a_peer_a_knocker_and_both() {
        let now = SystemTime::now();
        let destination = "ab".repeat(16);
        let known = peer(Some("Ann"), 5, now);
        let text = render_peer_detail(
            &destination,
            Some(&known),
            &KnockLookup::NotKnocked,
            Some("trusted"),
            now,
        );
        assert!(
            text.starts_with(&format!("destination: {destination}\n")),
            "{text}"
        );
        assert!(text.contains("trust: trusted"), "{text}");
        assert!(text.contains("knocked: no"), "{text}");

        let text = render_peer_detail(
            &destination,
            Some(&known),
            &KnockLookup::Unreadable,
            Some("trusted"),
            now,
        );
        assert!(
            text.contains("knocked: unknown (knock cache unreadable)"),
            "{text}"
        );

        let knocker = knock(
            Some("Bea"),
            Some("hi\nthere\u{1b}[31m"),
            "2026-09-21T14:13:20Z",
        );
        let text = render_peer_detail(
            &destination,
            None,
            &KnockLookup::Knocked(knocker),
            None,
            now,
        );
        assert!(
            text.contains("trust: untrusted (known only from its knock)"),
            "{text}"
        );
        assert!(text.contains("name: Bea"), "{text}");
        assert!(text.contains("intro: hi there"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");

        let knocker = knock(Some("Bea"), Some("hi"), "2026-09-21T14:13:20Z");
        let text = render_peer_detail(
            &destination,
            Some(&known),
            &KnockLookup::Knocked(knocker),
            Some("untrusted"),
            now,
        );
        assert_eq!(text.matches("identity:").count(), 1, "{text}");
        assert!(text.contains("name: Ann"), "{text}");
        assert!(
            text.contains("knocked:") && text.contains("intro: hi"),
            "{text}"
        );
    }

    #[test]
    fn propagation_nodes_render_nearest_first_or_say_none() {
        let now = SystemTime::now();
        let none = render_propagation_nodes(Vec::new(), now);
        assert!(none.contains("none heard yet"), "{none}");
        assert!(
            none.contains("selection: nearest by hops"),
            "the selection rule is stated even before a node is heard: {none}"
        );

        let record = |hops: u8, aspect: &str| PropagationNodeRecord {
            node: PropagationNode {
                destination: SingleOutputDestination::new(
                    *TransportIdentity::new_from_rand(OsRng).as_identity(),
                    DestinationName::new("lxmf", aspect),
                )
                .desc,
                stamp_cost: 8,
                per_transfer_limit_kb: 256,
                propagation_enabled: true,
            },
            hops,
            last_seen: now - Duration::from_secs(60),
        };
        let far = record(3, "far");
        let near = record(1, "near");
        let far_hex = far.node.destination.address_hash.to_hex_string();
        let near_hex = near.node.destination.address_hash.to_hex_string();
        let text = render_propagation_nodes(vec![far, near], now);
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("  propagation_nodes[0]") && first.contains(&near_hex),
            "the renderer itself puts the 1-hop node first: {text}"
        );
        assert!(first.contains("(1 hop(s), 1m ago)"), "{text}");
        assert!(text.lines().nth(1).unwrap().contains(&far_hex), "{text}");
        assert!(
            text.contains(
                "selection: nearest by hops (an operator-pinned node is not supported yet)"
            ),
            "{text}"
        );
    }

    #[test]
    fn ages_scale_with_distance() {
        let now = SystemTime::now();
        assert_eq!(age_text(now, now - Duration::from_secs(59)), "59s ago");
        assert_eq!(age_text(now, now - Duration::from_secs(60)), "1m ago");
        assert_eq!(age_text(now, now - Duration::from_secs(7200)), "2h ago");
        assert_eq!(age_text(now, now - Duration::from_secs(200_000)), "2d ago");
        assert_eq!(age_text(now, now + Duration::from_secs(5)), "0s ago");
    }

    fn message(kind: PeerKind, content: &str, in_reply_to: Option<&str>) -> PeerMessage {
        PeerMessage::new(RawPeerMessage {
            source_identity: "cd".repeat(16),
            source_destination: "ab".repeat(16),
            destination: "01".repeat(16),
            title: None,
            content: content.to_string(),
            fields: None,
            timestamp: 0.0,
            message_id: "m1".repeat(16),
            in_reply_to: in_reply_to.map(str::to_string),
            kind,
            via: PeerVia::Direct,
            thread: None,
            disposition: None,
            retry_after: None,
            parts: Vec::new(),
            dropped_parts: 0,
        })
    }

    fn inbox_row(message: PeerMessage) -> InboxRow {
        InboxRow {
            name: "Ann".to_string(),
            received: DateTime::from_timestamp(1_790_000_120, 0).unwrap(),
            message,
        }
    }

    #[test]
    fn inbox_lines_name_kind_sender_route_time_and_reply_target() {
        assert_eq!(render_inbox(&[], &[]), "Inbox is empty.");
        let mut bulletin = message(PeerKind::Bulletin, "first line\nsecond line", None);
        bulletin.title = Some("Heads up".into());
        bulletin.via = PeerVia::StoreAndForward;
        let rows = vec![
            inbox_row(bulletin),
            inbox_row(message(
                PeerKind::Reply,
                &"x".repeat(PEER_CONTENT_MAX_CHARS + 300),
                Some(&"77".repeat(16)),
            )),
        ];
        let text = render_inbox(&rows, &["q1".to_string()]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(
            lines[0].starts_with(&format!(
                "[bulletin] from Ann ({}) via store-and-forward at 14:15 UTC: Heads up: first line second line",
                "ab".repeat(4)
            )),
            "{text}"
        );
        assert!(!lines[0].contains("reply to"), "{text}");
        assert!(lines[1].starts_with("[reply] from Ann"), "{text}");
        assert!(lines[1].contains("via direct at"), "{text}");
        assert!(
            lines[1].ends_with(&format!(" (reply to {})", "77".repeat(4))),
            "{text}"
        );
        let content_len = lines[1]
            .split(": ")
            .nth(1)
            .unwrap()
            .trim_end_matches(|c| c != 'x')
            .len();
        assert_eq!(content_len, PEER_CONTENT_MAX_CHARS, "{text}");
        assert_eq!(lines[2], "answered awaiting collect: q1");
    }

    #[test]
    fn inbox_lines_render_every_part_shape_and_count_the_dropped_ones() {
        let mut with_parts = message(PeerKind::Message, "see attached", None);
        with_parts.parts = vec![
            Part::Text {
                text: "first line\nsecond line".into(),
            },
            Part::Data {
                data: serde_json::json!({"k": "v"}),
            },
            Part::File {
                name: "docs/notes.md".into(),
                size: 8,
                sha256: "ab".repeat(32),
                staged: Some(PathBuf::from("/tmp/inbox/abcdef01/docs/notes.md")),
                reference: None,
            },
        ];
        with_parts.dropped_parts = 2;

        let text = render_inbox(&[inbox_row(with_parts)], &[]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "{text}");
        assert_eq!(lines[1], "  text: first line second line");
        assert_eq!(lines[2], "  data: 9 bytes");
        assert_eq!(
            lines[3],
            "  file: docs/notes.md (8 B) staged at /tmp/inbox/abcdef01/docs/notes.md"
        );
        assert_eq!(lines[4], "  (2 parts dropped)");
    }

    fn correlation(id: &str, state: PendingState) -> Correlation {
        Correlation {
            record: PendingRecord {
                version: PENDING_RECORD_VERSION,
                id: id.to_string(),
                peer_destination: "ab".repeat(16),
                peer_identity: "cd".repeat(16),
                thread: id.to_string(),
                question: "what now?".to_string(),
                sent_at: "2026-09-21T14:13:20Z".to_string(),
                timeout_at: "2026-09-21T14:23:20Z".to_string(),
                state,
                reply: None,
            },
            reply: None,
        }
    }

    fn inbound(id: &str, envoy_question: &str) -> InboundRecord {
        InboundRecord {
            version: INBOUND_RECORD_VERSION,
            id: id.to_string(),
            peer_destination: "12".repeat(16),
            peer_identity: "ef".repeat(16),
            thread: id.to_string(),
            question: "may I read the plan?".to_string(),
            envoy_question: envoy_question.to_string(),
            received_at: "2026-09-21T14:13:20Z".to_string(),
            kind: InboundKind::Question,
            paths: Vec::new(),
            reason: String::new(),
        }
    }

    #[test]
    fn pending_lists_both_directions_or_says_none() {
        let empty = render_pending(&[], &[]);
        let lines: Vec<&str> = empty.lines().collect();
        assert_eq!(
            lines,
            [
                "Questions this node asked (awaiting a reply):",
                "  none",
                "Questions peers asked (escalated to you):",
                "  none",
                "answer one with `.mesh answer <id> <text>`",
            ]
        );

        let asked = vec![
            correlation("q1", PendingState::Open),
            correlation("q2", PendingState::Answered),
        ];
        let escalated = vec![inbound("p1", "Share the plan?"), inbound("p2", "")];
        let text = render_pending(&asked, &escalated);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8, "{text}");
        assert!(
            lines[1].starts_with(&format!(
                "  q1  {}  open  sent 2026-09-21T14:13:20Z",
                "ab".repeat(4)
            )),
            "{text}"
        );
        assert!(lines[1].contains("timeout 2026-09-21T14:23:20Z"), "{text}");
        assert!(lines[1].ends_with("what now?"), "{text}");
        assert!(lines[2].contains("answered, awaiting collect"), "{text}");
        assert!(
            lines[4].starts_with(&format!(
                "  p1  {}  received 2026-09-21T14:13:20Z",
                "12".repeat(4)
            )),
            "{text}"
        );
        assert_eq!(lines[5], "    envoy asks: Share the plan?");
        assert!(lines[6].starts_with("  p2  "), "{text}");
        assert!(!lines[7].contains("envoy asks"), "{text}");
    }

    #[test]
    fn pending_lists_access_requests_apart_from_questions_and_never_their_paths() {
        let access = InboundRecord {
            kind: InboundKind::Access,
            question: String::new(),
            paths: vec!["secret/plan.md".into(), "src/x.rs".into()],
            reason: "need the struct".into(),
            ..inbound("acc-1", "")
        };
        let question = inbound("p1", "");

        let text = render_pending(&[], &[access, question.clone()]);
        let lines: Vec<&str> = text.lines().collect();
        let hint = lines
            .iter()
            .position(|line| line.starts_with("answer one with"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            lines[..hint].iter().any(|line| line.starts_with("  p1  ")),
            "{text}"
        );
        assert!(
            !lines[..hint].iter().any(|line| line.contains("acc-1")),
            "an access request is not listed among the questions: {text}"
        );
        assert_eq!(
            lines[hint + 1],
            "Access requests (decide with grant or refuse):",
            "{text}"
        );
        let row = lines[hint + 2];
        assert!(
            row.starts_with(&format!(
                "  acc-1  {}  received 2026-09-21T14:13:20Z",
                "12".repeat(4)
            )),
            "{text}"
        );
        assert!(row.contains("2 paths"), "{text}");
        assert!(row.contains("grant: .mesh grant acc-1"), "{text}");
        assert!(row.contains("refuse: .mesh refuse acc-1"), "{text}");
        for secret in ["secret/plan.md", "src/x.rs", "need the struct"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }

        let questions_only = render_pending(&[], &[question]);
        assert!(
            !questions_only.contains("Access requests"),
            "{questions_only}"
        );
        assert!(
            questions_only.ends_with("answer one with `.mesh answer <id> <text>`"),
            "{questions_only}"
        );
    }

    #[test]
    fn answer_splits_the_id_from_quoted_or_bare_text() {
        assert_eq!(
            split_id_and_text(Some("q1 \"yes, go ahead\"")),
            Some(("q1", "yes, go ahead"))
        );
        assert_eq!(
            split_id_and_text(Some("q1   plain words here")),
            Some(("q1", "plain words here"))
        );
        assert_eq!(split_id_and_text(Some("q1")), None);
        assert_eq!(split_id_and_text(Some("q1 \"\"")), None);
        assert_eq!(split_id_and_text(Some("q1   ")), None);
        assert_eq!(split_id_and_text(None), None);
    }

    #[test]
    fn answer_routes_to_the_peer_first_then_to_our_own_question() {
        assert_eq!(answer_route(true, false), AnswerRoute::Inbound);
        assert_eq!(answer_route(true, true), AnswerRoute::Inbound);
        assert_eq!(answer_route(false, true), AnswerRoute::Outbound);
        assert_eq!(answer_route(false, false), AnswerRoute::Unknown);
    }

    #[test]
    fn an_answer_into_our_own_question_carries_its_thread_and_disposition_on_the_wire() {
        use crate::mesh::message::to_r3_body;

        let mut correlation = correlation("q1", PendingState::Open);
        correlation.record.thread = "t-root".to_string();

        let out = outbound_answer(&correlation, "yes, go ahead").unwrap();

        assert_eq!(out.kind, PeerKind::Reply);
        assert_eq!(out.in_reply_to.as_deref(), Some("q1"));
        assert_eq!(out.thread.as_deref(), Some("t-root"));
        assert_eq!(out.disposition, Some(Disposition::Answered));
        assert_eq!(out.retry_after, None);
        let rmpv::Value::Map(entries) = to_r3_body(&out, 1.0) else {
            panic!("a map body");
        };
        let field = |name: &str| {
            entries
                .iter()
                .find(|(key, _)| key.as_str() == Some(name))
                .map(|(_, value)| value.as_str().unwrap().to_string())
        };
        assert_eq!(field("thread").as_deref(), Some("t-root"));
        assert_eq!(field("disposition").as_deref(), Some("answered"));
    }

    #[test]
    fn yes_flag_is_taken_only_as_the_leading_word() {
        assert_eq!(take_flag("--yes hello", "--yes"), (true, "hello"));
        assert_eq!(take_flag("hello --yes", "--yes"), (false, "hello --yes"));
        assert_eq!(take_flag("--yes", "--yes"), (true, ""));
        assert_eq!(
            take_flag("say --yes now", "--yes"),
            (false, "say --yes now")
        );
        assert_eq!(take_flag("--yesterday", "--yes"), (false, "--yesterday"));

        let out = parse_outgoing("--yes \"ship it\"").unwrap();
        assert!(out.yes);
        assert_eq!(out.text, "ship it");
        let out = parse_outgoing("ship it").unwrap();
        assert!(!out.yes);
        assert_eq!(out.text, "ship it");
        let out = parse_outgoing("please say --yes").unwrap();
        assert!(!out.yes, "a trailing --yes is text, not consent");
        assert_eq!(out.text, "please say --yes");
        assert!(parse_outgoing("--yes").is_none());
        assert!(parse_outgoing("\"\"").is_none());
    }

    fn recipient(name: Option<&str>, outcome: RecipientOutcome) -> RecipientReport {
        RecipientReport {
            destination: "ab".repeat(16),
            display_name: name.map(str::to_string),
            outcome,
        }
    }

    #[test]
    fn broadcast_report_lists_each_recipient_and_totals_them() {
        let nobody = BroadcastOutcome {
            id: "b1".repeat(16),
            recipients: vec![],
        };
        assert!(render_broadcast(&nobody).starts_with("No trusted peer has a known path"));

        let outcome = BroadcastOutcome {
            id: "b1".repeat(16),
            recipients: vec![
                recipient(Some("Ann"), RecipientOutcome::Delivered),
                recipient(None, RecipientOutcome::StoreAndForward),
                recipient(
                    Some("Cy"),
                    RecipientOutcome::Unreachable {
                        reason: "no path".into(),
                    },
                ),
                recipient(
                    Some("Di"),
                    RecipientOutcome::Refused {
                        reason: "no access".into(),
                    },
                ),
                recipient(Some("Ed"), RecipientOutcome::Delivered),
            ],
        };
        let text = render_broadcast(&outcome);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "{text}");
        assert_eq!(lines[0], format!("  Ann ({}): delivered", "ab".repeat(4)));
        assert!(lines[1].starts_with("  (no name) "), "{text}");
        assert!(lines[1].ends_with("store-and-forward"), "{text}");
        assert!(lines[2].ends_with("unreachable: no path"), "{text}");
        assert!(lines[3].ends_with("refused: no access"), "{text}");
        assert_eq!(
            lines[5],
            format!(
                "Bulletin {}: 2 delivered, 1 store-and-forward, 1 unreachable, 1 refused.",
                "b1".repeat(4)
            )
        );
    }

    #[test]
    fn results_go_to_stdout_and_warnings_to_stderr() {
        let source = include_str!("mesh.rs");
        let println = format!("{}!(", "println");
        let eprintln = format!("e{println}");
        let eprintln_count = source.matches(&eprintln).count();
        let println_count = source.matches(&println).count() - eprintln_count;
        assert_eq!(println_count, 1, "stdout is written by out_text alone");
        assert_eq!(eprintln_count, 2, "stderr is written by err_text alone");
    }

    #[test]
    fn markers_are_plain_words_in_the_rendered_text() {
        let now = SystemTime::now();
        let mut old = peer(Some("Old"), 3600, now);
        old.compatibility = Compatibility::Incompatible { found: 9 };
        let rows = vec![
            PeerRow::Heard(old, "denied", None),
            PeerRow::Heard(peer(Some("Bad"), 5, now), "blocked", None),
        ];
        let text = render_peers(&rows, now);
        for marker in ["denied", "blocked", "(stale)", "incompatible"] {
            assert!(text.contains(marker), "{marker}: {text}");
        }
        assert!(dropped_warning(3).starts_with("WARNING: 3 peer message(s)"));
    }

    #[test]
    fn send_notices_name_the_destination_and_the_audience() {
        let destination = "ab".repeat(16);
        let notice = sending_notice(&destination);
        assert!(notice.contains("Sending"), "{notice}");
        assert!(notice.contains("abababab"), "{notice}");
        assert!(!notice.contains(&destination), "{notice}");
        assert!(BROADCAST_NOTICE.contains("every peer this node trusts"));
        assert!(BROADCAST_NOTICE.contains("not trusted receive nothing"));
    }

    /// Spec-first usage probe: peer-supplied text in `.mesh pending` and `.mesh knocks` is
    /// display-sanitised (`display_text`): escape sequences, control characters and line
    /// breaks never reach the terminal, and one record never renders as two rows.
    #[test]
    fn pending_and_knock_rows_strip_peer_control_characters() {
        let hostile = "\u{1b}[2J\u{1b}[31mSTOLEN\u{7}\nfake row\r\u{200b}?";
        let forbidden = ['\u{1b}', '\u{7}', '\r', '\u{200b}'];

        let mut escalated = inbound("p1", hostile);
        escalated.question = hostile.to_string();
        let text = render_pending(&[correlation("q1", PendingState::Open)], &[escalated]);
        for c in forbidden {
            assert!(
                !text.contains(c),
                "{c:?} leaked into `.mesh pending`: {text:?}"
            );
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            6,
            "one escalated record is one row plus its envoy line: {text}"
        );
        assert!(lines[3].starts_with("  p1  "), "{text}");
        assert!(
            lines[3].contains("STOLEN") && lines[3].contains("fake row"),
            "the readable part of the text survives on the same row: {text}"
        );
        assert!(
            lines[4].starts_with("    envoy asks: ") && lines[4].contains("fake row"),
            "{text}"
        );
        assert!(
            !lines[3].contains("[2J") && !lines[3].contains("[31m"),
            "escape bodies are stripped, not just their ESC: {text}"
        );

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_120);
        let records = vec![knock(
            Some("\u{1b}[0mEve\u{200b}"),
            Some(hostile),
            "2026-09-21T14:13:20Z",
        )];
        let text = render_knocks(&records, now);
        for c in forbidden {
            assert!(
                !text.contains(c),
                "{c:?} leaked into `.mesh knocks`: {text:?}"
            );
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one knock is one row: {text}");
        assert!(lines[1].contains("Eve"), "{text}");
        assert!(!lines[1].contains("[0m"), "{text}");
        assert!(
            lines[1].contains("STOLEN") && lines[1].contains("fake row"),
            "{text}"
        );
        assert!(
            text.ends_with("1 knock(s). Names and intros are peer-supplied text."),
            "{text}"
        );

        // Peer text that is nothing but control characters renders as absent, never as an
        // empty column that could be mistaken for a blank name or intro.
        let blank = "\u{1b}[2J\u{7}\r\n";
        let records = vec![knock(Some(blank), Some(blank), "2026-09-21T14:13:20Z")];
        let text = render_knocks(&records, now);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[1].contains("(no name)"), "{text}");
        assert!(lines[1].ends_with("(no intro)"), "{text}");
        let mut escalated = inbound("p2", blank);
        escalated.question = blank.to_string();
        let text = render_pending(&[], &[escalated]);
        assert!(
            !text.contains("envoy asks"),
            "a blank envoy question prints no envoy line: {text}"
        );
        assert_eq!(
            text.lines().count(),
            5,
            "two headers, `none`, one row, the footer: {text}"
        );
    }

    mod repl {
        use super::super::*;
        use crate::config::{AppConfig, AppState, Session, WorkingMode};
        use crate::repl::run_repl_command;
        use crate::testing::TestConfigDirGuard;
        use crate::utils::create_abort_signal;
        use serial_test::serial;
        use std::future::Future;

        /// `run_repl_command`'s poll frame is far larger than a test thread's stack, so the
        /// body runs on an 8 MiB thread; two workers keep a node's tasks moving meanwhile.
        fn run_async<F>(f: F) -> F::Output
        where
            F: Future + Send,
            F::Output: Send,
        {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .stack_size(8 * 1024 * 1024)
                    .spawn_scoped(scope, || {
                        tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(2)
                            .enable_all()
                            .build()
                            .unwrap()
                            .block_on(f)
                    })
                    .unwrap()
                    .join()
                    .unwrap()
            })
        }

        fn ctx_with(mesh: MeshConfig, function_calling_support: bool) -> RequestContext {
            let mut app = AppState::test_default();
            app.config = Arc::new(AppConfig {
                mesh,
                function_calling_support,
                ..AppConfig::default()
            });
            RequestContext::new(Arc::new(app), WorkingMode::Repl)
        }

        fn off_ctx() -> RequestContext {
            RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl)
        }

        async fn run(ctx: &mut RequestContext, line: &str) -> Result<bool> {
            Box::pin(run_repl_command(ctx, create_abort_signal(), line)).await
        }

        fn err_of(ctx: &mut RequestContext, line: &str) -> String {
            run_async(run(ctx, line)).unwrap_err().to_string()
        }

        #[cfg(unix)]
        fn mesh_tool_names(ctx: &RequestContext) -> Vec<String> {
            ctx.tool_scope
                .functions
                .declarations()
                .iter()
                .map(|f| f.name.clone())
                .filter(|name| name.starts_with("mesh__"))
                .collect()
        }

        #[test]
        fn every_mesh_verb_is_refused_inside_a_macro() {
            let mut ctx = off_ctx();
            ctx.macro_flag = true;
            for (verb, _, _) in VERBS {
                let err = err_of(&mut ctx, &format!(".mesh {verb}"));
                assert_eq!(
                    err, "Cannot perform this operation because you are in a macro",
                    "{verb}"
                );
            }
        }

        #[test]
        #[serial]
        fn bare_info_while_off_puts_every_value_in_the_same_column() {
            let _capture = capture::install();
            let mut ctx = off_ctx();
            run_async(run(&mut ctx, ".mesh info")).unwrap();
            let out = stdout_lines().join("\n");
            let rows: Vec<&str> = out.lines().filter(|line| !line.trim().is_empty()).collect();
            assert!(rows.iter().any(|line| line.starts_with("  reach")), "{out}");
            assert!(rows.iter().any(|line| line.starts_with("  node")), "{out}");
            for line in rows {
                let indent = line.len() - line.trim_start().len();
                let label_end = indent + line[indent..].find(' ').unwrap();
                let padding = line[label_end..].len() - line[label_end..].trim_start().len();
                assert_eq!(
                    label_end + padding,
                    2 + MESH_INFO_LABEL_WIDTH,
                    "{line:?} is not aligned with the rest:\n{out}"
                );
            }
        }

        #[test]
        #[serial]
        fn bare_mesh_and_verb_help_never_error() {
            let _capture = capture::install();
            let _script = prompt_script::install(&[]);
            let mut ctx = off_ctx();
            for line in [
                ".mesh",
                ".mesh brief set",
                ".mesh brief nonsense",
                ".mesh info",
                ".mesh answer",
                ".mesh reply",
                ".mesh broadcast",
                ".mesh reply 3f9c2a7b1d4e6f80a1b2c3d4e5f60718",
                ".mesh trust",
                ".mesh untrust",
                ".mesh forget",
                ".mesh block",
                ".mesh unblock",
                ".mesh knock",
                ".mesh allow",
                ".mesh deny",
                ".mesh unshare",
            ] {
                run_async(run(&mut ctx, line)).unwrap_or_else(|err| panic!("{line}: {err}"));
            }
            let out = stdout_lines().join("\n");
            for example in [
                ".mesh answer <id>",
                ".mesh reply <destination>",
                ".mesh broadcast",
                ".mesh trust <destination>",
                ".mesh untrust <destination>",
                ".mesh forget <destination>",
                ".mesh block <identity>",
                ".mesh unblock <identity>",
                ".mesh sync",
                ".mesh knock <destination>",
                ".mesh allow docs/**",
                ".mesh deny \"src/vault/*\"",
                ".mesh unshare docs/**",
            ] {
                assert!(out.contains(example), "{example} missing from {out}");
            }
            assert_eq!(prompt_script::prompts_asked(), 0);
            let err = err_of(&mut ctx, ".mesh bogus");
            assert!(err.contains(".mesh"), "{err}");

            run_async(run(&mut ctx, ".mesh status \"focus\"")).unwrap();
            assert_eq!(
                ctx.app
                    .mesh
                    .objective_override()
                    .as_deref()
                    .map(String::as_str),
                Some("focus")
            );
            run_async(run(&mut ctx, ".mesh status clear")).unwrap();
            assert!(ctx.app.mesh.objective_override().is_none());
        }

        /// The mesh-off refusal comes from the store's own text and lands before any
        /// prompt, so an unattended `.mesh trust` never hangs on a question it cannot act on.
        #[test]
        #[serial]
        fn trust_mutations_are_refused_while_the_mesh_is_off_before_any_prompt() {
            let _script = prompt_script::install(&[true; 8]);
            let mut ctx = off_ctx();
            let h = "ab".repeat(16);
            for line in [
                format!(".mesh trust {h}"),
                format!(".mesh trust --identity {h}"),
                ".mesh trust --prune".to_string(),
                format!(".mesh untrust {h}"),
                format!(".mesh untrust --identity {h}"),
                format!(".mesh forget {h}"),
                format!(".mesh block {h}"),
                format!(".mesh unblock {h}"),
            ] {
                assert_eq!(
                    err_of(&mut ctx, &line),
                    crate::mesh::trust::MESH_OFF,
                    "{line}"
                );
            }
            assert_eq!(prompt_script::prompts_asked(), 0);
        }

        /// Usage probe: the two new node-required verbs refuse while the
        /// mesh is off with the SAME teaching text as every other node verb, before any
        /// prompt and before any progress line, so an unattended `.mesh sync` or
        /// `.mesh knock` never hangs or claims to be asking a node that is not there.
        #[test]
        #[serial]
        fn sync_and_knock_are_refused_while_the_mesh_is_off_before_any_prompt() {
            let _capture = capture::install();
            let _script = prompt_script::install(&[true; 4]);
            let mut ctx = off_ctx();
            let h = "ab".repeat(16);
            for line in [
                ".mesh sync".to_string(),
                format!(".mesh knock {h}"),
                format!(".mesh knock {h} --yes --intro \"hi\""),
            ] {
                assert_eq!(err_of(&mut ctx, &line), MESH_OFF, "{line}");
            }
            let out = stdout_lines();
            assert!(
                !out.iter().any(|line| {
                    line.starts_with("Asking the nearest propagation node")
                        || line.starts_with("This knocks on")
                        || line.starts_with("Knocking on")
                }),
                "no progress or consent line while off: {out:?}"
            );
            assert_eq!(prompt_script::prompts_asked(), 0);
        }

        /// Usage probe: `.mesh sync` takes no arguments, so a stray word or flag is a
        /// teaching error carrying the verb's usage line, like the other flagless verbs,
        /// and is refused before the node is consulted.
        #[test]
        #[serial]
        fn sync_with_arguments_is_a_usage_error_naming_the_verb() {
            let _capture = capture::install();
            let mut ctx = off_ctx();
            for line in [".mesh sync now", ".mesh sync --yes"] {
                let err = err_of(&mut ctx, line);
                assert!(err.starts_with("Unexpected '"), "{line}: {err}");
                assert!(err.contains(".mesh sync"), "{line}: {err}");
                assert_ne!(err, MESH_OFF, "{line}: the usage check comes first");
            }
            assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
        }

        /// Usage probe (TASK-100 (c)): "reports counts" means every counter of the report
        /// is on the line, with the node named by its short hash, so a human can tell
        /// listed-but-unwanted from received-but-deferred without reading logs.
        #[test]
        fn sync_report_line_carries_every_counter_and_the_short_node() {
            let node = "deadbeefdeadbeefdeadbeefdeadbeef".to_string();
            let report = FetchReport {
                node: node.clone(),
                listed: 7,
                wanted: 6,
                received: 5,
                delivered: 2,
                duplicates: 1,
                discarded: 1,
                deferred: 1,
                acknowledged: 5,
                response_branch: None,
            };
            let line = render_sync(&report);
            assert_eq!(
                line,
                "Fetched from deadbeef: 7 listed, 6 wanted, 5 received, 2 delivered, 1 duplicates, 1 discarded, 1 deferred."
            );
            assert!(
                !line.contains(&node),
                "the full node hash is not needed here"
            );
            let empty = FetchReport {
                listed: 0,
                wanted: 0,
                received: 0,
                ..report.clone()
            };
            assert_eq!(
                render_sync(&empty),
                "Nothing held for this node at deadbeef."
            );
            let none_served = FetchReport {
                listed: 7,
                wanted: 3,
                received: 0,
                ..report.clone()
            };
            assert_eq!(
                render_sync(&none_served),
                "deadbeef lists 7 messages for this node and 3 were asked for, but none were served; run .mesh sync again or check the node's logs."
            );
            let full_page_none_served = FetchReport {
                listed: 100,
                wanted: MAX_WANTS_PER_FETCH,
                received: 0,
                ..report.clone()
            };
            assert!(
                render_sync(&full_page_none_served)
                    .ends_with("check the node's logs. More may be held.")
            );
            let all_known = FetchReport {
                wanted: 0,
                received: 0,
                ..report.clone()
            };
            assert_eq!(
                render_sync(&all_known),
                "Nothing new held for this node at deadbeef: 7 listed, all already processed."
            );
            let full_page = FetchReport {
                listed: 100,
                wanted: MAX_WANTS_PER_FETCH,
                received: MAX_WANTS_PER_FETCH,
                ..report
            };
            assert_eq!(
                render_sync(&full_page),
                "Fetched from deadbeef: 100 listed, 64 wanted, 64 received, 2 delivered, 1 duplicates, 1 discarded, 1 deferred. More may be held; run .mesh sync again."
            );
        }

        #[test]
        #[serial]
        fn bare_mesh_brief_renders_the_served_brief() {
            use crate::mesh::test_support::snapshot_fixture;

            let _capture = capture::install();
            let mut ctx = off_ctx();
            ctx.app.mesh.publish(snapshot_fixture());
            ctx.app.mesh.set_user_brief(Some("note".to_string()));
            let served = ctx
                .app
                .mesh
                .brief()
                .expect("a snapshot and a user brief serve one");

            run_async(run(&mut ctx, ".mesh brief")).unwrap();

            let out = stdout_lines().join("\n");
            assert!(out.contains("brief mode:"), "{out}");
            assert!(out.contains(served.render_for_human()), "{out}");
        }

        #[test]
        #[serial]
        fn mesh_off_teaching_error_names_the_enabling_command() {
            let _capture = capture::install();
            let mut ctx = off_ctx();
            let hash = "ab".repeat(16);
            // `.mesh rotate` is gated the other way round and has its own tests.
            for line in [
                ".mesh peers".to_string(),
                ".mesh knocks".to_string(),
                ".mesh status".to_string(),
                format!(".mesh status {hash}"),
                format!(".mesh info {hash}"),
                ".mesh pending".to_string(),
                ".mesh answer q1 text".to_string(),
                format!(".mesh reply {hash} text"),
                ".mesh broadcast text".to_string(),
                ".mesh sync".to_string(),
                format!(".mesh knock {hash}"),
            ] {
                let err = err_of(&mut ctx, &line);
                assert!(err.contains(".mesh on"), "{line}: {err}");
            }
            // The inbox outlives the node: messages filed before `.mesh off` stay readable.
            run_async(run(&mut ctx, ".mesh inbox")).unwrap();
            assert!(
                stdout_lines().iter().any(|line| line == "Inbox is empty."),
                "{:?}",
                stdout_lines()
            );
            run_async(run(&mut ctx, ".mesh off")).unwrap();
            run_async(run(&mut ctx, ".mesh off --yes")).unwrap();
        }

        /// `.mesh off` is the session's decision even when nothing is running: a config
        /// loaded with `enabled: true` is overridden for the session, and nothing is
        /// written to disk.
        #[test]
        #[serial]
        fn mesh_off_overrides_an_enabled_config_even_when_no_node_runs() {
            let guard = TestConfigDirGuard::new("repl-mesh-off-enabled-config");
            let _capture = capture::install();
            let mesh = MeshConfig {
                enabled: true,
                ..MeshConfig::default()
            };
            let mut ctx = ctx_with(mesh, true);
            assert!(ctx.app.config.mesh.enabled);
            assert!(ctx.app.mesh.get().is_none());

            run_async(run(&mut ctx, ".mesh off")).unwrap();

            assert!(!ctx.app.config.mesh.enabled);
            assert!(!guard.path.join("config.yaml").exists());
            assert!(
                stdout_lines()
                    .iter()
                    .any(|line| line == "Mesh is already off for this session."),
                "{:?}",
                stdout_lines()
            );
        }

        #[test]
        #[serial]
        fn set_mesh_enabled_for_session_overrides_the_loaded_config_without_touching_disk() {
            let guard = TestConfigDirGuard::new("repl-mesh-session-override");
            let mut ctx = off_ctx();
            assert!(!ctx.app.config.mesh.enabled);
            let slot = Arc::clone(&ctx.app.mesh);

            ctx.set_mesh_enabled_for_session(true);

            assert!(ctx.app.config.mesh.enabled);
            assert!(Arc::ptr_eq(&slot, &ctx.app.mesh));
            assert!(!guard.path.join("config.yaml").exists());
        }

        #[test]
        #[serial]
        fn mesh_on_without_a_tty_and_without_yes_refuses_before_any_effect() {
            let _script = prompt_script::install_non_interactive();
            let mesh = MeshConfig {
                interfaces: vec![MeshInterface::Lan],
                ..MeshConfig::default()
            };
            let mut ctx = ctx_with(mesh, true);
            ctx.session = Some(Session::default());
            let err = err_of(&mut ctx, ".mesh on");
            assert!(err.contains("--yes"), "{err}");
            assert!(ctx.app.mesh.get().is_none());
            assert!(!ctx.app.config.mesh.enabled);
        }

        #[test]
        #[serial]
        fn mesh_on_declined_at_the_prompt_starts_nothing() {
            let _script = prompt_script::install(&[false]);
            let _capture = capture::install();
            let mesh = MeshConfig {
                interfaces: vec![MeshInterface::Lan],
                ..MeshConfig::default()
            };
            let mut ctx = ctx_with(mesh, true);
            ctx.session = Some(Session::default());

            run_async(run(&mut ctx, ".mesh on")).unwrap();

            assert!(ctx.app.mesh.get().is_none());
            assert!(!ctx.app.config.mesh.enabled);
            assert_eq!(prompt_script::prompts_asked(), 1);
            let out = stdout_lines();
            let preview = index_of(&out, "What leaves this machine");
            let declined = index_of(&out, "Mesh stays off");
            assert!(preview < declined, "{out:?}");
        }

        /// The start runs on a copy of the session, so a refused start leaves the id it
        /// was minted for untouched and the session clean.
        #[test]
        fn mesh_on_with_fresh_keeps_the_old_id_when_the_start_is_refused() {
            let id = "0123456789abcdef".repeat(2);
            let session: Session = serde_yaml::from_str(&format!(
                "model: provider:test\nmessages: []\nmesh_instance_id: {id}"
            ))
            .unwrap();
            assert!(!session.dirty());
            let mut ctx = ctx_with(MeshConfig::default(), false);
            ctx.session = Some(session);

            let err = err_of(&mut ctx, ".mesh on --fresh --yes");

            assert!(err.contains("function_calling_support"), "{err}");
            let session = ctx.session.as_ref().unwrap();
            assert_eq!(session.mesh_instance_id(), Some(id.as_str()));
            assert!(!session.dirty());
            assert!(ctx.app.mesh.get().is_none());
        }

        /// Spec-first usage probe: the louder `type: public` confirmation must reach the
        /// human on the command path, not just exist as a rendering. Without a terminal the
        /// refusal carries the question that would have been asked, so the public wording
        /// (`WORLD-VISIBLE`) is visible there and absent for a LAN-only node; declined at
        /// the prompt from `$HOME`, the stdout preview names the world-visible reach before
        /// the decline, the `$HOME` warning lands on stderr (never stdout), and nothing
        /// starts. From a project directory the same command warns about nothing.
        #[cfg(unix)]
        #[test]
        #[serial]
        fn public_on_confirmation_reaches_the_human_and_a_home_cwd_warns_on_stderr() {
            use crate::testing::EnvVarGuard;

            let public = || {
                let mut ctx = ctx_with(super::public_config(), true);
                ctx.session = Some(Session::default());
                ctx
            };
            let lan = || {
                let mesh = MeshConfig {
                    interfaces: vec![MeshInterface::Lan],
                    ..MeshConfig::default()
                };
                let mut ctx = ctx_with(mesh, true);
                ctx.session = Some(Session::default());
                ctx
            };

            {
                let _script = prompt_script::install_non_interactive();
                let mut ctx = public();
                let err = err_of(&mut ctx, ".mesh on");
                assert!(
                    err.contains("WORLD-VISIBLE"),
                    "public wording missing: {err}"
                );
                assert!(err.contains("--yes"), "{err}");
                assert!(ctx.app.mesh.get().is_none());
                assert!(!ctx.app.config.mesh.enabled);

                let mut ctx = lan();
                let err = err_of(&mut ctx, ".mesh on");
                assert!(!err.contains("WORLD-VISIBLE"), "lan must not shout: {err}");
                assert!(err.contains("--yes"), "{err}");
                assert!(ctx.app.mesh.get().is_none());
            }

            let cwd = env::current_dir().unwrap();
            let parent = cwd
                .parent()
                .expect("cargo test does not run from a filesystem root")
                .to_path_buf();
            let warning_marker = "this session's working directory is";
            {
                let _home = EnvVarGuard::set("HOME", &cwd);
                let _script = prompt_script::install(&[false]);
                let _capture = capture::install();
                let mut ctx = public();

                run_async(run(&mut ctx, ".mesh on")).unwrap();

                assert_eq!(prompt_script::prompts_asked(), 1);
                assert!(ctx.app.mesh.get().is_none(), "declined must start nothing");
                assert!(!ctx.app.config.mesh.enabled);
                let out = stdout_lines();
                let preview = index_of(&out, "world-visible");
                let declined = index_of(&out, "Mesh stays off");
                assert!(preview < declined, "{out:?}");
                assert!(
                    !out.iter().any(|line| line.contains(warning_marker)),
                    "the cwd warning must not go to stdout: {out:?}"
                );
                let warned = stderr_lines();
                assert!(
                    warned.iter().any(|line| {
                        line.contains(warning_marker) && line.contains(&cwd.display().to_string())
                    }),
                    "a `$HOME` cwd warns on stderr naming the directory: {warned:?}"
                );
            }
            {
                let _home = EnvVarGuard::set("HOME", &parent);
                let _script = prompt_script::install(&[false]);
                let _capture = capture::install();
                let mut ctx = lan();

                run_async(run(&mut ctx, ".mesh on")).unwrap();

                assert_eq!(prompt_script::prompts_asked(), 1);
                assert!(
                    stderr_lines().is_empty(),
                    "a project directory warns about nothing: {:?}",
                    stderr_lines()
                );
                index_of(&stdout_lines(), "Mesh stays off");
            }
        }

        /// Spec-first usage probe: `.mesh inbox` works with the mesh off, renders the FULL
        /// sanitised content of every filed message (control sequences stripped, nothing
        /// truncated below the peer content cap), drains what it rendered, and reports
        /// messages dropped past the inbox cap as a stderr warning, never in the results.
        #[test]
        #[serial]
        fn inbox_with_the_mesh_off_renders_full_sanitised_content_then_drains() {
            use crate::mesh::message::{PEER_INBOX_CAPACITY, RawPeerMessage};

            let raw = |content: String, id: &str| {
                PeerMessage::new(RawPeerMessage {
                    source_identity: "cd".repeat(16),
                    source_destination: "ab".repeat(16),
                    destination: "01".repeat(16),
                    title: None,
                    content,
                    fields: None,
                    timestamp: 0.0,
                    message_id: id.repeat(16),
                    in_reply_to: None,
                    kind: PeerKind::Message,
                    via: PeerVia::Direct,
                    thread: None,
                    disposition: None,
                    retry_after: None,
                    parts: Vec::new(),
                    dropped_parts: 0,
                })
            };
            let _capture = capture::install();
            let mut ctx = off_ctx();
            assert!(ctx.app.mesh.get().is_none());
            let head = "a".repeat(900);
            let tail = "b".repeat(900);
            ctx.app
                .mesh
                .peer_inbox()
                .deliver(raw(format!("{head}\x1b[31m\u{7}\r\n{tail}"), "m1"));

            run_async(run(&mut ctx, ".mesh inbox")).unwrap();

            let out = stdout_lines().join("\n");
            assert!(out.contains("[message] from"), "{out}");
            assert!(out.contains(&head), "the head is rendered in full: {out}");
            assert!(out.contains(&tail), "the tail is rendered in full: {out}");
            for control in ['\x1b', '\u{7}', '\r'] {
                assert!(
                    !out.contains(control),
                    "{control:?} leaked into `.mesh inbox`: {out:?}"
                );
            }
            assert!(!out.contains("[31m"), "{out}");
            assert!(stderr_lines().is_empty(), "{:?}", stderr_lines());

            let before = stdout_lines().len();
            run_async(run(&mut ctx, ".mesh inbox")).unwrap();
            assert_eq!(
                stdout_lines()[before..],
                ["Inbox is empty.".to_string()],
                "a drained inbox renders nothing twice"
            );

            for i in 0..=PEER_INBOX_CAPACITY {
                ctx.app
                    .mesh
                    .peer_inbox()
                    .deliver(raw(format!("note {i}"), "m2"));
            }
            let before = stdout_lines().len();
            run_async(run(&mut ctx, ".mesh inbox")).unwrap();
            let rows: Vec<String> = stdout_lines()[before..]
                .iter()
                .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
                .collect();
            assert_eq!(rows.len(), PEER_INBOX_CAPACITY, "{rows:?}");
            assert!(
                !rows.iter().any(|line| line.contains("dropped")),
                "the drop warning must not be in the results: {rows:?}"
            );
            let warned = stderr_lines();
            assert_eq!(warned.len(), 1, "{warned:?}");
            assert!(
                warned[0].contains("1 peer message(s) were dropped"),
                "{warned:?}"
            );
        }

        fn stdout_lines() -> Vec<String> {
            capture::lines()
                .into_iter()
                .filter(|(stream, _)| *stream == capture::Stream::Out)
                .map(|(_, text)| text)
                .collect()
        }

        fn stderr_lines() -> Vec<String> {
            capture::lines()
                .into_iter()
                .filter(|(stream, _)| *stream == capture::Stream::Err)
                .map(|(_, text)| text)
                .collect()
        }

        fn index_of(lines: &[String], needle: &str) -> usize {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not printed: {lines:?}"))
        }

        #[test]
        fn mesh_on_refuses_without_function_calling() {
            let mut ctx = ctx_with(MeshConfig::default(), false);
            ctx.session = Some(Session::default());
            let err = err_of(&mut ctx, ".mesh on --yes");
            assert!(err.contains("function_calling_support"), "{err}");
            assert!(ctx.app.mesh.get().is_none());
            assert!(!ctx.app.config.mesh.enabled);
        }

        #[test]
        fn mesh_on_refuses_without_a_session() {
            let mut ctx = ctx_with(MeshConfig::default(), true);
            let err = err_of(&mut ctx, ".mesh on --yes");
            assert!(err.contains(".session"), "{err}");
            assert!(ctx.app.mesh.get().is_none());
            assert!(!ctx.app.config.mesh.enabled);
        }

        // `autostart` prints its leading notice through `out_text`, which lands in the
        // process-global `capture` when another `#[serial]` test has one installed, so this
        // test must not overlap them (it broke `bare_info_while_off_puts_every_value_in_the_same_column`).
        #[test]
        #[serial]
        fn autostart_without_a_session_prints_the_mesh_on_refusal_and_stays_off() {
            let enabled = MeshConfig {
                enabled: true,
                ..MeshConfig::default()
            };
            let mut ctx = ctx_with(enabled.clone(), true);
            let err = run_async(autostart(&mut ctx)).unwrap_err().to_string();
            assert_eq!(
                err,
                "Mesh needs a session: this node's destination is derived from an id kept in the session file. Run `.session <name>` first."
            );
            assert!(ctx.app.mesh.get().is_none());

            let mut by_command = ctx_with(enabled, true);
            assert_eq!(err_of(&mut by_command, ".mesh on --yes"), err);
            assert!(by_command.app.mesh.get().is_none());
        }

        /// Criterion (b): an unmatched `.mesh` verb is refused by the mesh family itself
        /// and never falls through to the macro path's generic "Unknown command".
        #[test]
        fn an_unknown_mesh_verb_is_refused_by_mesh_itself_not_the_macro_path() {
            let mut ctx = off_ctx();
            let err = err_of(&mut ctx, ".mesh bogus");
            assert!(err.starts_with("Unknown .mesh command 'bogus'"), "{err}");
            assert!(err.contains("`.mesh`"), "{err}");
            assert!(!err.contains("Unknown command. Type"), "{err}");
            assert!(!err.to_lowercase().contains("macro"), "{err}");
        }

        /// `.mesh brief auto|manual|off` flips the per-session mode and `set`/`clear`
        /// drive `set_user_brief`, all in memory: config.yaml is never written.
        #[test]
        #[serial]
        fn brief_mode_and_text_are_session_scoped_and_never_written_to_disk() {
            let guard = TestConfigDirGuard::new("repl-mesh-brief");
            let mut ctx = off_ctx();
            assert_eq!(ctx.app.config.mesh.brief, MeshBrief::Auto);
            let slot = Arc::clone(&ctx.app.mesh);

            run_async(run(&mut ctx, ".mesh brief manual")).unwrap();
            assert_eq!(ctx.app.config.mesh.brief, MeshBrief::Manual);
            run_async(run(&mut ctx, ".mesh brief off")).unwrap();
            assert_eq!(ctx.app.config.mesh.brief, MeshBrief::Off);
            run_async(run(&mut ctx, ".mesh brief auto")).unwrap();
            assert_eq!(ctx.app.config.mesh.brief, MeshBrief::Auto);

            run_async(run(&mut ctx, ".mesh brief set \"we ship on Friday\"")).unwrap();
            assert_eq!(
                ctx.app.mesh.user_brief().as_deref().map(String::as_str),
                Some("we ship on Friday")
            );
            run_async(run(&mut ctx, ".mesh brief")).unwrap();
            run_async(run(&mut ctx, ".mesh brief clear")).unwrap();
            assert!(ctx.app.mesh.user_brief().is_none());

            assert!(
                Arc::ptr_eq(&slot, &ctx.app.mesh),
                "the slot survives the config swap"
            );
            assert!(!guard.path.join("config.yaml").exists());
        }

        #[cfg(unix)]
        fn minted_key(guard: &TestConfigDirGuard) -> (std::path::PathBuf, Vec<u8>, String) {
            let path = identity::identity_path();
            assert!(path.starts_with(&guard.path));
            let minted = identity::load_or_mint_identity(&path).unwrap();
            let bytes = fs::read(&path).unwrap();
            (path, bytes, fingerprint(&minted))
        }

        #[cfg(unix)]
        fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
            let mut out = Vec::new();
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    out.extend(files_under(&path));
                } else {
                    out.push(path);
                }
            }
            out
        }

        #[cfg(unix)]
        fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
            haystack
                .windows(needle.len())
                .any(|window| window == needle)
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn rotate_dry_run_prints_the_token_and_changes_nothing() {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-dry-run");
            let _capture = capture::install();
            let (path, before, old) = minted_key(&guard);
            let stale = path.with_added_extension("new");
            fs::write(&stale, [3u8; 64]).unwrap();
            let mut ctx = off_ctx();
            let token = format!("rotate-{}", short(&old));

            for line in [".mesh rotate", ".mesh rotate --dry-run"] {
                let printed = stdout_lines().len();
                run_async(run(&mut ctx, line)).unwrap();
                let out = stdout_lines()[printed..].join("\n");
                assert!(out.contains(&old), "{line}: {out}");
                assert!(out.contains("This was a dry run"), "{line}: {out}");
                assert!(out.contains("(0 recorded so far)"), "{line}: {out}");
                assert!(out.contains("sees a stranger"), "{line}: {out}");
                assert!(
                    out.contains("refused while any session's node on this config dir is running"),
                    "{line}: {out}"
                );
                assert!(out.contains("mesh/identity.key.lock"), "{line}: {out}");
                assert!(
                    out.contains(&format!(".mesh rotate --confirm {token}")),
                    "{line}: {out}"
                );
                assert_eq!(fs::read(&path).unwrap(), before, "{line}");
                assert!(!identity::predecessors_path(&path).exists(), "{line}");
                assert!(stale.exists(), "{line}: a dry run sweeps nothing");
            }
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn rotate_surfaces_a_refused_predecessors_file_and_changes_nothing() {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-unversioned-history");
            let (path, before, old) = minted_key(&guard);
            let history = identity::predecessors_path(&path);
            let unversioned = "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n";
            fs::write(&history, unversioned).unwrap();
            let mut ctx = off_ctx();
            let token = format!("rotate-{}", short(&old));

            for line in [
                ".mesh rotate".to_string(),
                ".mesh rotate --dry-run".to_string(),
                format!(".mesh rotate --confirm {token}"),
            ] {
                let err = err_of(&mut ctx, &line);
                assert!(err.contains("no readable `version` field"), "{line}: {err}");
                assert!(err.contains("move the file aside"), "{line}: {err}");
                assert!(
                    err.contains(&history.display().to_string()),
                    "{line}: {err}"
                );
                assert_eq!(fs::read(&path).unwrap(), before, "{line}");
                assert_eq!(fs::read_to_string(&history).unwrap(), unversioned, "{line}");
                assert!(!path.with_added_extension("new").exists(), "{line}");
            }
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn rotate_is_refused_while_another_process_holds_the_identity_lock() {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-locked");
            let (path, before, old) = minted_key(&guard);
            let _node = identity::IdentityLock::share(&path).unwrap();
            let mut ctx = off_ctx();
            let token = format!("rotate-{}", short(&old));

            for line in [
                ".mesh rotate".to_string(),
                ".mesh rotate --dry-run".to_string(),
                format!(".mesh rotate --confirm {token}"),
            ] {
                let err = err_of(&mut ctx, &line);
                assert!(err.contains(".mesh off"), "{line}: {err}");
                assert!(
                    err.contains(&format!("pid {}", std::process::id())),
                    "{line}: {err}"
                );
                assert_eq!(fs::read(&path).unwrap(), before, "{line}");
                assert!(!identity::predecessors_path(&path).exists(), "{line}");
                assert!(!path.with_added_extension("new").exists(), "{line}");
            }
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn rotate_with_a_wrong_token_refuses_and_changes_nothing() {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-wrong-token");
            let (path, before, old) = minted_key(&guard);
            let mut ctx = off_ctx();

            let err = err_of(&mut ctx, ".mesh rotate --confirm rotate-nope");

            assert!(err.contains("'rotate-nope' is not the token"), "{err}");
            assert!(
                err.contains(&format!("--confirm rotate-{}", short(&old))),
                "{err}"
            );
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(!identity::predecessors_path(&path).exists());
        }

        /// The instance id lives in the session, not the key; rotating one leaves the
        /// other alone.
        #[cfg(unix)]
        #[test]
        #[serial]
        fn rotate_replaces_the_key_records_the_predecessor_and_leaves_no_trace_or_instance_change()
        {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-confirm");
            let _capture = capture::install();
            let (path, before, old) = minted_key(&guard);
            assert_eq!(
                before.len(),
                64,
                "the minted key is the 64-byte private key"
            );
            let mut ctx = off_ctx();
            let mut session = Session::default();
            let id = session.ensure_mesh_instance_id().to_string();
            ctx.session = Some(session);

            run_async(run(
                &mut ctx,
                &format!(".mesh rotate --confirm rotate-{}", short(&old)),
            ))
            .unwrap();

            let files = files_under(&guard.path.join("mesh"));
            assert!(!files.is_empty(), "{files:?}");
            for file in &files {
                assert!(
                    !contains_bytes(&fs::read(file).unwrap(), &before),
                    "old key material survives in {}",
                    file.display()
                );
            }
            let now = fingerprint(&identity::load_or_mint_identity(&path).unwrap());
            assert_ne!(now, old);
            let predecessors = identity::predecessors(&path).unwrap();
            assert_eq!(predecessors.len(), 1, "{predecessors:?}");
            assert_eq!(predecessors[0].identity_hash, old);
            assert_eq!(
                ctx.session.as_ref().unwrap().mesh_instance_id(),
                Some(id.as_str())
            );
            let out = stdout_lines().join("\n");
            assert!(
                out.contains(&format!("Rotated the mesh identity: {old} -> {now}.")),
                "{out}"
            );
            assert!(out.contains("Predecessors recorded: 1."), "{out}");
            assert!(out.contains(".mesh on"), "{out}");
            assert!(out.contains("peers must re-trust it"), "{out}");
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn rotate_refuses_yes_and_positional_and_dry_run_with_confirm() {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-flags");
            let (path, before, old) = minted_key(&guard);
            let mut ctx = off_ctx();
            let token = format!("rotate-{}", short(&old));

            let err = err_of(&mut ctx, ".mesh rotate --yes");
            assert_eq!(
                err,
                "`.mesh rotate` takes `--confirm rotate-<identity-short>` from a dry run, not `--yes`."
            );
            assert_eq!(fs::read(&path).unwrap(), before);

            for (line, offending) in [
                (format!(".mesh rotate {token}"), token.as_str()),
                (
                    format!(".mesh rotate --dry-run --confirm {token}"),
                    "--dry-run",
                ),
            ] {
                let err = err_of(&mut ctx, &line);
                assert!(
                    err.starts_with(&format!("Unexpected '{offending}'.")),
                    "{line}: {err}"
                );
                assert!(err.contains(".mesh rotate ["), "{line}: {err}");
                assert_eq!(fs::read(&path).unwrap(), before, "{line}");
            }
        }

        #[test]
        #[serial]
        fn rotate_with_no_identity_names_mesh_on() {
            let guard = TestConfigDirGuard::new("repl-mesh-rotate-no-identity");
            let mut ctx = off_ctx();

            let err = err_of(&mut ctx, ".mesh rotate");

            assert!(err.contains("No mesh identity"), "{err}");
            assert!(err.contains(".mesh on"), "{err}");
            assert!(!guard.path.join("mesh").exists());
        }

        #[cfg(unix)]
        mod with_a_node {
            use super::*;
            use crate::mesh::brief::Digest;
            use crate::mesh::envoy::{EnvoyJob, EnvoySink};
            use crate::mesh::hex_lower;
            use crate::mesh::idle::{IdleNotify, IdleSink, Origin};
            use crate::mesh::limits::{PeerRefusal, RefusalReason};
            use crate::mesh::notify::{
                Notification, NotificationSink, RenderedNotification, Source,
            };
            use crate::mesh::test_support::{
                FakeNode, PeerSighting, PeerStub, StartedRuntime, loopback_relay, private_config,
                started_runtime, started_runtime_on, wait_until,
            };
            use crate::mesh::trust::{LiveMesh, TrustOptions};
            use crate::testing::EnvVarGuard;
            use crate::utils::get_env_name;
            use parking_lot::Mutex;
            use rmpv::Value;
            use rns_transport::iface::tcp_server::TcpServer;
            use std::sync::atomic::{AtomicUsize, Ordering};

            /// (destination, identity, name hash) of a freshly minted peer. Trusting verifies
            /// identity + name hash -> destination, so the destination is derived for real.
            fn announced_peer() -> (String, String, String) {
                use crate::mesh::session_destination_name;
                use rand_core::OsRng;
                use rns_transport::destination::SingleInputDestination;
                use rns_transport::identity::PrivateIdentity;

                let name = session_destination_name("probe");
                let announced =
                    SingleInputDestination::new(PrivateIdentity::new_from_rand(OsRng), name);
                (
                    announced.desc.address_hash.to_hex_string(),
                    announced.desc.identity.address_hash.to_hex_string(),
                    hex_lower(name.as_name_hash_slice()),
                )
            }

            /// Puts a heard peer in `runtime`'s peer table, last seen `at`, and returns its
            /// (destination, identity); nothing is trusted.
            fn heard_peer(runtime: &MeshRuntime, name: &str, at: SystemTime) -> (String, String) {
                let (destination, identity, name_hash) = announced_peer();
                runtime.peers().observe(
                    PeerSighting {
                        destination_hash: destination.clone(),
                        identity_hash: identity.clone(),
                        name_hash,
                        display_name: Some(name.to_string()),
                        protocol_version: 1,
                        hops: 1,
                    },
                    at,
                );
                (destination, identity)
            }

            /// Puts a heard, trusted peer named "Tia" in `runtime`'s peer table and returns
            /// its destination hash.
            fn heard_trusted_peer(runtime: &MeshRuntime, slot: &dyn LiveMesh) -> String {
                let now = SystemTime::now();
                let (hash, _) = heard_peer(runtime, "Tia", now);
                runtime
                    .trust()
                    .trust_destination(slot, &hash, TrustOptions::default(), now)
                    .unwrap();
                hash
            }

            #[derive(Default)]
            struct Recording(Mutex<Vec<String>>);

            impl IdleSink for Recording {
                fn push(&self, note: IdleNotify) -> std::result::Result<(), IdleNotify> {
                    self.0.lock().push(note.text);
                    Ok(())
                }

                fn request_sync(&self) {}
            }

            impl NotificationSink for Recording {
                fn notify(&self, rendered: RenderedNotification) {
                    self.0.lock().extend(rendered.lines().iter().cloned());
                }
            }

            /// An envoy that takes nothing and only counts how often it is interrupted.
            #[derive(Default)]
            struct BusyEnvoy {
                interrupts: AtomicUsize,
            }

            impl EnvoySink for BusyEnvoy {
                fn accept(&self, _job: EnvoyJob) -> std::result::Result<(), PeerRefusal> {
                    Err(PeerRefusal::capacity(RefusalReason::EnvoyBusy))
                }

                fn answer(&self, _id: &str, _text: &str) -> bool {
                    false
                }

                fn interrupt(&self) {
                    self.interrupts.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            #[serial]
            fn mesh_off_leaves_the_idle_and_notifier_hooks_in_place() {
                let _guard = TestConfigDirGuard::new("repl-mesh-off");
                run_async(async {
                    let started = started_runtime("repl-mesh-off").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let idle = Arc::new(Recording::default());
                    let notifier = Arc::new(Recording::default());
                    let envoy = Arc::new(BusyEnvoy::default());
                    ctx.app.mesh.set_idle(idle.clone());
                    ctx.app.mesh.set_notifier(notifier.clone());
                    ctx.app.mesh.set_envoy(envoy.clone());

                    run(&mut ctx, ".mesh off --yes").await.unwrap();

                    assert!(ctx.app.mesh.get().is_none());
                    assert_eq!(
                        envoy.interrupts.load(Ordering::SeqCst),
                        1,
                        "stopping the node cuts the envoy's run short exactly once"
                    );
                    assert!(
                        ctx.app.mesh.envoy_attached(),
                        "the envoy hook outlives the node"
                    );
                    assert!(ctx.app.mesh.push_idle(IdleNotify {
                        source: Source::Mesh,
                        text: "still routed".to_string(),
                        origin: Origin::Local,
                        model_note: None,
                    }));
                    assert_eq!(idle.0.lock().as_slice(), ["still routed"]);
                    ctx.app
                        .mesh
                        .notify(Notification::new(Source::Mesh, "still printed"));
                    let printed = notifier.0.lock();
                    assert_eq!(printed.len(), 1, "{printed:?}");
                    assert!(printed[0].contains("still printed"), "{printed:?}");
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn mesh_on_and_off_flip_the_session_config_and_the_tool_catalog() {
                let guard = TestConfigDirGuard::new("repl-mesh-on");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), guard.path.join("cache"));
                run_async(async {
                    let (addr, relay_handle, _) = loopback_relay().await;
                    let mut ctx = ctx_with(private_config(addr.port()), true);
                    ctx.session = Some(Session::default());
                    ctx.refresh_tool_scope(create_abort_signal()).await.unwrap();
                    assert!(mesh_tool_names(&ctx).is_empty());
                    let idle = Arc::new(Recording::default());
                    let notifier = Arc::new(Recording::default());
                    let envoy = Arc::new(BusyEnvoy::default());
                    ctx.app.mesh.set_idle(idle.clone());
                    ctx.app.mesh.set_notifier(notifier.clone());
                    ctx.app.mesh.set_envoy(envoy.clone());

                    run(&mut ctx, ".mesh on --yes").await.unwrap();

                    assert!(ctx.app.mesh.get().is_some());
                    assert!(ctx.app.config.mesh.enabled);
                    assert!(ctx.app.mesh.push_idle(IdleNotify {
                        source: Source::Mesh,
                        text: "routed after on".to_string(),
                        origin: Origin::Local,
                        model_note: None,
                    }));
                    assert!(
                        idle.0.lock().contains(&"routed after on".to_string()),
                        "the idle hook attached before `.mesh on` still receives events"
                    );
                    ctx.app
                        .mesh
                        .notify(Notification::new(Source::Mesh, "printed after on"));
                    assert!(
                        notifier
                            .0
                            .lock()
                            .iter()
                            .any(|line| line.contains("printed after on")),
                        "the notifier attached before `.mesh on` still prints"
                    );
                    assert!(
                        ctx.app.mesh.envoy_attached(),
                        "the envoy attached before `.mesh on` is still attached"
                    );
                    assert_eq!(
                        mesh_tool_names(&ctx),
                        [
                            "mesh__peers",
                            "mesh__send",
                            "mesh__ask",
                            "mesh__collect",
                            "mesh__check_inbox",
                            "mesh__broadcast",
                            "mesh__list",
                            "mesh__fetch",
                            "mesh__request_access",
                        ]
                    );
                    let slot = Arc::clone(&ctx.app.mesh);

                    let err = run(&mut ctx, ".mesh on --yes")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains(".mesh off"), "{err}");

                    slot.publish_digest(Some(Digest {
                        text: "- Working on the widget".into(),
                        generated_at: SystemTime::now(),
                        covered_messages: 3,
                    }));
                    assert!(slot.digest().is_some());

                    run(&mut ctx, ".mesh off --yes").await.unwrap();

                    assert!(slot.get().is_none());
                    assert!(
                        slot.digest().is_none(),
                        "a served digest must not outlive the node"
                    );
                    assert!(!ctx.app.config.mesh.enabled);
                    assert!(mesh_tool_names(&ctx).is_empty());
                    assert!(Arc::ptr_eq(&slot, &ctx.app.mesh));

                    let old_id = ctx
                        .session
                        .as_ref()
                        .unwrap()
                        .mesh_instance_id()
                        .expect("the first start minted an id")
                        .to_string();
                    run(&mut ctx, ".mesh on --fresh --yes").await.unwrap();
                    let new_id = ctx
                        .session
                        .as_ref()
                        .unwrap()
                        .mesh_instance_id()
                        .unwrap()
                        .to_string();
                    assert_ne!(new_id, old_id);
                    assert_eq!(slot.get().unwrap().current_instance_id(), new_id);

                    run(&mut ctx, ".mesh off --yes").await.unwrap();
                    assert!(slot.get().is_none());
                    relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn autostart_joins_the_mesh_without_a_prompt() {
                let guard = TestConfigDirGuard::new("repl-mesh-autostart");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), guard.path.join("cache"));
                let _script = prompt_script::install_non_interactive();
                let _capture = capture::install();
                run_async(async {
                    let (addr, relay_handle, _) = loopback_relay().await;
                    let mut ctx = ctx_with(
                        MeshConfig {
                            enabled: true,
                            ..private_config(addr.port())
                        },
                        true,
                    );
                    ctx.session = Some(Session::default());

                    autostart(&mut ctx).await.unwrap();

                    assert!(ctx.app.mesh.get().is_some());
                    assert!(ctx.app.config.mesh.enabled);
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    let out = stdout_lines();
                    let notice = index_of(&out, "mesh.enabled is true in config.yaml");
                    let preview = index_of(&out, "What leaves this machine");
                    let summary = index_of(&out, "Mesh is on for this session");
                    assert!(notice < preview && preview < summary, "{out:?}");

                    run(&mut ctx, ".mesh off --yes").await.unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn autostart_when_the_mesh_is_already_on_is_the_mesh_on_refusal() {
                let _guard = TestConfigDirGuard::new("repl-mesh-autostart-already-on");
                run_async(async {
                    let started = started_runtime("repl-mesh-autostart-already-on").await;
                    let mut ctx = ctx_with(
                        MeshConfig {
                            enabled: true,
                            ..MeshConfig::default()
                        },
                        true,
                    );
                    ctx.session = Some(Session::default());
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();

                    let err = autostart(&mut ctx).await.unwrap_err().to_string();
                    assert_eq!(err, MESH_ALREADY_ON);
                    assert!(ctx.app.mesh.get().is_some());

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Criterion (i) end to end: the real `.mesh on` path leaves config.yaml
            /// unwritten; `.mesh status "<text>"|clear` drive the objective override; the
            /// read-only listings never error on a fresh node.
            #[test]
            #[serial]
            fn mesh_on_writes_nothing_to_disk_and_status_objective_is_session_scoped() {
                let guard = TestConfigDirGuard::new("repl-mesh-objective");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), guard.path.join("cache"));
                let _capture = capture::install();
                run_async(async {
                    let (addr, relay_handle, _) = loopback_relay().await;
                    let mut ctx = ctx_with(private_config(addr.port()), true);
                    ctx.session = Some(Session::default());

                    run(&mut ctx, ".mesh on --yes").await.unwrap();
                    assert!(ctx.app.mesh.get().is_some());
                    assert!(
                        !guard.path.join("config.yaml").exists(),
                        "`.mesh on` must not persist"
                    );

                    let before = stdout_lines().len();
                    run(&mut ctx, ".mesh status").await.unwrap();
                    let out: Vec<String> = stdout_lines()[before..]
                        .iter()
                        .flat_map(|text| text.lines().map(str::to_string))
                        .collect();
                    for field in ["name: ", "objective: ", "state: ", "served: "] {
                        assert!(
                            out.iter().any(|line| line.starts_with(field)),
                            "bare `.mesh status` shows the node's own card ({field}): {out:?}"
                        );
                    }
                    run(&mut ctx, ".mesh status \"ship it\"").await.unwrap();
                    assert_eq!(
                        ctx.app
                            .mesh
                            .objective_override()
                            .as_deref()
                            .map(String::as_str),
                        Some("ship it")
                    );
                    let before = stdout_lines().len();
                    run(&mut ctx, ".mesh status").await.unwrap();
                    let out: Vec<String> = stdout_lines()[before..]
                        .iter()
                        .flat_map(|text| text.lines().map(str::to_string))
                        .collect();
                    assert!(
                        out.iter().any(|line| line == "objective: ship it"),
                        "the override shows in the node's own card: {out:?}"
                    );
                    run(&mut ctx, ".mesh status clear").await.unwrap();
                    assert!(ctx.app.mesh.objective_override().is_none());

                    for line in [
                        ".mesh peers",
                        ".mesh knocks",
                        ".mesh info",
                        ".mesh pending",
                        ".mesh inbox",
                        ".mesh brief",
                    ] {
                        run(&mut ctx, line)
                            .await
                            .unwrap_or_else(|err| panic!("{line}: {err}"));
                    }

                    run(&mut ctx, ".mesh off --yes").await.unwrap();
                    assert!(!guard.path.join("config.yaml").exists());
                    relay_handle.abort();
                });
            }

            /// Criterion (e) with the mesh on: a verb missing its arguments prints help and
            /// takes no action. Under `cargo test` stdin is not a terminal, so any attempt to
            /// send would have failed naming `--yes`; `Ok` is the proof that nothing was tried.
            /// Unknown targets fail with the remedy named.
            #[test]
            #[serial]
            fn missing_arguments_print_help_and_unknown_targets_name_the_remedy_while_on() {
                let _guard = TestConfigDirGuard::new("repl-mesh-help-on");
                let _script = prompt_script::install_non_interactive();
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-help-on").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let hash = "ab".repeat(16);

                    for line in [
                        ".mesh reply".to_string(),
                        format!(".mesh reply {hash}"),
                        format!(".mesh reply {hash} \"\""),
                        ".mesh answer".to_string(),
                        ".mesh answer q1".to_string(),
                        ".mesh broadcast".to_string(),
                        ".mesh broadcast --yes".to_string(),
                        ".mesh status nonsense".to_string(),
                        ".mesh brief set".to_string(),
                    ] {
                        run(&mut ctx, &line)
                            .await
                            .unwrap_or_else(|err| panic!("{line}: {err}"));
                    }
                    assert!(ctx.app.mesh.correlations().list().is_empty());
                    let out = stdout_lines().join("\n");
                    for usage in [
                        "Usage: .mesh reply <destination>",
                        "Usage: .mesh answer <id>",
                        "Usage: .mesh broadcast",
                    ] {
                        assert!(out.contains(usage), "{usage} missing from {out}");
                    }

                    let err = run(&mut ctx, &format!(".mesh info {hash}"))
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(
                        err.contains(&hash) && err.contains("not been heard"),
                        "{err}"
                    );
                    assert!(
                        err.contains(".mesh peers") && err.contains(".mesh knocks"),
                        "{err}"
                    );

                    let err = run(&mut ctx, ".mesh info nothex")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("32 hex"), "{err}");

                    let err = run(&mut ctx, ".mesh reply nothex --yes hi")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("32 hex"), "{err}");

                    let err = run(&mut ctx, ".mesh answer nope \"yes\"")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("No open question has id nope"), "{err}");
                    assert!(err.contains(".mesh pending"), "{err}");

                    let err = run(&mut ctx, &format!(".mesh status {hash}"))
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains(&hash) && err.contains(".mesh peers"), "{err}");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn bare_info_with_a_node_lists_instance_destination_interfaces_and_reach() {
                let _guard = TestConfigDirGuard::new("repl-mesh-info-node");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-info-node").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let runtime = started.runtime.clone();

                    run(&mut ctx, ".mesh info").await.unwrap();

                    let out = stdout_lines().join("\n");
                    let row = |name: &str| {
                        let head = format!("  {name:<MESH_INFO_LABEL_WIDTH$}");
                        out.lines()
                            .find_map(|line| line.strip_prefix(&head))
                            .unwrap_or_else(|| panic!("no {name} row in {out}"))
                            .to_string()
                    };
                    assert_eq!(row("node"), "on", "{out}");
                    assert_eq!(row("instance"), runtime.current_instance_id(), "{out}");
                    assert_eq!(
                        row("destination"),
                        runtime.current_destination_hash(),
                        "{out}"
                    );
                    let joined = row("joined");
                    for interface in runtime.interfaces() {
                        assert!(joined.contains(&interface), "{interface} missing: {out}");
                    }
                    assert_eq!(row("reach"), reach_line(&ctx.app.config.mesh), "{out}");
                    assert!(out.contains("selection: nearest by hops"), "{out}");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The row named `name` of the last `.mesh info` printed.
            fn info_row(out: &str, name: &str) -> String {
                let head = format!("  {name:<MESH_INFO_LABEL_WIDTH$}");
                out.lines()
                    .rev()
                    .find_map(|line| line.strip_prefix(&head))
                    .unwrap_or_else(|| panic!("no {name} row in {out}"))
                    .to_string()
            }

            #[test]
            #[serial]
            fn rotate_is_refused_while_the_mesh_is_on_and_names_mesh_off() {
                let guard = TestConfigDirGuard::new("repl-mesh-rotate-while-on");
                run_async(async {
                    let started = started_runtime("repl-mesh-rotate-while-on").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let path = identity::identity_path();
                    let minted = identity::load_or_mint_identity(&path).unwrap();
                    let before = fs::read(&path).unwrap();
                    let token = format!("rotate-{}", short(&fingerprint(&minted)));

                    for line in [
                        ".mesh rotate".to_string(),
                        format!(".mesh rotate --confirm {token}"),
                    ] {
                        let err = refusal(&mut ctx, &line).await;
                        assert_eq!(err, ROTATE_NEEDS_OFF, "{line}");
                        assert!(err.contains(".mesh off"), "{line}: {err}");
                    }
                    assert_eq!(fs::read(&path).unwrap(), before);
                    assert!(!identity::predecessors_path(&path).exists());
                    assert!(path.starts_with(&guard.path));

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn node_facts_show_predecessors_and_key_changes() {
                let _guard = TestConfigDirGuard::new("repl-mesh-info-predecessors");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-info-predecessors").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let runtime = started.runtime.clone();

                    let out = out_of(&mut ctx, ".mesh info").await.unwrap();
                    assert_eq!(info_row(&out, "identity predecessors"), "none", "{out}");
                    assert_eq!(info_row(&out, "key changes"), "none", "{out}");

                    let path = identity::identity_path();
                    let retired = fingerprint(&identity::load_or_mint_identity(&path).unwrap());
                    identity::rotate_identity(&path, &retired, SystemTime::now()).unwrap();
                    let old_dest = heard_trusted_peer(&runtime, ctx.app.mesh.as_ref());
                    let (_, new_identity) = heard_peer(&runtime, "Tia again", SystemTime::now());
                    let name_hash = runtime.peers().get(&old_dest).unwrap().name_hash;
                    assert_eq!(
                        runtime
                            .trust()
                            .note_key_change(&new_identity, &name_hash, SystemTime::now())
                            .len(),
                        1
                    );

                    let out = out_of(&mut ctx, ".mesh info").await.unwrap();
                    assert_eq!(
                        info_row(&out, "identity predecessors"),
                        format!("1 (latest {} rotated 0s ago)", short(&retired)),
                        "{out}"
                    );
                    assert_eq!(
                        info_row(&out, "key changes"),
                        "1 trusted instance(s) announced under another identity; see .mesh peers",
                        "{out}"
                    );

                    fs::write(identity::predecessors_path(&path), "not json\n").unwrap();
                    let out = out_of(&mut ctx, ".mesh info").await.unwrap();
                    let row = info_row(&out, "identity predecessors");
                    assert!(row.starts_with("unreadable: "), "{out}");
                    assert!(row.contains(identity::PREDECESSORS_FILE), "{out}");
                    assert!(
                        row.contains(&crate::mesh::schema::unversioned_cause(
                            identity::PREDECESSOR_RECORD_VERSION
                        )),
                        "{out}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The peer table's marker and the trust verb's naming of the superseded
            /// record are two views of one mark: trusting the instance under its new key
            /// clears it.
            #[test]
            #[serial]
            fn trusting_the_new_key_of_a_known_instance_names_the_superseded_record() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-superseded");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-superseded").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let runtime = started.runtime.clone();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let old_dest = heard_trusted_peer(&runtime, slot);
                    let old_identity = identity_of(&runtime.trust(), &old_dest);
                    let (new_dest, new_identity) =
                        heard_peer(&runtime, "Tia again", SystemTime::now());
                    let name_hash = runtime.peers().get(&old_dest).unwrap().name_hash;
                    assert_eq!(
                        runtime.peers().get(&new_dest).unwrap().name_hash,
                        name_hash,
                        "both announces name the same instance"
                    );
                    runtime
                        .trust()
                        .note_key_change(&new_identity, &name_hash, SystemTime::now());

                    let out = out_of(&mut ctx, ".mesh peers").await.unwrap();
                    let marker = out
                        .lines()
                        .find(|line| line.contains("key changed:"))
                        .unwrap_or_else(|| panic!("no key-change marker in {out}"));
                    assert!(marker.starts_with(' '), "{out}");
                    assert!(marker.contains(short(&new_identity)), "{out}");
                    assert!(marker.contains(short(&old_identity)), "{out}");
                    assert!(
                        marker.contains(&format!("then .mesh trust {new_dest}; ")),
                        "the marker names the destination heard under the new key: {out}"
                    );
                    assert!(
                        marker.ends_with(&format!(".mesh untrust {old_dest} forgets the old key")),
                        "{out}"
                    );

                    let out = out_of(&mut ctx, &format!(".mesh trust {new_dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("added"), "{out}");
                    assert!(
                        out.contains(&format!(
                            "trusted before as {old_dest} under another identity"
                        )),
                        "{out}"
                    );
                    assert!(out.contains(&format!(".mesh untrust {old_dest}")), "{out}");

                    let out = out_of(&mut ctx, ".mesh peers").await.unwrap();
                    assert!(!out.contains("key changed:"), "{out}");
                    let out = out_of(&mut ctx, ".mesh info").await.unwrap();
                    assert_eq!(info_row(&out, "key changes"), "none", "{out}");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn mesh_on_accepted_at_the_prompt_reaches_start() {
                let guard = TestConfigDirGuard::new("repl-mesh-on-accepted");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), guard.path.join("cache"));
                let _script = prompt_script::install(&[true]);
                let _capture = capture::install();
                run_async(async {
                    let (addr, relay_handle, _) = loopback_relay().await;
                    let mut ctx = ctx_with(private_config(addr.port()), true);
                    ctx.session = Some(Session::default());

                    run(&mut ctx, ".mesh on").await.unwrap();

                    assert!(ctx.app.mesh.get().is_some());
                    assert!(ctx.app.config.mesh.enabled);
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    let out = stdout_lines();
                    let preview = index_of(&out, "What leaves this machine");
                    let summary = index_of(&out, "Mesh is on for this session");
                    assert!(preview < summary, "{out:?}");

                    run(&mut ctx, ".mesh off --yes").await.unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn mesh_off_declined_keeps_the_node() {
                let _guard = TestConfigDirGuard::new("repl-mesh-off-declined");
                let _script = prompt_script::install(&[false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-off-declined").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    ctx.set_mesh_enabled_for_session(true);

                    run(&mut ctx, ".mesh off").await.unwrap();

                    assert!(ctx.app.mesh.get().is_some());
                    assert!(ctx.app.config.mesh.enabled);
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    index_of(&stdout_lines(), "Mesh stays on");

                    run(&mut ctx, ".mesh off --yes").await.unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn reply_and_broadcast_announce_then_send_nothing_when_declined() {
                let _guard = TestConfigDirGuard::new("repl-mesh-declined-sends");
                let _script = prompt_script::install(&[false, false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-declined-sends").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let hash = heard_trusted_peer(&started.runtime, ctx.app.mesh.as_ref());

                    run(&mut ctx, &format!(".mesh reply {hash} hello"))
                        .await
                        .unwrap();
                    let out = stdout_lines();
                    let notice = index_of(&out, "This sends your text to");
                    assert!(out[notice].contains("trust: trusted"), "{out:?}");
                    let nothing = index_of(&out, "Nothing was sent.");
                    assert!(notice < nothing, "{out:?}");
                    let before = out.len();

                    run(&mut ctx, ".mesh broadcast hello").await.unwrap();
                    let out = stdout_lines()[before..].to_vec();
                    let notice = index_of(&out, BROADCAST_NOTICE);
                    let nothing = index_of(&out, "Nothing was sent.");
                    assert!(notice < nothing, "{out:?}");

                    assert_eq!(prompt_script::prompts_asked(), 2);
                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn non_tty_reply_and_broadcast_and_off_name_the_flag() {
                let _guard = TestConfigDirGuard::new("repl-mesh-non-tty");
                let _script = prompt_script::install_non_interactive();
                run_async(async {
                    let started = started_runtime("repl-mesh-non-tty").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let hash = heard_trusted_peer(&started.runtime, ctx.app.mesh.as_ref());

                    for line in [
                        ".mesh off".to_string(),
                        format!(".mesh reply {hash} hi"),
                        ".mesh broadcast hi".to_string(),
                    ] {
                        let err = run(&mut ctx, &line).await.unwrap_err().to_string();
                        assert!(err.contains("--yes"), "{line}: {err}");
                        assert!(ctx.app.mesh.get().is_some(), "{line}");
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Spec-first usage probe, criterion (f) at the command level: `--yes` is consent
            /// only as the LEADING token. Inside the message text it is just words, so
            /// without a terminal the send is refused naming the flag, no prompt is asked,
            /// and nothing leaves the node; a leading `--yes` on the same node proceeds to
            /// the send path (which then fails at the link, since the trusted peer has not
            /// announced, still naming the destination rather than the flag).
            #[test]
            #[serial]
            fn a_yes_inside_the_message_text_is_words_not_consent() {
                let _guard = TestConfigDirGuard::new("repl-mesh-yes-in-text");
                let _script = prompt_script::install_non_interactive();
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-yes-in-text").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let hash = heard_trusted_peer(&started.runtime, ctx.app.mesh.as_ref());

                    for line in [
                        format!(".mesh reply {hash} please say --yes"),
                        format!(".mesh reply {hash} \"--yes in quotes\""),
                        ".mesh broadcast ship it --yes".to_string(),
                        ".mesh broadcast \"--yes\" now".to_string(),
                    ] {
                        let err = run(&mut ctx, &line).await.unwrap_err().to_string();
                        assert!(err.contains("--yes"), "{line}: {err}");
                        assert!(
                            !err.contains(&hash),
                            "{line}: refused for want of consent, not at trust: {err}"
                        );
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    let out = stdout_lines();
                    assert!(
                        !out.iter().any(|line| line.contains("delivered")
                            || line.contains("store-and-forward")
                            || line.starts_with("Bulletin ")),
                        "nothing was sent: {out:?}"
                    );

                    let err = run(
                        &mut ctx,
                        &format!(".mesh reply {hash} --yes please say --yes"),
                    )
                    .await
                    .unwrap_err()
                    .to_string();
                    assert!(
                        err.contains(&hash),
                        "a leading --yes consents and the send reaches trust: {err}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `.mesh answer <id>` routes an inbound id to `answer_inbound` and an outbound
            /// correlation id to a Reply toward that correlation's peer; `reply` takes a
            /// destination and never an id. Both peers are unheard, so each send fails fast
            /// at the trust check naming the routed destination, before any network.
            #[test]
            #[serial]
            fn answer_routes_by_store_and_reply_is_never_an_alias() {
                use crate::mesh::pending::{
                    INBOUND_RECORD_VERSION, InboundKind, PENDING_RECORD_VERSION,
                };
                use crate::mesh::rfc3339_utc;

                let _guard = TestConfigDirGuard::new("repl-mesh-answer");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-answer").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let asked_peer = "ab".repeat(16);
                    let asker_peer = "12".repeat(16);

                    ctx.app
                        .mesh
                        .correlations()
                        .open(crate::mesh::pending::PendingRecord {
                            version: PENDING_RECORD_VERSION,
                            id: "q1".to_string(),
                            peer_destination: asked_peer.clone(),
                            peer_identity: "cd".repeat(16),
                            thread: "q1".to_string(),
                            question: "what now?".to_string(),
                            sent_at: rfc3339_utc(now),
                            timeout_at: rfc3339_utc(now + std::time::Duration::from_secs(600)),
                            state: PendingState::Open,
                            reply: None,
                        })
                        .unwrap();
                    let store = ctx
                        .app
                        .mesh
                        .inbound_store()
                        .expect("install attaches a store");
                    store
                        .upsert(
                            InboundRecord {
                                version: INBOUND_RECORD_VERSION,
                                id: "p1".to_string(),
                                peer_destination: asker_peer.clone(),
                                peer_identity: "ef".repeat(16),
                                thread: "p1".to_string(),
                                question: "may I read the plan?".to_string(),
                                envoy_question: String::new(),
                                received_at: rfc3339_utc(now),
                                kind: InboundKind::Question,
                                paths: Vec::new(),
                                reason: String::new(),
                            },
                            now,
                        )
                        .unwrap();

                    let err = run(&mut ctx, ".mesh answer q1 \"yes\"")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(!err.contains("No open question"), "{err}");
                    assert!(
                        err.contains(&asked_peer),
                        "outbound id routes to its peer: {err}"
                    );
                    assert!(ctx.app.mesh.correlations().get("q1").is_some());

                    let err = run(&mut ctx, ".mesh answer p1 \"no\"")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(!err.contains("No open question"), "{err}");
                    assert!(
                        err.contains(&asker_peer),
                        "inbound id routes to its asker: {err}"
                    );
                    assert!(
                        store.get("p1").unwrap().is_some(),
                        "a failed send keeps the question"
                    );
                    let out = stdout_lines();
                    assert_eq!(
                        out.iter()
                            .filter(|line| line.contains("Sending your answer to"))
                            .count(),
                        2,
                        "each routed answer announces its destination before sending: {out:?}"
                    );
                    assert!(
                        out.iter().any(|line| line.contains(short(&asker_peer))),
                        "{out:?}"
                    );
                    assert!(!out.iter().any(|line| line.contains("Answered")), "{out:?}");

                    let err = run(&mut ctx, ".mesh reply q1 --yes hi")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("32 hex"), "reply never accepts an id: {err}");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Spec-first probe: `.mesh answer <access-id> <text>`
            /// is refused with a teaching error naming `.mesh grant <id>` and
            /// `.mesh refuse <id>` — before any "Sending" notice, so nothing goes on the
            /// wire and the request stays filed for the human to decide. `.mesh pending`
            /// lists the access row apart from the questions, with its path COUNT and the
            /// two verbs, and never a path or the reason (peer text goes to no model and,
            /// on this surface, to no screen either). The question rows still answer.
            #[test]
            #[serial]
            fn usage_probe_answer_refuses_an_access_id_and_pending_lists_it_without_paths() {
                use crate::mesh::pending::{INBOUND_RECORD_VERSION, InboundKind};
                use crate::mesh::rfc3339_utc;

                let _guard = TestConfigDirGuard::new("repl-mesh-answer-access");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-answer-access").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let asker_peer = "12".repeat(16);
                    let store = ctx
                        .app
                        .mesh
                        .inbound_store()
                        .expect("install attaches a store");
                    store
                        .upsert(
                            InboundRecord {
                                version: INBOUND_RECORD_VERSION,
                                id: "acc-1".to_string(),
                                peer_destination: asker_peer.clone(),
                                peer_identity: "ef".repeat(16),
                                thread: "acc-1".to_string(),
                                question: String::new(),
                                envoy_question: String::new(),
                                received_at: rfc3339_utc(now),
                                kind: InboundKind::Access,
                                paths: vec![
                                    "secret/probe-leak-plan.md".to_string(),
                                    "src/probe-leak-x.rs".to_string(),
                                ],
                                reason: "probe-leak-reason".to_string(),
                            },
                            now,
                        )
                        .unwrap();

                    // Only an access row: the question section is empty, the access block
                    // follows the answer hint, and nothing peer-supplied but the id shows.
                    run(&mut ctx, ".mesh pending").await.unwrap();
                    let text = stdout_lines().join("\n");
                    let out: Vec<&str> = text.lines().collect();
                    assert!(
                        !text.contains("probe-leak"),
                        "a path or the reason leaked into `.mesh pending`: {text}"
                    );
                    let hint = out
                        .iter()
                        .position(|line| line.starts_with("answer one with"))
                        .unwrap_or_else(|| panic!("{text}"));
                    assert!(
                        out[..hint].iter().any(|line| line.trim() == "none"),
                        "no question rows: {text}"
                    );
                    assert!(
                        !out[..hint].iter().any(|line| line.contains("acc-1")),
                        "an access request is not a question row: {text}"
                    );
                    assert_eq!(
                        out[hint + 1],
                        "Access requests (decide with grant or refuse):"
                    );
                    let row = out[hint + 2];
                    assert!(row.starts_with("  acc-1  "), "{text}");
                    assert!(row.contains(short(&asker_peer)), "{text}");
                    assert!(row.contains("2 paths"), "{text}");
                    assert!(row.contains("grant: .mesh grant acc-1"), "{text}");
                    assert!(row.contains("refuse: .mesh refuse acc-1"), "{text}");

                    // Answering the access id: teaching error, nothing sent, still filed.
                    let err = run(&mut ctx, ".mesh answer acc-1 \"yes\"")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(
                        err.contains("`acc-1` is an access request, not a question"),
                        "{err}"
                    );
                    assert!(err.contains(".mesh grant acc-1"), "{err}");
                    assert!(err.contains(".mesh refuse acc-1"), "{err}");
                    assert!(!err.contains("No open question"), "{err}");
                    assert!(!err.contains("probe-leak"), "{err}");
                    let out = stdout_lines();
                    assert!(
                        !out.iter()
                            .any(|line| line.contains("Sending your answer to")),
                        "an access id is refused before any send: {out:?}"
                    );
                    assert!(!out.iter().any(|line| line.contains("Answered")), "{out:?}");
                    let filed = store.get("acc-1").unwrap().expect("still filed");
                    assert_eq!(filed.kind, InboundKind::Access);
                    assert_eq!(filed.paths.len(), 2);
                    assert!(ctx.app.mesh.correlations().list().is_empty());

                    // Bare `.mesh answer acc-1` (no text) is still the usage help, and a
                    // question under another id still routes as a question.
                    let before = stdout_lines().len();
                    run(&mut ctx, ".mesh answer acc-1").await.unwrap();
                    let after = stdout_lines();
                    assert!(
                        after[before..]
                            .iter()
                            .any(|t| t.contains("Usage: .mesh answer <id>")),
                        "{after:?}"
                    );
                    store
                        .upsert(
                            InboundRecord {
                                version: INBOUND_RECORD_VERSION,
                                id: "p1".to_string(),
                                peer_destination: asker_peer.clone(),
                                peer_identity: "ef".repeat(16),
                                thread: "p1".to_string(),
                                question: "may I read the plan?".to_string(),
                                envoy_question: String::new(),
                                received_at: rfc3339_utc(now),
                                kind: InboundKind::Question,
                                paths: Vec::new(),
                                reason: String::new(),
                            },
                            now,
                        )
                        .unwrap();
                    let err = run(&mut ctx, ".mesh answer p1 \"no\"")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(!err.contains("is an access request"), "{err}");
                    assert!(
                        err.contains(&asker_peer),
                        "a question still routes to its asker: {err}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Criterion (i), refused by the start itself rather than by `validate`: a
            /// private relay nobody listens on makes `MeshRuntime::start` fail after the
            /// trial id was minted. The session keeps its old id and stays clean, no node is
            /// installed, the catalog and the session config are untouched, and the preview
            /// (with the fresh-id consequence) was already on stdout before the start ran
            /// (B-40) while no "on" summary ever appeared.
            #[test]
            #[serial]
            fn fresh_id_is_not_committed_when_the_real_start_is_refused() {
                let guard = TestConfigDirGuard::new("repl-mesh-fresh-refused");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), guard.path.join("cache"));
                let _capture = capture::install();
                let id = "fedcba9876543210".repeat(2);
                let session: Session = serde_yaml::from_str(&format!(
                    "model: provider:test\nmessages: []\nmesh_instance_id: {id}"
                ))
                .unwrap();
                assert!(!session.dirty());
                run_async(async {
                    let closed_port = {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        listener.local_addr().unwrap().port()
                    };
                    let mut ctx = ctx_with(private_config(closed_port), true);
                    ctx.session = Some(session);
                    ctx.refresh_tool_scope(create_abort_signal()).await.unwrap();

                    let err = run(&mut ctx, ".mesh on --fresh --yes")
                        .await
                        .unwrap_err()
                        .to_string();

                    assert!(
                        !err.contains("function_calling_support"),
                        "the refusal must come from the start, not from validate: {err}"
                    );
                    let session = ctx.session.as_ref().unwrap();
                    assert_eq!(session.mesh_instance_id(), Some(id.as_str()));
                    assert!(!session.dirty(), "a refused start leaves the session clean");
                    assert!(ctx.app.mesh.get().is_none());
                    assert!(!ctx.app.config.mesh.enabled);
                    assert!(mesh_tool_names(&ctx).is_empty());
                    let out = stdout_lines();
                    index_of(&out, "What leaves this machine");
                    index_of(&out, "fresh id");
                    assert!(
                        !out.iter()
                            .any(|line| line.contains("Mesh is on for this session")),
                        "{out:?}"
                    );
                });
            }

            /// `.mesh status <dest>` on a trusted peer whose announce this node's transport
            /// has not carried (the table row exists, the identity does not resolve) says so
            /// instead of opening a link.
            #[test]
            #[serial]
            fn status_of_a_trusted_but_unresolvable_peer_says_it_cannot_be_reached_yet() {
                use crate::mesh::hex_lower;
                use crate::mesh::session_destination_name;
                use crate::mesh::test_support::PeerSighting;
                use crate::mesh::trust::{LiveMesh, TrustOptions};
                use rand_core::OsRng;
                use rns_transport::destination::SingleInputDestination;
                use rns_transport::identity::PrivateIdentity;

                let _guard = TestConfigDirGuard::new("repl-mesh-status-unresolvable");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-status-unresolvable").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let name = session_destination_name("probe");
                    let announced =
                        SingleInputDestination::new(PrivateIdentity::new_from_rand(OsRng), name);
                    let trusted = announced.desc.address_hash.to_hex_string();
                    started.runtime.peers().observe(
                        PeerSighting {
                            destination_hash: trusted.clone(),
                            identity_hash: announced.desc.identity.address_hash.to_hex_string(),
                            name_hash: hex_lower(name.as_name_hash_slice()),
                            display_name: Some("Tia".to_string()),
                            protocol_version: 1,
                            hops: 1,
                        },
                        now,
                    );
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    started
                        .runtime
                        .trust()
                        .trust_destination(slot, &trusted, TrustOptions::default(), now)
                        .unwrap();

                    let err = run(&mut ctx, &format!(".mesh status {trusted}"))
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("cannot be reached yet"), "{err}");
                    let out = stdout_lines();
                    assert!(
                        !out.iter().any(|line| line.contains("Asking")),
                        "no link is opened toward an unresolvable peer: {out:?}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `.mesh status <dest>` on a peer that is in the table but not trusted is
            /// refused by trust standing before any link is opened, naming the same
            /// standing `.mesh peers` lists (`untrusted` / `denied` / `blocked`); a denied
            /// destination the node has never heard is likewise never linked, and its
            /// deny-only row reads `denied`, never `trusted`.
            #[test]
            #[serial]
            fn status_of_an_untrusted_denied_or_blocked_peer_is_refused_before_any_link() {
                use crate::mesh::hex_lower;
                use crate::mesh::test_support::PeerSighting;
                use crate::mesh::trust::LiveMesh;

                let _guard = TestConfigDirGuard::new("repl-mesh-status-refusal");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-status-refusal").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let sighting = |dest: u8, identity: u8, name: &str| PeerSighting {
                        destination_hash: hex_lower(&[dest; 16]),
                        identity_hash: hex_lower(&[identity; 16]),
                        name_hash: String::new(),
                        display_name: Some(name.to_string()),
                        protocol_version: 1,
                        hops: 1,
                    };
                    let untrusted = hex_lower(&[0xa1; 16]);
                    let denied = hex_lower(&[0xb2; 16]);
                    let blocked = hex_lower(&[0xc3; 16]);
                    let blocked_identity = hex_lower(&[0xc4; 16]);
                    let deny_only = hex_lower(&[0xd5; 16]);
                    let peers = started.runtime.peers();
                    peers.observe(sighting(0xa1, 0xa2, "Ann"), now);
                    peers.observe(sighting(0xb2, 0xb3, "Bea"), now);
                    peers.observe(sighting(0xc3, 0xc4, "Cid"), now);
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    trust.deny_destination(slot, &denied, None, now).unwrap();
                    trust
                        .block_identity(slot, &blocked_identity, None, now)
                        .unwrap();
                    trust.deny_destination(slot, &deny_only, None, now).unwrap();

                    for (destination, standing) in [
                        (&untrusted, "untrusted"),
                        (&denied, "denied"),
                        (&blocked, "blocked"),
                    ] {
                        let err = run(&mut ctx, &format!(".mesh status {destination}"))
                            .await
                            .unwrap_err()
                            .to_string();
                        assert!(
                            err.contains(&format!(" is {standing} ")),
                            "{destination} should be refused as {standing}: {err}"
                        );
                    }
                    assert!(
                        run(&mut ctx, &format!(".mesh status {deny_only}"))
                            .await
                            .is_err(),
                        "an unheard denied destination is never fetched"
                    );
                    let out = stdout_lines();
                    assert!(
                        !out.iter().any(|line| line.contains("Asking")),
                        "no status link may be opened toward a refused destination: {out:?}"
                    );

                    run(&mut ctx, ".mesh peers").await.unwrap();
                    let out = stdout_lines();
                    let row = |destination: &str| {
                        out.iter()
                            .flat_map(|text| text.lines())
                            .find(|line| line.contains(short(destination)))
                            .unwrap_or_else(|| panic!("{destination} missing from {out:?}"))
                            .to_string()
                    };
                    assert!(row(&untrusted).contains("untrusted"), "{}", row(&untrusted));
                    assert!(row(&denied).contains(" denied "), "{}", row(&denied));
                    assert!(!row(&denied).contains(" trusted "), "{}", row(&denied));
                    assert!(row(&blocked).contains(" blocked "), "{}", row(&blocked));
                    assert!(row(&deny_only).contains(" denied "), "{}", row(&deny_only));
                    assert!(
                        !row(&deny_only).contains(" trusted "),
                        "{}",
                        row(&deny_only)
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `.mesh info <dest>` consults the peer table first: a knock cache that no
            /// longer parses does not hide a known peer, while for a destination the node
            /// has not heard the command still errors rather than rendering anything.
            /// `.mesh knocks` reports the unreadable cache as an error, never as an empty
            /// list.
            #[test]
            #[serial]
            fn info_on_a_known_peer_survives_an_unreadable_knock_cache() {
                use crate::mesh::hex_lower;
                use crate::mesh::test_support::PeerSighting;

                let _guard = TestConfigDirGuard::new("repl-mesh-info-knocks");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-info-knocks").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let known = hex_lower(&[0x5a; 16]);
                    started.runtime.peers().observe(
                        PeerSighting {
                            destination_hash: known.clone(),
                            identity_hash: hex_lower(&[0x5b; 16]),
                            name_hash: String::new(),
                            display_name: Some("Kay".to_string()),
                            protocol_version: 1,
                            hops: 2,
                        },
                        now,
                    );
                    let cache_path = started.runtime.knock_gate().cache().path().to_path_buf();
                    std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
                    std::fs::write(&cache_path, "{this is not a knock record\n").unwrap();

                    run(&mut ctx, &format!(".mesh info {known}")).await.unwrap();
                    let out = stdout_lines();
                    assert!(out.iter().any(|line| line.contains("Kay")), "{out:?}");
                    assert!(out.iter().any(|line| line.contains(&known)), "{out:?}");
                    let warned: Vec<String> = capture::lines()
                        .into_iter()
                        .filter(|(stream, _)| *stream == capture::Stream::Err)
                        .map(|(_, text)| text)
                        .collect();
                    assert!(
                        warned.iter().any(|line| line.contains("knock cache")),
                        "the unreadable cache is reported on stderr: {warned:?}"
                    );

                    let unknown = hex_lower(&[0x6b; 16]);
                    assert!(
                        run(&mut ctx, &format!(".mesh info {unknown}"))
                            .await
                            .is_err(),
                        "nothing known must be an error"
                    );
                    assert!(
                        !stdout_lines().iter().any(|line| line.contains(&unknown)),
                        "an unknown destination renders no detail"
                    );
                    let err = run(&mut ctx, ".mesh knocks").await.unwrap_err().to_string();
                    assert!(err.contains("knock"), "{err}");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// A destination that only knocked is still something `.mesh info` can describe.
            #[test]
            #[serial]
            fn info_on_a_knock_only_destination_renders_the_knock() {
                use crate::mesh::hex_lower;
                use crate::mesh::knocks::KNOCK_RECORD_VERSION;
                use crate::mesh::rfc3339_utc;

                let _guard = TestConfigDirGuard::new("repl-mesh-info-knock-only");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-info-knock-only").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let knocker = hex_lower(&[0x7c; 16]);
                    started
                        .runtime
                        .knock_gate()
                        .cache()
                        .append(
                            KnockRecord {
                                version: KNOCK_RECORD_VERSION,
                                received_at: rfc3339_utc(now),
                                identity_hash: hex_lower(&[0x7d; 16]),
                                destination_hash: knocker.clone(),
                                name_hash: String::new(),
                                display_name: Some("Kip".to_string()),
                                intro: Some("hello there".to_string()),
                                hops: 1,
                            },
                            now,
                        )
                        .unwrap();

                    run(&mut ctx, &format!(".mesh info {knocker}"))
                        .await
                        .unwrap();

                    let out = stdout_lines();
                    assert!(out.iter().any(|line| line.contains(&knocker)), "{out:?}");
                    assert!(out.iter().any(|line| line.contains("Kip")), "{out:?}");
                    assert!(out.iter().any(|line| line.contains("knocked:")), "{out:?}");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Spec-first usage probe, criterion (f) + B-28 at the command level: `.mesh reply`
            /// announces a trusted destination WITH its trust standing before asking, and an
            /// untrusted, denied or blocked destination is refused naming that standing before
            /// any prompt is asked (a leading `--yes` does not bypass that refusal: it is not a
            /// consent question). A destination the trust list denies but the node has not
            /// heard is still a denied destination and must be refused before the prompt too.
            #[test]
            #[serial]
            fn reply_shows_the_standing_and_refuses_denied_or_blocked_before_asking() {
                use crate::mesh::hex_lower;
                use crate::mesh::session_destination_name;
                use crate::mesh::test_support::PeerSighting;
                use crate::mesh::trust::{LiveMesh, TrustOptions};
                use rand_core::OsRng;
                use rns_transport::destination::SingleInputDestination;
                use rns_transport::identity::PrivateIdentity;

                let _guard = TestConfigDirGuard::new("repl-mesh-reply-standing");
                let _script = prompt_script::install(&[false, false, false, false, false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-reply-standing").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let sighting = |dest: u8, identity: u8, name: &str| PeerSighting {
                        destination_hash: hex_lower(&[dest; 16]),
                        identity_hash: hex_lower(&[identity; 16]),
                        name_hash: String::new(),
                        display_name: Some(name.to_string()),
                        protocol_version: 1,
                        hops: 1,
                    };
                    // Trusting a destination verifies identity + name hash -> destination,
                    // so the trusted peer is derived for real.
                    let name = session_destination_name("probe");
                    let announced =
                        SingleInputDestination::new(PrivateIdentity::new_from_rand(OsRng), name);
                    let trusted = announced.desc.address_hash.to_hex_string();
                    let untrusted = hex_lower(&[0x22; 16]);
                    let denied = hex_lower(&[0x33; 16]);
                    let blocked = hex_lower(&[0x44; 16]);
                    let blocked_identity = hex_lower(&[0x45; 16]);
                    let deny_only = hex_lower(&[0x66; 16]);
                    let peers = started.runtime.peers();
                    peers.observe(
                        PeerSighting {
                            destination_hash: trusted.clone(),
                            identity_hash: announced.desc.identity.address_hash.to_hex_string(),
                            name_hash: hex_lower(name.as_name_hash_slice()),
                            display_name: Some("Tia".to_string()),
                            protocol_version: 1,
                            hops: 1,
                        },
                        now,
                    );
                    peers.observe(sighting(0x22, 0x23, "Uma"), now);
                    peers.observe(sighting(0x33, 0x34, "Dee"), now);
                    peers.observe(sighting(0x44, 0x45, "Bob"), now);
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    trust
                        .trust_destination(slot, &trusted, TrustOptions::default(), now)
                        .unwrap();
                    trust.deny_destination(slot, &denied, None, now).unwrap();
                    trust
                        .block_identity(slot, &blocked_identity, None, now)
                        .unwrap();
                    trust.deny_destination(slot, &deny_only, None, now).unwrap();

                    // Trusted peer: the announcement names the peer and its standing, then the
                    // (declined) prompt is asked and nothing is sent.
                    let asked_before = prompt_script::prompts_asked();
                    let before = stdout_lines().len();
                    run(&mut ctx, &format!(".mesh reply {trusted} hello"))
                        .await
                        .unwrap();
                    let out = stdout_lines()[before..].to_vec();
                    let notice = &out[index_of(&out, "This sends your text to")];
                    assert!(notice.contains("Tia"), "{notice}");
                    assert!(
                        notice.contains("trust: trusted"),
                        "the announcement carries the standing: {notice}"
                    );
                    assert!(
                        out.iter().any(|line| line == "Nothing was sent."),
                        "{out:?}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), asked_before + 1);

                    // Untrusted, denied and blocked heard peers: refused before any prompt,
                    // and `--yes` changes nothing about that.
                    for (destination, standing) in [
                        (&untrusted, "untrusted"),
                        (&denied, "denied"),
                        (&blocked, "blocked"),
                    ] {
                        for line in [
                            format!(".mesh reply {destination} hello"),
                            format!(".mesh reply --yes {destination} hello"),
                        ] {
                            let asked_before = prompt_script::prompts_asked();
                            let before = stdout_lines().len();
                            let err = run(&mut ctx, &line).await.unwrap_err().to_string();
                            assert!(
                                err.contains(&format!(" is {standing} ")),
                                "{line}: refused naming the standing: {err}"
                            );
                            assert_eq!(
                                prompt_script::prompts_asked(),
                                asked_before,
                                "{line}: refused before the prompt"
                            );
                            let out = stdout_lines()[before..].to_vec();
                            assert!(
                                !out.iter()
                                    .any(|text| text.contains("This sends your text to")
                                        || text.starts_with("Sent ")),
                                "{line}: no announcement, no send: {out:?}"
                            );
                        }
                    }

                    // A denied destination the node has not heard: still denied, so still
                    // refused before the prompt, and never announced as a plain unheard node.
                    let asked_before = prompt_script::prompts_asked();
                    let before = stdout_lines().len();
                    let result = run(&mut ctx, &format!(".mesh reply {deny_only} hello")).await;
                    let out = stdout_lines()[before..].to_vec();
                    assert!(
                        result.is_err(),
                        "a denied destination is refused even when unheard: {out:?}"
                    );
                    assert_eq!(
                        prompt_script::prompts_asked(),
                        asked_before,
                        "a denied destination is refused before the prompt: {out:?}"
                    );
                    assert!(
                        !out.iter().any(|text| text.contains("unheard node")),
                        "the trust list knows this destination; it is not a plain unheard node: {out:?}"
                    );
                    // With consent given up front nothing may leave either, and the
                    // announcement must still not misreport the standing.
                    let before = stdout_lines().len();
                    let err = run(&mut ctx, &format!(".mesh reply --yes {deny_only} hello"))
                        .await
                        .unwrap_err()
                        .to_string();
                    let out = stdout_lines()[before..].to_vec();
                    assert!(!out.iter().any(|text| text.starts_with("Sent ")), "{out:?}");
                    assert!(err.contains(&deny_only) || err.contains("denied"), "{err}");
                    assert!(
                        !out.iter().any(|text| text.contains("unheard node")),
                        "denied, not merely unheard: {out:?}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe (spec (f)): the reply gate runs BEFORE any notice or prompt and
            /// refuses every non-Allow verdict. A destination the node has not heard (and
            /// the trust list does not deny) is a not-heard error, never an announcement or
            /// a consent question; a heard peer whose verdict is not Allow ("untrusted",
            /// the default-closed standing) is refused naming that standing, and a leading
            /// `--yes` does not turn it into a send. Nothing may reach `send_peer`, which
            /// would refuse the same peer as NotTrusted after the human had already consented.
            #[test]
            #[serial]
            fn reply_refuses_unheard_and_untrusted_destinations_before_asking() {
                use crate::mesh::test_support::PeerSighting;

                let _guard = TestConfigDirGuard::new("repl-mesh-reply-gate-first");
                // Every answer is "yes": if any prompt is asked at all, consent is given,
                // so a wrongly gated destination would visibly reach the send path.
                let _script = prompt_script::install(&[true, true, true, true]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-reply-gate-first").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let unheard = hex_lower(&[0x77; 16]);
                    let untrusted = hex_lower(&[0x88; 16]);
                    started.runtime.peers().observe(
                        PeerSighting {
                            destination_hash: untrusted.clone(),
                            identity_hash: hex_lower(&[0x89; 16]),
                            name_hash: String::new(),
                            display_name: Some("Uma".to_string()),
                            protocol_version: 1,
                            hops: 1,
                        },
                        now,
                    );

                    // Not heard, not denied: a not-heard error, no notice, no prompt.
                    for line in [
                        format!(".mesh reply {unheard} hello"),
                        format!(".mesh reply --yes {unheard} hello"),
                    ] {
                        let asked_before = prompt_script::prompts_asked();
                        let before = stdout_lines().len();
                        let err = run(&mut ctx, &line).await.unwrap_err().to_string();
                        let out = stdout_lines()[before..].to_vec();
                        assert!(
                            err.contains("has not been heard"),
                            "{line}: an unheard destination is a not-heard error: {err}"
                        );
                        assert_eq!(
                            prompt_script::prompts_asked(),
                            asked_before,
                            "{line}: no consent question for an unheard destination"
                        );
                        assert!(
                            !out.iter()
                                .any(|text| text.contains("This sends your text to")
                                    || text.starts_with("Sent ")),
                            "{line}: no announcement, no send: {out:?}"
                        );
                    }

                    // Heard but untrusted (default-closed): the verdict is not Allow, so the
                    // gate refuses naming the standing before any notice or prompt.
                    for line in [
                        format!(".mesh reply {untrusted} hello"),
                        format!(".mesh reply --yes {untrusted} hello"),
                    ] {
                        let asked_before = prompt_script::prompts_asked();
                        let before = stdout_lines().len();
                        let result = run(&mut ctx, &line).await;
                        let out = stdout_lines()[before..].to_vec();
                        let err = match result {
                            Ok(_) => panic!(
                                "{line}: an untrusted peer must be refused, not sent to: {out:?}"
                            ),
                            Err(err) => err.to_string(),
                        };
                        assert!(
                            err.contains("untrusted"),
                            "{line}: refused naming the standing: {err}"
                        );
                        assert_eq!(
                            prompt_script::prompts_asked(),
                            asked_before,
                            "{line}: the trust gate runs before the prompt (out: {out:?}, err: {err})"
                        );
                        assert!(
                            !out.iter()
                                .any(|text| text.contains("This sends your text to")),
                            "{line}: the gate runs before any notice: {out:?}"
                        );
                    }

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn knock_help_intro_limit_and_bad_hash_are_teaching_errors() {
                use crate::mesh::knocks::KNOCK_INTRO_MAX_CHARS;

                let _guard = TestConfigDirGuard::new("repl-mesh-knock-teaching");
                let _script = prompt_script::install(&[]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-teaching").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let (heard, _) = heard_peer(&started.runtime, "Tia", SystemTime::now());

                    run(&mut ctx, ".mesh knock").await.unwrap();
                    let out = stdout_lines();
                    assert!(
                        out.iter()
                            .any(|line| line.contains(".mesh knock <destination>")),
                        "{out:?}"
                    );

                    let err = run(&mut ctx, ".mesh knock nothex")
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("is not a destination hash"), "{err}");

                    let long = "x".repeat(KNOCK_INTRO_MAX_CHARS + 1);
                    let err = run(&mut ctx, &format!(".mesh knock {heard} --intro \"{long}\""))
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(
                        err.contains(&format!(
                            "above the {KNOCK_INTRO_MAX_CHARS}-character limit"
                        )),
                        "{err}"
                    );

                    // Heard, but its announce never reached the transport: refused after the
                    // consent, before any link is opened.
                    let before = stdout_lines().len();
                    let err = run(&mut ctx, &format!(".mesh knock {heard} --yes"))
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("cannot be reached yet"), "{err}");
                    let out = stdout_lines()[before..].to_vec();
                    assert!(
                        !out.iter().any(|line| line.starts_with("Knocking on")),
                        "{out:?}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn knock_announces_then_sends_nothing_when_declined() {
                let _guard = TestConfigDirGuard::new("repl-mesh-knock-declined");
                let _script = prompt_script::install(&[false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-declined").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let (heard, _) = heard_peer(&started.runtime, "Tia", SystemTime::now());

                    run(&mut ctx, &format!(".mesh knock {heard} --intro \"hello\""))
                        .await
                        .unwrap();
                    let out = stdout_lines();
                    let notice = index_of(&out, "This knocks on Tia (");
                    assert!(out[notice].contains("with intro \"hello\""), "{out:?}");
                    let nothing = index_of(&out, "Nothing was sent.");
                    assert!(notice < nothing, "{out:?}");
                    assert!(
                        !out.iter().any(|line| line.starts_with("Knocking on")),
                        "{out:?}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 1);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn knock_to_a_destination_never_heard_is_refused() {
                let _guard = TestConfigDirGuard::new("repl-mesh-knock-unheard");
                let _script = prompt_script::install(&[]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-unheard").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let unheard = hex_lower(&[0x77; 16]);

                    for line in [
                        format!(".mesh knock {unheard}"),
                        format!(".mesh knock {unheard} --yes --intro \"hi\""),
                    ] {
                        let before = stdout_lines().len();
                        let err = run(&mut ctx, &line).await.unwrap_err().to_string();
                        assert!(err.contains("has not been heard"), "{line}: {err}");
                        let out = stdout_lines()[before..].to_vec();
                        assert!(
                            !out.iter().any(|text| text.starts_with("This knocks on")),
                            "{line}: no notice for an unheard destination: {out:?}"
                        );
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe (spec (f)): `.mesh reply <dest>` applies the SAME trust gate as
            /// `.mesh status <dest>`, so for one destination both verbs classify the refusal
            /// the same way: plain unheard -> not-heard; unheard but denied at the
            /// destination tier -> denied; heard untrusted/denied/blocked -> that standing.
            #[test]
            #[serial]
            fn reply_and_status_classify_the_same_destination_the_same_way() {
                use crate::mesh::test_support::PeerSighting;
                use crate::mesh::trust::LiveMesh;

                let _guard = TestConfigDirGuard::new("repl-mesh-reply-status-parity");
                let _script = prompt_script::install(&[false, false, false, false, false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-reply-status-parity").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let sighting = |dest: u8, identity: u8, name: &str| PeerSighting {
                        destination_hash: hex_lower(&[dest; 16]),
                        identity_hash: hex_lower(&[identity; 16]),
                        name_hash: String::new(),
                        display_name: Some(name.to_string()),
                        protocol_version: 1,
                        hops: 1,
                    };
                    let unheard = hex_lower(&[0x51; 16]);
                    let unheard_denied = hex_lower(&[0x52; 16]);
                    let untrusted = hex_lower(&[0x61; 16]);
                    let denied = hex_lower(&[0x62; 16]);
                    let blocked = hex_lower(&[0x63; 16]);
                    let blocked_identity = hex_lower(&[0x64; 16]);
                    let peers = started.runtime.peers();
                    peers.observe(sighting(0x61, 0x71, "Una"), now);
                    peers.observe(sighting(0x62, 0x72, "Dot"), now);
                    peers.observe(sighting(0x63, 0x64, "Bex"), now);
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    trust.deny_destination(slot, &denied, None, now).unwrap();
                    trust
                        .deny_destination(slot, &unheard_denied, None, now)
                        .unwrap();
                    trust
                        .block_identity(slot, &blocked_identity, None, now)
                        .unwrap();

                    let classify = |err: &str| -> &'static str {
                        if err.contains(" is denied ") {
                            "denied"
                        } else if err.contains(" is blocked ") {
                            "blocked"
                        } else if err.contains(" is untrusted ") {
                            "untrusted"
                        } else if err.contains("has not been heard") {
                            "not-heard"
                        } else {
                            "other"
                        }
                    };

                    for (destination, expected) in [
                        (&unheard, "not-heard"),
                        (&unheard_denied, "denied"),
                        (&untrusted, "untrusted"),
                        (&denied, "denied"),
                        (&blocked, "blocked"),
                    ] {
                        let asked_before = prompt_script::prompts_asked();
                        let status_err = run(&mut ctx, &format!(".mesh status {destination}"))
                            .await
                            .unwrap_err()
                            .to_string();
                        let reply_err = run(&mut ctx, &format!(".mesh reply {destination} hello"))
                            .await
                            .unwrap_err()
                            .to_string();
                        assert_eq!(
                            classify(&reply_err),
                            expected,
                            "reply gate for {destination}: {reply_err}"
                        );
                        assert_eq!(
                            classify(&status_err),
                            expected,
                            "status gate for {destination}: {status_err}"
                        );
                        assert_eq!(
                            classify(&status_err),
                            classify(&reply_err),
                            "reply and status share one gate for {destination}:\n  status: {status_err}\n  reply:  {reply_err}"
                        );
                        assert_eq!(
                            prompt_script::prompts_asked(),
                            asked_before,
                            "{destination}: refused before any prompt"
                        );
                    }
                    let out = stdout_lines();
                    assert!(
                        !out.iter().any(|line| line.contains("Asking")
                            || line.contains("This sends your text to")
                            || line.starts_with("Sent ")),
                        "no notice or link toward a refused destination: {out:?}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            fn trust_file(store: &TrustStore) -> Option<Vec<u8>> {
                fs::read(store.path()).ok()
            }

            fn identity_of(store: &TrustStore, destination: &str) -> String {
                store
                    .records()
                    .into_iter()
                    .find(|record| record.hash == destination)
                    .and_then(|record| record.identity)
                    .unwrap_or_else(|| panic!("{destination} has no trust record"))
            }

            fn trusted_hashes(store: &TrustStore) -> Vec<String> {
                store
                    .records()
                    .into_iter()
                    .map(|record| record.hash)
                    .collect()
            }

            /// What `line` alone printed to stdout, apart from the commands before it.
            async fn out_of(ctx: &mut RequestContext, line: &str) -> Result<String> {
                let before = stdout_lines().len();
                run(ctx, line).await?;
                Ok(stdout_lines()[before..].join("\n"))
            }

            async fn refusal(ctx: &mut RequestContext, line: &str) -> String {
                run(ctx, line).await.expect_err(line).to_string()
            }

            /// The answer is the consent: `no` writes nothing, `yes` adds the record with
            /// its label, and `--yes` stands in for the question rather than asking it.
            #[test]
            #[serial]
            fn trust_confirms_then_writes_the_record_on_yes_and_nothing_on_no() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-confirm");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-confirm").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    assert!(trust_file(&trust).is_none());

                    let declined = prompt_script::install(&[false]);
                    let out = out_of(&mut ctx, &format!(".mesh trust {dest}"))
                        .await
                        .unwrap();
                    assert!(out.contains(NOTHING_CHANGED), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    assert!(trust_file(&trust).is_none());
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Refuse);
                    drop(declined);

                    let _script = prompt_script::install(&[true]);
                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --label \"Tia box\""))
                        .await
                        .unwrap();
                    assert!(out.contains("added"), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Allow);
                    assert!(
                        trust.records().iter().any(|record| record.hash == dest
                            && record.label.as_deref() == Some("Tia box")),
                        "{:?}",
                        trust.records()
                    );

                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("updated"), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 1);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Without a terminal each mutation refuses naming `--yes` instead of hanging on
            /// a prompt nobody can answer; with `--yes` each goes through, and a block takes
            /// the identity's instance record with it.
            #[test]
            #[serial]
            fn trust_mutations_without_a_terminal_fail_naming_the_flag() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-non-tty");
                let _capture = capture::install();
                let _script = prompt_script::install_non_interactive();
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-non-tty").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    for line in [
                        format!(".mesh trust {dest}"),
                        format!(".mesh untrust {dest}"),
                        format!(".mesh forget {dest}"),
                        format!(".mesh block {id}"),
                        format!(".mesh unblock {id}"),
                    ] {
                        let err = refusal(&mut ctx, &line).await;
                        assert!(err.contains("pass --yes to confirm"), "{line}: {err}");
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Untrusted"), "{out}");
                    assert!(!trusted_hashes(&trust).contains(&dest));
                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Trusted"), "{out}");
                    assert!(trusted_hashes(&trust).contains(&dest));
                    let out = out_of(&mut ctx, &format!(".mesh block {id} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Blocked"), "{out}");
                    assert!(trust.blocked().iter().any(|record| record.hash == id));
                    assert!(
                        !trusted_hashes(&trust).contains(&dest),
                        "a block removes the identity's instance record"
                    );
                    let out = out_of(&mut ctx, &format!(".mesh unblock {id} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Unblocked"), "{out}");
                    assert!(trust.blocked().is_empty());
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn trust_identity_confirms_with_every_instance_wording() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-identity");
                let _capture = capture::install();
                let _script = prompt_script::install(&[true]);
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-identity").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (_, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());

                    let out = out_of(&mut ctx, &format!(".mesh trust --identity {id}"))
                        .await
                        .unwrap();
                    assert!(out.contains("every instance"), "{out}");
                    assert!(out.contains("added"), "{out}");
                    assert!(
                        out.contains("A rotation of this identity is not detected"),
                        "{out}"
                    );
                    assert!(out.contains(".mesh trust <destination>"), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    assert!(
                        trust
                            .records()
                            .iter()
                            .any(|record| record.tier == Tier::Identity
                                && record.hash == id
                                && record.all_destinations),
                        "{:?}",
                        trust.records()
                    );
                    let other = hex_lower(&[0x5a; 16]);
                    assert_eq!(trust.authorize(&id, &other).decision, Decision::Allow);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The prune token, not a prompt, is the consent: bare and `--dry-run` only list
            /// and print the token (on a terminal or not), a token for a different count is
            /// refused, and the matching token removes exactly the listed instances.
            #[test]
            #[serial]
            fn trust_prune_is_a_dry_run_until_the_token_comes_back() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-prune");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-prune").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    // The peer table's sighting rescues a record from pruning, so the stale
                    // peer must have been heard as long ago as it was trusted.
                    let long_ago = SystemTime::now() - Duration::from_secs(40 * 86_400);
                    let (old, _) = heard_peer(&started.runtime, "Old", long_ago);
                    trust
                        .trust_destination(slot, &old, TrustOptions::default(), long_ago)
                        .unwrap();
                    let fresh = heard_trusted_peer(&started.runtime, slot);
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    let tty = prompt_script::install(&[]);
                    for line in [".mesh trust --prune", ".mesh trust --prune --dry-run"] {
                        let out = out_of(&mut ctx, line).await.unwrap();
                        assert!(out.contains(&old), "{line}: {out}");
                        assert!(!out.contains(&fresh), "{line}: {out}");
                        assert!(out.contains("This was a dry run"), "{line}: {out}");
                        assert!(
                            out.contains(
                                "To remove these 1 instance(s), run: .mesh trust --prune --confirm prune-1"
                            ),
                            "no key-change count when nothing is marked: {line}: {out}"
                        );
                        assert_eq!(trust_file(&trust), before, "{line}");
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    drop(tty);

                    let _script = prompt_script::install_non_interactive();
                    for line in [".mesh trust --prune", ".mesh trust --prune --dry-run"] {
                        let out = out_of(&mut ctx, line).await.unwrap();
                        assert!(out.contains(&old), "{line}: {out}");
                        assert!(out.contains("This was a dry run"), "{line}: {out}");
                        assert!(out.contains("--confirm prune-1"), "{line}: {out}");
                        assert_eq!(trust_file(&trust), before, "{line}");
                    }

                    let err = refusal(&mut ctx, ".mesh trust --prune --confirm prune-2").await;
                    assert!(err.contains("has changed since the dry run"), "{err}");
                    let err = refusal(&mut ctx, ".mesh trust --prune --yes").await;
                    assert!(err.contains("--confirm prune-<N>"), "{err}");
                    let err = refusal(&mut ctx, ".mesh trust --prune --confirm bogus").await;
                    assert!(err.contains("prune-<N>"), "{err}");
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(&mut ctx, ".mesh trust --prune --confirm prune-1")
                        .await
                        .unwrap();
                    assert!(out.contains("Removed 1"), "{out}");
                    assert!(out.contains(&old), "{out}");
                    let hashes = trusted_hashes(&trust);
                    assert!(!hashes.contains(&old), "{hashes:?}");
                    assert!(hashes.contains(&fresh), "{hashes:?}");

                    let out = out_of(&mut ctx, ".mesh trust --prune --older-than 1m")
                        .await
                        .unwrap();
                    assert!(out.contains("nothing to prune"), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn prune_dry_run_shows_key_change_marks() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-prune-marked");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-prune-marked").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let long_ago = SystemTime::now() - Duration::from_secs(40 * 86_400);
                    let (marked, _) = heard_peer(&started.runtime, "Old", long_ago);
                    trust
                        .trust_destination(slot, &marked, TrustOptions::default(), long_ago)
                        .unwrap();
                    let (_, seen) = heard_peer(&started.runtime, "Old again", SystemTime::now());
                    let name_hash = started.runtime.peers().get(&marked).unwrap().name_hash;
                    assert_eq!(
                        trust
                            .note_key_change(&seen, &name_hash, SystemTime::now())
                            .len(),
                        1
                    );
                    let before = trust_file(&trust);

                    let out = out_of(&mut ctx, ".mesh trust --prune").await.unwrap();

                    let marked_row = out
                        .lines()
                        .find(|line| line.contains(&marked))
                        .unwrap_or_else(|| panic!("{out}"));
                    assert!(
                        marked_row.contains(&format!(
                            "key changed: announced under identity {} 0s ago",
                            short(&seen)
                        )),
                        "{out}"
                    );
                    assert!(
                        out.contains(
                            "To remove these 1 instance(s), 1 of them marked key-changed, run: .mesh trust --prune --confirm prune-1"
                        ),
                        "{out}"
                    );
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(&mut ctx, ".mesh trust --prune --confirm prune-1")
                        .await
                        .unwrap();
                    assert!(out.contains("Removed 1"), "{out}");
                    assert!(
                        trust
                            .records()
                            .iter()
                            .all(|record| record.key_changed.is_none()),
                        "a confirmed prune removes the marked record with the rest"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn untrust_identity_is_a_dry_run_until_its_token_comes_back() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-identity");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-identity").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);
                    run(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    let token = format!("untrust-{}", short(&id));

                    for line in [
                        format!(".mesh untrust --identity {id}"),
                        format!(".mesh untrust --identity {id} --dry-run"),
                    ] {
                        let out = out_of(&mut ctx, &line).await.unwrap();
                        assert!(out.contains(&id), "{line}: {out}");
                        assert!(out.contains(&format!("instance {dest}")), "{line}: {out}");
                        assert!(!out.contains("refused"), "{line}: {out}");
                        assert!(out.contains("This was a dry run"), "{line}: {out}");
                        assert!(out.contains(&format!("--confirm {token}")), "{line}: {out}");
                        assert_eq!(trust_file(&trust), before, "{line}");
                    }
                    let err = refusal(
                        &mut ctx,
                        &format!(".mesh untrust --identity {id} --confirm untrust-nope"),
                    )
                    .await;
                    assert!(err.contains(&token), "{err}");
                    let err =
                        refusal(&mut ctx, &format!(".mesh untrust --identity {id} --yes")).await;
                    assert!(err.contains("--confirm untrust-"), "{err}");
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(
                        &mut ctx,
                        &format!(".mesh untrust --identity {id} --confirm {token}"),
                    )
                    .await
                    .unwrap();
                    assert!(out.contains("Untrusted identity"), "{out}");
                    let hashes = trusted_hashes(&trust);
                    assert!(!hashes.contains(&id), "{hashes:?}");
                    assert!(!hashes.contains(&dest), "{hashes:?}");

                    let unknown = hex_lower(&[0x5b; 16]);
                    let err =
                        refusal(&mut ctx, &format!(".mesh untrust --identity {unknown}")).await;
                    assert!(err.contains("nothing to untrust"), "{err}");
                    let err = refusal(&mut ctx, &format!(".mesh untrust {dest} --yes")).await;
                    assert!(err.contains("nothing to untrust"), "{err}");
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// A knock carries the same proof as an announce, so a destination heard only
            /// through the knock cache can be trusted; one in neither place is sent to the
            /// lists that show what can be.
            #[test]
            #[serial]
            fn trust_accepts_a_destination_that_only_knocked() {
                use crate::mesh::knocks::KNOCK_RECORD_VERSION;
                use crate::mesh::rfc3339_utc;

                let _guard = TestConfigDirGuard::new("repl-mesh-trust-knock-only");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-knock-only").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id, name_hash) = announced_peer();
                    started
                        .runtime
                        .knock_gate()
                        .cache()
                        .append(
                            KnockRecord {
                                version: KNOCK_RECORD_VERSION,
                                received_at: rfc3339_utc(now),
                                identity_hash: id.clone(),
                                destination_hash: dest.clone(),
                                name_hash,
                                display_name: Some("Kip".to_string()),
                                intro: None,
                                hops: 1,
                            },
                            now,
                        )
                        .unwrap();

                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("added"), "{out}");
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Allow);

                    let (other, _, _) = announced_peer();
                    let err = refusal(&mut ctx, &format!(".mesh trust {other} --yes")).await;
                    assert!(err.contains(".mesh knocks"), "{err}");
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// With the mesh on, a malformed hash is refused by shape, naming which kind of
            /// hash the verb wanted, before any prompt.
            #[test]
            #[serial]
            fn mutation_errors_name_the_hash_shape() {
                let _guard = TestConfigDirGuard::new("repl-mesh-hash-shape");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-hash-shape").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();

                    for flag in ["", " --yes"] {
                        let err = refusal(&mut ctx, &format!(".mesh block nothex{flag}")).await;
                        assert!(err.contains("is not an identity hash"), "{err}");
                        let err = refusal(&mut ctx, &format!(".mesh untrust nothex{flag}")).await;
                        assert!(err.contains("is not a destination hash"), "{err}");
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// With a node running, each bare trust verb is still help: no error, no
            /// prompt, no change to the trust file, and the untrust and block help each
            /// name the other as the contrast.
            #[test]
            #[serial]
            fn bare_trust_verbs_while_on_print_help_and_touch_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-bare-trust-on");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-bare-trust-on").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    for (verb, noun) in [
                        ("trust", "<destination>"),
                        ("untrust", "<destination>"),
                        ("forget", "<destination>"),
                        ("block", "<identity>"),
                        ("unblock", "<identity>"),
                    ] {
                        let out = out_of(&mut ctx, &format!(".mesh {verb}")).await.unwrap();
                        assert!(
                            out.contains(&format!(".mesh {verb} {noun}")),
                            "{verb}: {out}"
                        );
                        assert!(
                            !out.contains("added"),
                            "{verb}: help is never an action: {out}"
                        );
                    }
                    let untrust_help = out_of(&mut ctx, ".mesh untrust").await.unwrap();
                    assert!(untrust_help.contains("`block` remembers"), "{untrust_help}");
                    let block_help = out_of(&mut ctx, ".mesh block").await.unwrap();
                    assert!(
                        block_help.contains("`untrust` only forgets"),
                        "{block_help}"
                    );
                    let forget_help = out_of(&mut ctx, ".mesh forget").await.unwrap();
                    assert!(forget_help.contains("alias of `untrust`"), "{forget_help}");

                    assert_eq!(prompt_script::prompts_asked(), 0);
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Allow);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `untrust <destination>` is its own question: declined it leaves the record
            /// and the Allow verdict in place; accepted it removes exactly that record and
            /// the verdict falls back to the closed default.
            #[test]
            #[serial]
            fn untrust_destination_confirms_then_removes_only_that_record() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-dest");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-dest").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);
                    let other = heard_trusted_peer(&started.runtime, slot);
                    let other_id = identity_of(&trust, &other);
                    let before = trust_file(&trust);

                    let declined = prompt_script::install(&[false]);
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest}"))
                        .await
                        .unwrap();
                    assert!(out.contains(NOTHING_CHANGED), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Allow);
                    drop(declined);

                    let _script = prompt_script::install(&[true]);
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest}"))
                        .await
                        .unwrap();
                    assert!(out.contains("Untrusted"), "{out}");
                    assert!(
                        out.contains(short(&dest)),
                        "the removed record is named: {out}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    let verdict = trust.authorize(&id, &dest);
                    assert_eq!(verdict.decision, Decision::Refuse);
                    assert_eq!(verdict.rule, Rule::DefaultClosed);
                    assert_eq!(
                        trust.authorize(&other_id, &other).decision,
                        Decision::Allow,
                        "the other instance keeps its record"
                    );
                    let instances: Vec<String> = trust
                        .records()
                        .into_iter()
                        .filter(|record| record.tier == Tier::Destination)
                        .map(|record| record.hash)
                        .collect();
                    assert_eq!(
                        instances,
                        vec![other.clone()],
                        "exactly the named instance record is gone"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Refusing one instance and blocking its identity are distinct standings with
            /// distinct words: an untrusted instance of a trusted-all identity is refused by
            /// the destination rule while its siblings stay trusted; a blocked identity is
            /// refused everywhere. Each declines to nothing, unblocking leaves the instance's
            /// refusal standing, and only `trust <destination>` lifts it.
            #[test]
            #[serial]
            fn untrust_of_a_trusted_all_instance_and_block_set_distinct_verdicts() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-vs-block");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-vs-block").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    let sibling = hex_lower(&[0x5c; 16]);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    assert!(before.is_some());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);

                    let declined = prompt_script::install(&[false, false]);
                    for line in [format!(".mesh untrust {dest}"), format!(".mesh block {id}")] {
                        let out = out_of(&mut ctx, &line).await.unwrap();
                        assert!(out.contains(NOTHING_CHANGED), "{line}: {out}");
                    }
                    assert_eq!(prompt_script::prompts_asked(), 2);
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);
                    drop(declined);

                    let _script = prompt_script::install(&[]);
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Refused"), "{out}");
                    assert!(
                        !out.contains("Blocked"),
                        "untrust reports a refusal, not a block: {out}"
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_eq!(
                        trust.authorize(&id, &sibling).rule,
                        Rule::IdentityTrusted,
                        "untrust stops one instance, not the identity"
                    );

                    let out = out_of(&mut ctx, &format!(".mesh block {id} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Blocked"), "{out}");
                    assert!(
                        !out.contains("Refused") && !out.contains("refused"),
                        "block never says refused: {out}"
                    );
                    assert_eq!(trust.authorize(&id, &sibling).rule, Rule::IdentityBlocked);

                    let out = out_of(&mut ctx, &format!(".mesh unblock {id} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Unblocked"), "{out}");
                    assert_eq!(
                        trust.authorize(&id, &dest).rule,
                        Rule::DestinationDenied,
                        "unblocking the identity does not lift the destination's deny"
                    );
                    assert_eq!(trust.authorize(&id, &sibling).rule, Rule::DefaultClosed);
                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        out.contains("The refusal on this instance is lifted."),
                        "{out}"
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    assert!(trust.denied().is_empty());
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Blocking a refused instance's identity takes its records and leaves the deny;
            /// after the unblock, `untrust <dest>` on that deny-only row names the refusal
            /// and the verb that lifts it rather than claiming there is nothing to untrust.
            #[test]
            #[serial]
            fn untrust_of_a_deny_only_row_is_a_teaching_error_naming_trust() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-deny-only");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-deny-only").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {dest} --yes"),
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let row = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the deny is still listed");
                    assert_eq!(row.identity, None, "block took the bound record");
                    let before = trust_file(&trust);

                    let err = refusal(&mut ctx, &format!(".mesh untrust {dest} --yes")).await;
                    assert_eq!(
                        err,
                        format!(
                            "Destination {dest} is refused and its identity is not trusted; `.mesh trust {dest}` lifts that once the peer is heard."
                        )
                    );
                    assert!(!err.contains("nothing to untrust"), "{err}");
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The `trust --identity` preview counts the instances the grant leaves refused,
            /// so the user is not surprised that a trusted-all identity still has a silent
            /// instance; with no refusal the preview says nothing about it.
            #[test]
            #[serial]
            fn trust_identity_preview_counts_the_instances_that_stay_refused() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-identity-refused-count");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-identity-refused-count").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    let clause = "instance(s) of this identity stay refused; `.mesh trust <destination>` lifts each.";

                    let out = out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    assert!(!out.contains("stay refused"), "{out}");
                    out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);

                    let out = out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let preview = out.lines().next().unwrap();
                    assert!(preview.starts_with("This trusts identity"), "{out}");
                    assert!(preview.ends_with(&format!(" 1 {clause}")), "{preview}");
                    assert_eq!(
                        trust.authorize(&id, &dest).rule,
                        Rule::DestinationDenied,
                        "re-trusting the identity lifts no refusal"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Without a terminal the refusal of a trusted-all instance is explained first and
            /// then refused for want of `--yes`, writing nothing: the preview is not consent.
            #[test]
            #[serial]
            fn untrust_of_a_trusted_all_instance_without_a_terminal_needs_yes_and_writes_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-refuse-non-tty");
                let _capture = capture::install();
                let _non_tty = prompt_script::install_non_interactive();
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-refuse-non-tty").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    let printed = stdout_lines().len();
                    let err = refusal(&mut ctx, &format!(".mesh untrust {dest}")).await;
                    assert!(err.contains("--yes"), "{err}");
                    assert_eq!(
                        stdout_lines()[printed..],
                        [format!(
                            "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`."
                        )]
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    assert_eq!(trust_file(&trust), before);
                    assert!(trust.denied().is_empty());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The sentence that says what an untrust of a trusted-all instance will do
            /// comes from a dry pass, so it is printed before the write; the write is the
            /// `Refused` line that follows it.
            #[test]
            #[serial]
            fn untrust_of_an_instance_under_a_trusted_identity_prints_the_sentence_before_it_writes()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-sentence-order");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-sentence-order").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    let lines = stdout_lines();
                    let sentence = format!(
                        "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`."
                    );
                    assert!(
                        index_of(&lines, &sentence) < index_of(&lines, "Refused "),
                        "{lines:?}"
                    );
                    assert!(lines.contains(&format!(
                        "Refused {}; `.mesh trust {dest}` lifts that.",
                        short(&dest)
                    )));
                    assert_ne!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn untrust_dry_run_prints_the_same_sentence_and_writes_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-dry-run");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-dry-run").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`.\nThis was a dry run; nothing changed."
                        )
                    );
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);
                    assert!(trust.denied().is_empty());
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn untrust_dry_run_on_a_bound_instance_prints_the_forget_preview_and_writes_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-bound-dry-run");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-bound-dry-run").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "This forgets trusted instance {}; the record for its identity stays.\nThis was a dry run; nothing changed.",
                            short(&dest)
                        )
                    );
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The `trust <dest>` preview says when it also lifts a standing refusal, so
            /// the question is answered knowing both effects; a declined prompt writes
            /// nothing.
            #[test]
            #[serial]
            fn trust_preview_says_it_lifts_a_standing_refusal() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-preview-lift");
                let _capture = capture::install();
                let _script = prompt_script::install(&[false]);
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-preview-lift").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let before = trust_file(&trust);

                    let out = out_of(&mut ctx, &format!(".mesh trust {dest}"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "This trusts instance {}: a peer you trust can message this node and ask it questions. This also lifts the refusal on it.\n{NOTHING_CHANGED}",
                            short(&dest)
                        )
                    );
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);

                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("This also lifts the refusal on it."), "{out}");
                    assert!(
                        out.contains("The refusal on this instance is lifted."),
                        "{out}"
                    );
                    assert!(trust.denied().is_empty());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    assert_eq!(prompt_script::prompts_asked(), 1);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// An instance trusted on its own record is forgotten, not refused: no deny
            /// row is written and the verdict falls back to the closed default.
            #[test]
            #[serial]
            fn untrust_of_a_bound_instance_forgets_it_as_before() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-bound");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-bound").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("This forgets trusted instance"), "{out}");
                    assert!(
                        out.ends_with(&format!("Untrusted {}.", short(&dest))),
                        "{out}"
                    );
                    assert!(!out.contains("refused"), "{out}");
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DefaultClosed);
                    assert!(trust.denied().is_empty());
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            #[test]
            #[serial]
            fn untrust_of_an_already_refused_instance_changes_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-twice");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-twice").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let before = trust_file(&trust);

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{} is already refused; its identity stays trusted.\n{NOTHING_CHANGED}",
                            short(&dest)
                        )
                    );
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{} is already refused; its identity stays trusted.\nThis was a dry run; nothing changed.",
                            short(&dest)
                        )
                    );
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// A legacy deny-only row (refuse → block → unblock → `trust --identity`) has
            /// no record for the repeat refusal to leave alone, so the dry run says the
            /// record would be bound to the heard identity and writes nothing.
            #[test]
            #[serial]
            fn untrust_dry_run_on_a_legacy_deny_only_row_says_the_record_would_be_bound_and_writes_nothing()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-backfill-dry-run");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-backfill-dry-run").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {dest} --yes"),
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                        format!(".mesh trust --identity {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    let legacy = |trust: &TrustStore| {
                        trust
                            .records()
                            .into_iter()
                            .find(|record| record.hash == dest)
                            .expect("the deny-only row is listed")
                    };
                    assert!(legacy(&trust).denied);
                    assert_eq!(legacy(&trust).identity, None);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let before = trust_file(&trust);

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{} is already refused; its record is bound to Tia again.\nThis was a dry run; nothing changed.",
                            short(&dest)
                        )
                    );
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(legacy(&trust).identity, None);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `forget` is `untrust` under another name: the same flags, the same dry-run
            /// token (still `untrust-<short>`), the same refusals, and the same final line.
            #[test]
            #[serial]
            fn forget_runs_the_untrust_arm_with_the_same_flags() {
                let _guard = TestConfigDirGuard::new("repl-mesh-forget-alias");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-forget-alias").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let dest = heard_trusted_peer(&started.runtime, slot);
                    let id = identity_of(&trust, &dest);
                    let before = trust_file(&trust);

                    let out = out_of(&mut ctx, &format!(".mesh forget --identity {id}"))
                        .await
                        .unwrap();
                    let last = out.lines().last().unwrap();
                    assert!(
                        last.contains(&format!(
                            ".mesh forget --identity {id} --confirm untrust-{}",
                            short(&id)
                        )),
                        "{out}"
                    );
                    assert_eq!(trust_file(&trust), before);

                    let err = refusal(&mut ctx, ".mesh forget --prune").await;
                    assert!(
                        err.starts_with("Unexpected '--prune'. Usage: .mesh forget"),
                        "{err}"
                    );
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out.lines().last(),
                        Some(format!("Untrusted {}.", short(&dest)).as_str()),
                        "{out}"
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DefaultClosed);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// With no destination record to read the identity from, the untrust finds it
            /// in the peer table and still writes the deny.
            #[test]
            #[serial]
            fn untrust_of_a_peer_table_only_instance_under_a_trusted_identity_refuses_it() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-peer-table");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-peer-table").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        !trust
                            .records()
                            .iter()
                            .any(|record| record.tier == Tier::Destination),
                        "the instance is known from the peer table alone"
                    );

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Refused"), "{out}");
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert!(trust.denied().iter().any(|record| record.hash == dest));
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: `undeny` and `fetch` are no longer verbs, and `deny` takes a
            /// file pattern. With the node ON and an identity trusted for all its
            /// instances, the withdrawn words are refused as unknown `.mesh` commands and a
            /// `deny` of a destination is sent to `untrust` (not the mesh-off teaching
            /// text, not a macro-path error); each asks nothing and leaves the trust file,
            /// the deny overlay and the verdict exactly as they were. The renamed and
            /// folded verbs are the ones that work in their place.
            #[test]
            #[serial]
            fn usage_probe_deny_undeny_and_fetch_are_withdrawn_verbs_that_touch_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-withdrawn-verbs");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-withdrawn-verbs").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    assert!(before.is_some());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);

                    let deny_teaching = format!(
                        "`.mesh deny` takes a file pattern, not a peer; to refuse the instance {} run `.mesh untrust {dest}`.",
                        short(&dest)
                    );
                    for (expected, line) in [
                        (deny_teaching.clone(), format!(".mesh deny {dest}")),
                        (deny_teaching.clone(), format!(".mesh deny {dest} --yes")),
                        (
                            "Unknown .mesh command 'undeny'".to_string(),
                            format!(".mesh undeny {dest}"),
                        ),
                        (
                            "Unknown .mesh command 'undeny'".to_string(),
                            format!(".mesh undeny {dest} --yes"),
                        ),
                        (
                            "Unknown .mesh command 'fetch'".to_string(),
                            ".mesh fetch".to_string(),
                        ),
                    ] {
                        let err = refusal(&mut ctx, &line).await;
                        assert!(err.starts_with(&expected), "{line}: {err}");
                        assert!(!err.contains(MESH_OFF), "{line}: {err}");
                        assert!(!err.contains("Unknown command. Type"), "{line}: {err}");
                        assert_eq!(trust_file(&trust), before, "{line}");
                        assert!(trust.denied().is_empty(), "{line}");
                        assert_eq!(
                            trust.authorize(&id, &dest).rule,
                            Rule::IdentityTrusted,
                            "{line}"
                        );
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    // Verb help for a withdrawn word falls back to the full list, which
                    // names the verbs that replaced them and none of the withdrawn ones.
                    for verb in ["undeny", "fetch"] {
                        assert_eq!(render_verb_help(verb), render_help(), "{verb}");
                    }
                    let help = render_help();
                    for present in [".mesh sync", ".mesh forget", ".mesh untrust", ".mesh block"] {
                        assert!(help.contains(present), "{present}: {help}");
                    }
                    for absent in [".mesh undeny", "undeny"] {
                        assert!(!help.contains(absent), "{absent}: {help}");
                    }
                    // A later file verb named `fetch` would get its own row; only the row is pinned absent.
                    assert!(!VERBS.iter().any(|(name, _, _)| *name == "fetch"));
                    assert!(
                        !crate::repl::REPL_COMMANDS
                            .iter()
                            .any(|command| command.name == ".mesh fetch")
                    );
                    assert!(
                        !KNOCK_REFUSAL_TAIL.contains("undeny"),
                        "{KNOCK_REFUSAL_TAIL}"
                    );
                    assert!(
                        !KNOCK_REFUSAL_TAIL.contains(".mesh deny"),
                        "{KNOCK_REFUSAL_TAIL}"
                    );
                    assert!(
                        KNOCK_REFUSAL_TAIL.contains("`.mesh trust <destination>`"),
                        "a denied destination is lifted by `trust`: {KNOCK_REFUSAL_TAIL}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: `forget` is `untrust` on the trusted-all path too. `--dry-run`
            /// prints the identity-stays-trusted sentence and writes nothing; the real run
            /// prints it and then writes the deny, leaving the identity and its sibling
            /// instances trusted; `trust <dest>` lifts the refusal; and `--identity` takes
            /// the SAME `untrust-<short>` token, which removes the identity record when it
            /// comes back.
            #[test]
            #[serial]
            fn usage_probe_forget_refuses_a_trusted_all_instance_like_untrust_and_takes_its_token()
            {
                let _guard = TestConfigDirGuard::new("repl-mesh-forget-trusted-all");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-forget-trusted-all").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    let sibling = hex_lower(&[0x5d; 16]);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust);
                    assert!(before.is_some());
                    let sentence = format!(
                        "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`."
                    );

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!("{sentence}\nThis was a dry run; nothing changed.")
                    );
                    assert_eq!(trust_file(&trust), before);
                    assert!(trust.denied().is_empty());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --yes"))
                        .await
                        .unwrap();
                    let lines: Vec<String> = out.lines().map(str::to_string).collect();
                    assert!(
                        index_of(&lines, &sentence) < index_of(&lines, "Refused "),
                        "{out}"
                    );
                    assert!(
                        lines.contains(&format!(
                            "Refused {}; `.mesh trust {dest}` lifts that.",
                            short(&dest)
                        )),
                        "{out}"
                    );
                    assert!(!out.contains("Blocked"), "{out}");
                    assert_ne!(trust_file(&trust), before);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_eq!(
                        trust.authorize(&id, &sibling).rule,
                        Rule::IdentityTrusted,
                        "forget stops one instance, not the identity"
                    );
                    assert_eq!(trust.denied().len(), 1);
                    assert_eq!(trust.denied()[0].hash, dest);

                    let before = trust_file(&trust);
                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("is already refused"), "{out}");
                    assert!(out.contains(NOTHING_CHANGED), "{out}");
                    assert_eq!(
                        trust_file(&trust),
                        before,
                        "a repeat refusal does not rewrite the file"
                    );

                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        out.contains("The refusal on this instance is lifted."),
                        "{out}"
                    );
                    assert!(trust.denied().is_empty());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);

                    let token = format!("untrust-{}", short(&id));
                    let before = trust_file(&trust);
                    let out = out_of(&mut ctx, &format!(".mesh forget --identity {id} --dry-run"))
                        .await
                        .unwrap();
                    assert!(out.contains("This was a dry run"), "{out}");
                    assert!(out.contains(&format!("--confirm {token}")), "{out}");
                    assert_eq!(trust_file(&trust), before);
                    let err =
                        refusal(&mut ctx, &format!(".mesh forget --identity {id} --yes")).await;
                    assert!(err.contains("--confirm untrust-"), "{err}");
                    assert!(err.contains(".mesh forget --identity"), "{err}");
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(
                        &mut ctx,
                        &format!(".mesh forget --identity {id} --confirm {token}"),
                    )
                    .await
                    .unwrap();
                    assert!(out.contains("Untrusted identity"), "{out}");
                    let hashes = trusted_hashes(&trust);
                    assert!(!hashes.contains(&id), "{hashes:?}");
                    assert!(!hashes.contains(&dest), "{hashes:?}");
                    assert_eq!(trust.authorize(&id, &sibling).rule, Rule::DefaultClosed);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Refusing an instance that has its own record keeps the record, flagged
            /// denied, with its label; `untrust --identity` then forgets the identity, the
            /// record and the refusal together, so the instance is merely heard again.
            #[test]
            #[serial]
            fn usage_probe_untrust_identity_forgets_the_refusals_of_its_instances() {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-identity-sweeps");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-identity-sweeps").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(
                        &mut ctx,
                        &format!(".mesh trust {dest} --yes --label \"Tia's desk\""),
                    )
                    .await
                    .unwrap();
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Refused"), "{out}");
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the refused instance keeps its record");
                    assert!(record.denied);
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));
                    assert_eq!(record.label.as_deref(), Some("Tia's desk"));
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);

                    let token = format!("untrust-{}", short(&id));
                    let dry = out_of(&mut ctx, &format!(".mesh untrust --identity {id}"))
                        .await
                        .unwrap();
                    let instance_row = dry
                        .lines()
                        .find(|line| line.contains(&format!("instance {dest}")))
                        .unwrap_or_else(|| panic!("no instance row in {dry}"));
                    assert!(instance_row.ends_with("  refused"), "{instance_row}");
                    assert_eq!(
                        dry.lines().last().unwrap(),
                        format!(
                            "This was a dry run; nothing changed. To forget this identity and its 1 instance(s), 1 of them refused (the refusal goes with the record), run: .mesh untrust --identity {id} --confirm {token}"
                        )
                    );
                    let out = out_of(
                        &mut ctx,
                        &format!(".mesh untrust --identity {id} --confirm {token}"),
                    )
                    .await
                    .unwrap();
                    assert!(out.contains("Untrusted identity"), "{out}");
                    assert!(
                        trust.denied().is_empty(),
                        "the refusal goes with the record"
                    );
                    let hashes = trusted_hashes(&trust);
                    assert!(!hashes.contains(&id), "{hashes:?}");
                    assert!(!hashes.contains(&dest), "{hashes:?}");
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DefaultClosed);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Refusing an instance never trusted on its own binds a record to the identity
            /// the peer table proves, so `untrust --identity` sweeps the refusal with the
            /// rest; afterwards `untrust <dest>` is truthfully "nothing to untrust".
            #[test]
            #[serial]
            fn usage_probe_untrust_identity_sweeps_the_refusal_of_a_peer_table_only_instance() {
                let _guard = TestConfigDirGuard::new("repl-mesh-peer-table-only-refusal");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-peer-table-only-refusal").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Refused"), "{out}");
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the refusal writes a record bound to the identity");
                    assert!(record.denied);
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));
                    assert_eq!(record.label, None);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);

                    let token = format!("untrust-{}", short(&id));
                    let out = out_of(
                        &mut ctx,
                        &format!(".mesh untrust --identity {id} --confirm {token}"),
                    )
                    .await
                    .unwrap();
                    assert!(out.contains("Untrusted identity"), "{out}");
                    assert!(
                        trust.denied().is_empty(),
                        "the refusal goes with the record"
                    );
                    assert!(
                        !trust.records().iter().any(|record| record.hash == dest),
                        "{:?}",
                        trust.records()
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DefaultClosed);
                    let before = trust_file(&trust);

                    let err = refusal(&mut ctx, &format!(".mesh untrust {dest} --yes")).await;
                    assert!(err.contains("not in the trust list"), "{err}");
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: refusing a labelled instance of a trusted-all identity and
            /// then trusting it again is a round trip the user can see end to end. While
            /// refused, `untrust <TAB>` stops offering it and `trust <TAB>` keeps it; the
            /// lift says so, keeps the label and the identity binding on the record, puts
            /// the row back under `untrust <TAB>` with that label, and a second `trust` of
            /// the now-plain record says nothing about a refusal. Nothing asks a question.
            #[test]
            #[serial]
            fn usage_probe_refuse_then_trust_round_trips_the_label_and_completion() {
                let _guard = TestConfigDirGuard::new("repl-mesh-refuse-lift-roundtrip");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-refuse-lift-roundtrip").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(
                        &mut ctx,
                        &format!(".mesh trust {dest} --yes --label \"Tia's desk\""),
                    )
                    .await
                    .unwrap();
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let offers =
                        |ctx: &RequestContext, verb: &str| -> Vec<(String, Option<String>)> {
                            ctx.repl_complete(".mesh", &[verb, ""], "")
                        };
                    let offered_as = |rows: &[(String, Option<String>)], value: &str| {
                        rows.iter()
                            .find(|(candidate, _)| candidate == value)
                            .map(|(_, description)| description.clone().unwrap_or_default())
                    };
                    assert!(
                        offered_as(&offers(&ctx, "untrust"), &dest).is_some_and(|text| text
                            .starts_with(&format!("Tia's desk . {} . trusted ", short(&dest)))),
                        "{:?}",
                        offers(&ctx, "untrust")
                    );

                    out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert!(offered_as(&offers(&ctx, "untrust"), &dest).is_none());
                    assert!(offered_as(&offers(&ctx, "forget"), &dest).is_none());
                    assert!(
                        offered_as(&offers(&ctx, "trust"), &dest).is_some(),
                        "`trust` can lift the refusal, so it still offers the instance"
                    );

                    let before = trust_file(&trust);
                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        out.contains("The refusal on this instance is lifted."),
                        "{out}"
                    );
                    assert_ne!(trust_file(&trust), before, "the lift is written");
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the lifted instance keeps its record");
                    assert!(!record.denied);
                    assert_eq!(record.label.as_deref(), Some("Tia's desk"));
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));
                    assert!(trust.denied().is_empty());
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    assert!(
                        offered_as(&offers(&ctx, "untrust"), &dest)
                            .is_some_and(|text| text.starts_with("Tia's desk . ")),
                        "{:?}",
                        offers(&ctx, "untrust")
                    );

                    let again = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        !again.contains("refusal"),
                        "a plain re-trust has no refusal to speak of: {again}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: with no record of its own, the instance's identity is read from
            /// the peer table only when that row proves the binding. A row whose identity
            /// column does not derive the destination (the identity of a different,
            /// trusted-all peer pasted in) is not a reason to refuse: `untrust <dest>` says
            /// the instance is not in the trust list, writes nothing, denies nothing, and
            /// the forged identity's own standing is untouched. `forget` behaves the same.
            #[test]
            #[serial]
            fn usage_probe_untrust_does_not_refuse_on_a_peer_row_that_does_not_prove_its_identity()
            {
                let _guard = TestConfigDirGuard::new("repl-mesh-untrust-forged-row");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-untrust-forged-row").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (_, impostor_id) = heard_peer(&started.runtime, "Impostor", now);
                    out_of(
                        &mut ctx,
                        &format!(".mesh trust --identity {impostor_id} --yes"),
                    )
                    .await
                    .unwrap();
                    let (dest, real_id, name_hash) = announced_peer();
                    started.runtime.peers().observe(
                        PeerSighting {
                            destination_hash: dest.clone(),
                            identity_hash: impostor_id.clone(),
                            name_hash,
                            display_name: Some("Tia".to_string()),
                            protocol_version: 1,
                            hops: 1,
                        },
                        now,
                    );
                    let before = trust_file(&trust);
                    assert!(before.is_some());

                    for verb in ["untrust", "forget"] {
                        let err = refusal(&mut ctx, &format!(".mesh {verb} {dest} --yes")).await;
                        assert!(err.contains("not in the trust list"), "{verb}: {err}");
                        assert!(!err.contains("refused"), "{verb}: {err}");
                    }
                    assert_eq!(trust_file(&trust), before);
                    assert!(trust.denied().is_empty());
                    assert_eq!(trust.authorize(&real_id, &dest).rule, Rule::DefaultClosed);
                    assert!(
                        trust.is_trusted_identity(&impostor_id),
                        "the pasted identity keeps its trusted-all standing"
                    );
                    assert!(
                        !stdout_lines()
                            .iter()
                            .any(|line| line.contains("stays trusted")),
                        "{:?}",
                        stdout_lines()
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: an instance known from the peer table alone, refused under its
            /// trusted-all identity, is visible as refused everywhere the user looks. It was
            /// never under `untrust`/`forget <TAB>` (no record), stays out while refused,
            /// `trust <TAB>` keeps offering it, `untrust --identity <TAB>` still offers the
            /// identity, `.mesh peers` labels the row `denied`; a dry run of `forget` on the
            /// record-less instance writes neither record nor deny. After `trust <dest>`
            /// lifts the refusal, `.mesh peers` says `trusted` and `untrust`/`forget <TAB>`
            /// offer the (unlabelled) record.
            #[test]
            #[serial]
            fn usage_probe_record_less_refusal_shows_in_peers_and_completion_until_lifted() {
                let _guard = TestConfigDirGuard::new("repl-mesh-record-less-refusal-visible");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-record-less-refusal-visible").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let offers = |ctx: &RequestContext, words: &[&str]| -> Vec<String> {
                        ctx.repl_complete(".mesh", words, "")
                            .into_iter()
                            .map(|(candidate, _)| candidate)
                            .collect()
                    };
                    async fn peers_row(ctx: &mut RequestContext) -> String {
                        let out = out_of(ctx, ".mesh peers").await.unwrap();
                        out.lines()
                            .find(|line| line.starts_with("Tia"))
                            .map(str::to_string)
                            .unwrap_or_else(|| panic!("no Tia row in {out}"))
                    }
                    assert!(!offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(!offers(&ctx, &["forget", ""]).contains(&dest));
                    assert!(offers(&ctx, &["trust", ""]).contains(&dest));
                    assert!(peers_row(&mut ctx).await.contains("trusted"));
                    let before = trust_file(&trust);

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`.\nThis was a dry run; nothing changed."
                        )
                    );
                    assert_eq!(
                        trust_file(&trust),
                        before,
                        "a dry run writes no record either"
                    );
                    assert!(
                        !trust.records().iter().any(|record| record.hash == dest),
                        "{:?}",
                        trust.records()
                    );
                    assert!(trust.denied().is_empty());

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(out.contains("Refused "), "{out}");
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("forget writes the binding it refuses, like untrust");
                    assert!(record.denied);
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));
                    assert_eq!(record.label, None);
                    let secs = |at: SystemTime| {
                        at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
                    };
                    assert_eq!(
                        secs(record.last_seen_at),
                        secs(now),
                        "the record carries the peer's sighting"
                    );

                    let row = peers_row(&mut ctx).await;
                    assert!(row.contains("denied"), "{row}");
                    assert!(!row.contains("trusted"), "{row}");
                    assert!(!offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(!offers(&ctx, &["forget", ""]).contains(&dest));
                    assert!(offers(&ctx, &["trust", ""]).contains(&dest));
                    assert!(
                        offers(&ctx, &["untrust", "--identity", ""]).contains(&id),
                        "the identity stays trusted, so it stays under `untrust --identity`"
                    );
                    assert!(
                        offers(&ctx, &["block", ""]).contains(&id),
                        "a refusal is not a block, so `block` still offers the identity"
                    );
                    assert!(!offers(&ctx, &["unblock", ""]).contains(&id));

                    let out = out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        out.contains("The refusal on this instance is lifted."),
                        "{out}"
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    let row = peers_row(&mut ctx).await;
                    assert!(row.contains("trusted"), "{row}");
                    assert!(!row.contains("denied"), "{row}");
                    for verb in ["untrust", "forget"] {
                        let rows = ctx.repl_complete(".mesh", &[verb, ""], "");
                        let description = rows
                            .iter()
                            .find(|(candidate, _)| *candidate == dest)
                            .and_then(|(_, description)| description.clone())
                            .unwrap_or_else(|| {
                                panic!("{verb} <TAB> offers the lifted record: {rows:?}")
                            });
                        assert!(
                            description.contains(short(&dest)) && description.contains("trusted "),
                            "{verb}: {description}"
                        );
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: `trust --prune` leaves a refused instance alone through the REPL.
            /// A refused record whose sighting is older than the horizon is neither listed
            /// by the dry run nor counted in the prune token (a token counting it is refused
            /// as stale), and the confirmed prune removes only the stale trusted sibling;
            /// the refusal survives, still answering `DestinationDenied`.
            #[test]
            #[serial]
            fn usage_probe_prune_skips_a_refused_instance_through_the_repl() {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-prune-refused");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-prune-refused").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let long_ago = SystemTime::now() - Duration::from_secs(40 * 86_400);
                    let (stale, _) = heard_peer(&started.runtime, "Old", long_ago);
                    trust
                        .trust_destination(slot, &stale, TrustOptions::default(), long_ago)
                        .unwrap();
                    let (refused, refused_id) = heard_peer(&started.runtime, "Gone", long_ago);
                    for line in [
                        format!(".mesh trust --identity {refused_id} --yes"),
                        format!(".mesh untrust {refused} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    let refused_record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == refused)
                        .expect("the refusal wrote a bound record");
                    assert!(refused_record.denied);
                    assert!(
                        SystemTime::now()
                            .duration_since(refused_record.last_seen_at)
                            .unwrap()
                            > Duration::from_secs(30 * 86_400),
                        "the refused record is as stale as the sibling, so only the refusal keeps it"
                    );
                    let before = trust_file(&trust);

                    for line in [".mesh trust --prune", ".mesh trust --prune --dry-run"] {
                        let out = out_of(&mut ctx, line).await.unwrap();
                        assert!(out.contains(&stale), "{line}: {out}");
                        assert!(
                            !out.contains(&refused),
                            "{line}: a refused instance is not listed for pruning: {out}"
                        );
                        assert!(
                            out.contains(
                                "To remove these 1 instance(s), run: .mesh trust --prune --confirm prune-1"
                            ),
                            "{line}: the token counts the stale sibling only: {out}"
                        );
                        assert_eq!(trust_file(&trust), before, "{line}");
                    }
                    let err = refusal(&mut ctx, ".mesh trust --prune --confirm prune-2").await;
                    assert!(err.contains("has changed since the dry run"), "{err}");
                    assert_eq!(trust_file(&trust), before);

                    let out = out_of(&mut ctx, ".mesh trust --prune --confirm prune-1")
                        .await
                        .unwrap();
                    assert!(out.contains("Removed 1"), "{out}");
                    assert!(out.contains(&stale), "{out}");
                    assert!(!out.contains(&refused), "{out}");
                    let hashes = trusted_hashes(&trust);
                    assert!(!hashes.contains(&stale), "{hashes:?}");
                    assert!(hashes.contains(&refused), "{hashes:?}");
                    assert_eq!(
                        trust.authorize(&refused_id, &refused).rule,
                        Rule::DestinationDenied
                    );
                    assert_eq!(trust.denied().len(), 1);
                    let out = out_of(&mut ctx, ".mesh trust --prune").await.unwrap();
                    assert!(out.contains("nothing to prune"), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: the teaching error on a deny-only row (refuse → block → unblock)
            /// names a remedy that works. `forget <dest>` without `--yes` says the same
            /// sentence as `untrust`, asks nothing and writes nothing; `.mesh trust <dest>`
            /// with the peer heard lifts the deny, binds the record to the heard identity,
            /// and the instance answers trusted again; `.mesh peers` and `untrust <TAB>`
            /// agree.
            #[test]
            #[serial]
            fn usage_probe_trust_lifts_the_deny_only_row_the_teaching_error_names() {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-deny-only-lift");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-deny-only-lift").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {dest} --yes"),
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert!(
                        trust
                            .records()
                            .iter()
                            .all(|record| record.identity.is_none()),
                        "{:?}",
                        trust.records()
                    );
                    let before = trust_file(&trust);
                    let teaching = format!(
                        "Destination {dest} is refused and its identity is not trusted; `.mesh trust {dest}` lifts that once the peer is heard."
                    );

                    for line in [
                        format!(".mesh forget {dest}"),
                        format!(".mesh forget {dest} --dry-run"),
                        format!(".mesh untrust {dest} --dry-run"),
                    ] {
                        assert_eq!(refusal(&mut ctx, &line).await, teaching, "{line}");
                    }
                    assert_eq!(trust_file(&trust), before);
                    assert_eq!(prompt_script::prompts_asked(), 0, "a refusal asks nothing");
                    let offers = |ctx: &RequestContext, words: &[&str]| -> Vec<String> {
                        ctx.repl_complete(".mesh", words, "")
                            .into_iter()
                            .map(|(candidate, _)| candidate)
                            .collect()
                    };
                    assert!(!offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(!offers(&ctx, &["forget", ""]).contains(&dest));
                    assert!(offers(&ctx, &["trust", ""]).contains(&dest));

                    out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    assert!(trust.denied().is_empty(), "{:?}", trust.denied());
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the lift wrote the record");
                    assert!(!record.denied);
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));
                    let out = out_of(&mut ctx, ".mesh peers").await.unwrap();
                    let row = out
                        .lines()
                        .find(|line| line.starts_with("Tia"))
                        .unwrap_or_else(|| panic!("no Tia row in {out}"));
                    assert!(row.contains("trusted") && !row.contains("denied"), "{row}");
                    assert!(offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(offers(&ctx, &["forget", ""]).contains(&dest));
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: a legacy deny-only row under a trusted-all identity (the
            /// refuse → block → unblock → `trust --identity` shape) repairs itself through
            /// the REPL: a repeat `untrust <dest>` back-fills the bound record in the same
            /// commit as the deny, so the `trust --identity` preview counts the instance that
            /// stays refused and `untrust --identity` later takes the deny with the record.
            #[test]
            #[serial]
            fn usage_probe_repeat_refusal_through_the_repl_backfills_a_legacy_deny_only_row() {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-backfill");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-backfill").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {dest} --yes"),
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                        format!(".mesh trust --identity {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    let legacy = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the deny-only row is listed");
                    assert!(legacy.denied);
                    assert_eq!(
                        legacy.identity, None,
                        "legacy shape: a deny without a record"
                    );
                    assert_eq!(
                        trust.authorize(&id, &dest).rule,
                        Rule::DestinationDenied,
                        "under the trusted-all identity the instance stays refused"
                    );

                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest}"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{short} is already refused; its record is bound to Tia again.\nBound {short}'s record to Tia; it stays refused.",
                            short = short(&dest)
                        )
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0, "{out}");
                    let backfilled = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .expect("the row is still listed");
                    assert!(backfilled.denied, "{out}");
                    assert_eq!(
                        backfilled.identity.as_deref(),
                        Some(id.as_str()),
                        "a repeat refusal back-fills the binding: {out}"
                    );
                    assert_eq!(backfilled.label, None);
                    let secs = |at: SystemTime| {
                        at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
                    };
                    assert_eq!(secs(backfilled.last_seen_at), secs(now));
                    assert_eq!(trust.denied().len(), 1);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);

                    let out = out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    let preview = out.lines().next().unwrap();
                    assert!(
                        preview.ends_with(
                            " 1 instance(s) of this identity stay refused; `.mesh trust <destination>` lifts each."
                        ),
                        "{out}"
                    );
                    let dry = out_of(&mut ctx, &format!(".mesh untrust --identity {id}"))
                        .await
                        .unwrap();
                    assert!(
                        dry.lines()
                            .any(|line| line.contains(&format!("instance {dest}"))
                                && line.ends_with("  refused")),
                        "{dry}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Re-binding a legacy deny-only row from the verb is bookkeeping: the REPL
            /// asks nothing, says what it bound, grants nothing and fires no `mesh.trust.*`
            /// hook; the next repeat on the now-bound row is the plain no-op that rewrites
            /// no file.
            #[test]
            #[serial]
            fn usage_probe_rebind_through_the_repl_asks_nothing_fires_nothing_and_then_repeats_as_a_no_op()
             {
                use crate::mesh::events::{MeshHooks, RecordingHookSink, TrustHookObserver};

                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-rebind-silent");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-rebind-silent").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {dest} --yes"),
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                        format!(".mesh trust --identity {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    let row = |trust: &TrustStore| {
                        trust
                            .records()
                            .into_iter()
                            .find(|record| record.hash == dest)
                            .expect("the row is listed")
                    };
                    assert!(row(&trust).denied && row(&trust).identity.is_none());
                    let hooks = MeshHooks::default();
                    let sink = RecordingHookSink::attach(&hooks);
                    trust.set_observer(Arc::new(TrustHookObserver(hooks)));

                    // Bare form: with no `--yes`, a prompt here would be the failure.
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest}"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{short} is already refused; its record is bound to Tia again.\nBound {short}'s record to Tia; it stays refused.",
                            short = short(&dest)
                        )
                    );
                    assert_eq!(
                        prompt_script::prompts_asked(),
                        0,
                        "re-binding is not asked about"
                    );
                    assert!(
                        sink.drain().is_empty(),
                        "re-binding grants nothing and fires nothing"
                    );
                    assert_eq!(row(&trust).identity.as_deref(), Some(id.as_str()));
                    assert!(row(&trust).denied);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let bound = trust_file(&trust);

                    // Now that the record is bound, the same command is the plain repeat.
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest}"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{} is already refused; its identity stays trusted.\n{NOTHING_CHANGED}",
                            short(&dest)
                        )
                    );
                    assert_eq!(
                        trust_file(&trust),
                        bound,
                        "a repeat on a bound row rewrites nothing"
                    );
                    assert!(sink.drain().is_empty());
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `forget` re-binds a legacy deny-only row exactly as `untrust` does — dry run
            /// first, then the write — the refused instance stays out of `untrust`/`forget`
            /// completion, and forgetting the identity afterwards takes the record AND its
            /// deny with it, so no orphan deny survives.
            #[test]
            #[serial]
            fn usage_probe_forget_rebinds_a_legacy_deny_only_row_and_untrust_identity_then_takes_the_deny()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-forget-rebind");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-forget-rebind").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {dest} --yes"),
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                        format!(".mesh trust --identity {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    let row = |trust: &TrustStore| {
                        trust
                            .records()
                            .into_iter()
                            .find(|record| record.hash == dest)
                    };
                    assert_eq!(row(&trust).unwrap().identity, None, "legacy deny-only row");
                    let before = trust_file(&trust);
                    let offers = |ctx: &RequestContext, words: &[&str]| -> Vec<String> {
                        ctx.repl_complete(".mesh", words, "")
                            .into_iter()
                            .map(|(candidate, _)| candidate)
                            .collect()
                    };

                    let out = out_of(&mut ctx, &format!(".mesh forget {dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{} is already refused; its record is bound to Tia again.\nThis was a dry run; nothing changed.",
                            short(&dest)
                        )
                    );
                    assert_eq!(trust_file(&trust), before, "a dry run writes nothing");
                    assert_eq!(row(&trust).unwrap().identity, None);

                    // Bare form: with no `--yes`, a prompt here would be the failure.
                    let out = out_of(&mut ctx, &format!(".mesh forget {dest}"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{short} is already refused; its record is bound to Tia again.\nBound {short}'s record to Tia; it stays refused.",
                            short = short(&dest)
                        )
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    let bound = row(&trust).unwrap();
                    assert!(bound.denied);
                    assert_eq!(bound.identity.as_deref(), Some(id.as_str()));
                    assert_eq!(bound.label, None);
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    // Completion never offers what the verb refuses: the refused instance
                    // stays off `untrust`/`forget` and on `trust`.
                    assert!(!offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(!offers(&ctx, &["forget", ""]).contains(&dest));
                    assert!(offers(&ctx, &["trust", ""]).contains(&dest));

                    // Forgetting the identity takes the bound record and its deny together.
                    let out = out_of(
                        &mut ctx,
                        &format!(
                            ".mesh forget --identity {id} --confirm untrust-{}",
                            short(&id)
                        ),
                    )
                    .await
                    .unwrap();
                    assert!(row(&trust).is_none(), "{out}");
                    assert!(
                        trust.denied().is_empty(),
                        "no orphan deny outlives the identity: {:?} / {out}",
                        trust.denied()
                    );
                    assert!(!trust.is_trusted_identity(&id));
                    assert_ne!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert!(!offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(!offers(&ctx, &["untrust", "--identity", ""]).contains(&id));
                    assert_eq!(
                        refusal(&mut ctx, &format!(".mesh untrust {dest}")).await,
                        format!(
                            "Destination {dest} is not in the trust list, so there is nothing to untrust."
                        )
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The l′ sentence names the identity whose trust stays. When the peer table's
            /// row for the destination has since been re-bound to another identity (a key
            /// change the record does not follow), that row proves nothing about the trusted
            /// identity and may not lend it a display name: the sentence falls back to the
            /// identity's short hash.
            #[test]
            #[serial]
            fn usage_probe_refusal_sentence_never_borrows_a_name_from_a_peer_row_bound_to_another_identity()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-refusal-name");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-refusal-name").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();

                    // While the peer row proves the trusted identity, its name is used.
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`.\nRefused {}; `.mesh trust {dest}` lifts that.",
                            short(&dest)
                        )
                    );
                    // Lift it on the intact, unlabelled record.
                    out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .unwrap();
                    assert_eq!(record.label, None);
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));

                    // The peer table now shows the same destination under another identity.
                    let (_, other, other_name_hash) = announced_peer();
                    started.runtime.peers().observe(
                        PeerSighting {
                            destination_hash: dest.clone(),
                            identity_hash: other.clone(),
                            name_hash: other_name_hash,
                            display_name: Some("Mallory".to_string()),
                            protocol_version: 1,
                            hops: 1,
                        },
                        SystemTime::now(),
                    );
                    assert_eq!(
                        started.runtime.peers().get(&dest).unwrap().identity_hash,
                        other
                    );

                    for line in [
                        format!(".mesh untrust {dest} --dry-run"),
                        format!(".mesh forget {dest} --dry-run"),
                        format!(".mesh untrust {dest} --yes"),
                    ] {
                        let out = out_of(&mut ctx, &line).await.unwrap();
                        let first = out.lines().next().unwrap();
                        assert_eq!(
                            first,
                            format!(
                                "{}'s identity stays trusted; this instance is refused until `.mesh trust {dest}`.",
                                short(&id)
                            ),
                            "{line}: {out}"
                        );
                        assert!(
                            !out.contains("Mallory") && !out.contains("Tia"),
                            "{line}: {out}"
                        );
                    }
                    // The refusal binds to the record's identity, not the re-bound row's.
                    let record = trust
                        .records()
                        .into_iter()
                        .find(|record| record.hash == dest)
                        .unwrap();
                    assert!(record.denied);
                    assert_eq!(record.identity.as_deref(), Some(id.as_str()));
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: the one legacy shape the withdrawn `.mesh deny` verb could leave —
            /// a denied record under an identity that is NOT trusted for all — is forgotten
            /// whole from the verb: `--dry-run` previews and writes nothing, the consented
            /// write takes the record AND its deny in one commit (one `mesh.trust.revoked`,
            /// no grant), so no orphan deny survives and the next `untrust` says the
            /// destination is not in the list at all.
            #[test]
            #[serial]
            fn usage_probe_untrust_of_a_denied_record_under_a_plain_identity_forgets_the_deny_through_the_repl()
             {
                use crate::hooks::HookEvent;
                use crate::mesh::events::{
                    MeshHooks, RecordingHookSink, TrustHookObserver, env_value, one_fire,
                };

                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-plain-denied-forget");
                let _capture = capture::install();
                run_async(async {
                    let started =
                        started_runtime("repl-mesh-usage-probe-plain-denied-forget").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        trust
                            .records()
                            .iter()
                            .all(|record| !(record.hash == id && record.all_destinations)),
                        "the identity is not trusted for all: destination tier only"
                    );
                    // The legacy shape: a deny laid over a record under a plain identity.
                    trust
                        .deny_destination(ctx.app.mesh.as_ref(), &dest, None, now)
                        .unwrap();
                    let row = |trust: &TrustStore| {
                        trust
                            .records()
                            .into_iter()
                            .find(|record| record.hash == dest)
                    };
                    assert!(row(&trust).unwrap().denied);
                    assert_eq!(row(&trust).unwrap().identity.as_deref(), Some(id.as_str()));
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    let before = trust_file(&trust).expect("the record is on disk");
                    let hooks = MeshHooks::default();
                    let sink = RecordingHookSink::attach(&hooks);
                    trust.set_observer(Arc::new(TrustHookObserver(hooks)));
                    let offers = |ctx: &RequestContext, words: &[&str]| -> Vec<String> {
                        ctx.repl_complete(".mesh", words, "")
                            .into_iter()
                            .map(|(candidate, _)| candidate)
                            .collect()
                    };
                    // (Whether `untrust <TAB>` offers a denied record under a plain identity is
                    // a deferred completion question; the verb's behaviour is what is pinned.)

                    // Dry run under both spellings: the preview, then nothing written.
                    let _script = prompt_script::install(&[]);
                    for verb in ["untrust", "forget"] {
                        let out = out_of(&mut ctx, &format!(".mesh {verb} {dest} --dry-run"))
                            .await
                            .unwrap();
                        assert_eq!(
                            out,
                            format!(
                                "This forgets trusted instance {}; the record for its identity stays.\n{DRY_RUN_NOTHING_CHANGED}",
                                short(&dest)
                            ),
                            "{verb}"
                        );
                    }
                    assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                    assert!(row(&trust).unwrap().denied);
                    assert_eq!(trust.denied().len(), 1);
                    assert!(sink.drain().is_empty(), "a dry run fires nothing");
                    assert_eq!(prompt_script::prompts_asked(), 0, "a dry run asks nothing");
                    drop(_script);

                    // Consented through the alias: record and deny go in the same write.
                    let _script = prompt_script::install(&[true]);
                    let out = out_of(&mut ctx, &format!(".mesh forget {dest}"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "This forgets trusted instance {short}; the record for its identity stays.\nUntrusted {short}.",
                            short = short(&dest)
                        )
                    );
                    assert_eq!(prompt_script::prompts_asked(), 1, "forgetting is consented");
                    assert!(row(&trust).is_none(), "the record is gone");
                    assert!(
                        trust.denied().is_empty(),
                        "no orphan deny survives the record: {:?}",
                        trust.denied()
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DefaultClosed);
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Refuse);
                    let envs = one_fire(&sink, HookEvent::MeshTrustRevoked);
                    assert_eq!(
                        env_value(&envs, "COYOTE_MESH_TRUST_TIER"),
                        Some("destination")
                    );
                    assert_eq!(
                        env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
                        Some(dest.as_str())
                    );
                    assert_ne!(trust_file(&trust).as_deref(), Some(before.as_slice()));

                    // Afterwards the destination is simply not in the list — not "refused".
                    assert_eq!(
                        refusal(&mut ctx, &format!(".mesh untrust {dest}")).await,
                        format!(
                            "Destination {dest} is not in the trust list, so there is nothing to untrust."
                        )
                    );
                    assert!(!offers(&ctx, &["untrust", ""]).contains(&dest));
                    assert!(!offers(&ctx, &["forget", ""]).contains(&dest));
                    assert!(
                        offers(&ctx, &["trust", ""]).contains(&dest),
                        "still heard, so `trust` offers it"
                    );
                    assert!(sink.drain().is_empty());

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: forgetting a denied record drops only ITS deny — a sibling's deny
            /// is left alone — and the rule holds for a record that lives in the session only:
            /// its on-disk deny is still taken, which is a file write even though the record
            /// itself was never on disk.
            #[test]
            #[serial]
            fn usage_probe_forgetting_a_denied_record_takes_only_its_own_deny_even_for_a_session_record()
             {
                use crate::hooks::HookEvent;
                use crate::mesh::events::{
                    MeshHooks, RecordingHookSink, TrustHookObserver, one_fire,
                };

                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-sibling-deny");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-sibling-deny").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (disk_dest, disk_id) = heard_peer(&started.runtime, "Tia", now);
                    let (session_dest, session_id) = heard_peer(&started.runtime, "Bea", now);
                    out_of(&mut ctx, &format!(".mesh trust {disk_dest} --yes"))
                        .await
                        .unwrap();
                    trust
                        .trust_destination_for_session(ctx.app.mesh.as_ref(), &session_dest, now)
                        .unwrap();
                    trust
                        .deny_destination(ctx.app.mesh.as_ref(), &disk_dest, None, now)
                        .unwrap();
                    trust
                        .deny_destination(ctx.app.mesh.as_ref(), &session_dest, None, now)
                        .unwrap();
                    let row = |trust: &TrustStore, hash: &str| {
                        trust
                            .records()
                            .into_iter()
                            .find(|record| record.hash == hash)
                    };
                    assert!(row(&trust, &disk_dest).unwrap().denied);
                    assert!(row(&trust, &session_dest).unwrap().denied);
                    assert_eq!(trust.denied().len(), 2);
                    assert_eq!(
                        trust.authorize(&session_id, &session_dest).rule,
                        Rule::DestinationDenied
                    );
                    let hooks = MeshHooks::default();
                    let sink = RecordingHookSink::attach(&hooks);
                    trust.set_observer(Arc::new(TrustHookObserver(hooks)));

                    // Forgetting the disk record leaves the sibling's deny where it is.
                    let out = out_of(&mut ctx, &format!(".mesh untrust {disk_dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        out.ends_with(&format!("Untrusted {}.", short(&disk_dest))),
                        "{out}"
                    );
                    assert!(row(&trust, &disk_dest).is_none());
                    assert_eq!(
                        trust.authorize(&disk_id, &disk_dest).rule,
                        Rule::DefaultClosed
                    );
                    let denied: Vec<String> = trust
                        .denied()
                        .into_iter()
                        .map(|overlay| overlay.hash)
                        .collect();
                    assert_eq!(denied, [session_dest.as_str()], "only its own deny went");
                    assert!(row(&trust, &session_dest).unwrap().denied);
                    assert_eq!(
                        trust.authorize(&session_id, &session_dest).rule,
                        Rule::DestinationDenied
                    );
                    one_fire(&sink, HookEvent::MeshTrustRevoked);
                    let between = trust_file(&trust).expect("the file holds the sibling deny");

                    // The session record never reached the file; its deny did, and goes too.
                    let out = out_of(&mut ctx, &format!(".mesh untrust {session_dest} --dry-run"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "This forgets trusted instance {}; the record for its identity stays.\n{DRY_RUN_NOTHING_CHANGED}",
                            short(&session_dest)
                        )
                    );
                    assert_eq!(trust_file(&trust).as_deref(), Some(between.as_slice()));
                    assert!(sink.drain().is_empty());
                    let out = out_of(&mut ctx, &format!(".mesh forget {session_dest} --yes"))
                        .await
                        .unwrap();
                    assert!(
                        out.ends_with(&format!("Untrusted {}.", short(&session_dest))),
                        "{out}"
                    );
                    assert!(row(&trust, &session_dest).is_none());
                    assert!(
                        trust.denied().is_empty(),
                        "the session record's on-disk deny went with it: {:?}",
                        trust.denied()
                    );
                    assert_eq!(
                        trust.authorize(&session_id, &session_dest).rule,
                        Rule::DefaultClosed
                    );
                    assert_ne!(
                        trust_file(&trust).as_deref(),
                        Some(between.as_slice()),
                        "dropping the deny is a file write"
                    );
                    one_fire(&sink, HookEvent::MeshTrustRevoked);
                    assert_eq!(
                        prompt_script::prompts_asked(),
                        0,
                        "--yes stood in for every question"
                    );
                    assert_eq!(
                        refusal(&mut ctx, &format!(".mesh untrust {session_dest}")).await,
                        format!(
                            "Destination {session_dest} is not in the trust list, so there is nothing to untrust."
                        )
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// A knock is only proof when its hashes derive the destination: one cached
            /// before its name hash was kept and one whose identity does not derive the
            /// destination are both refused with a teaching text, and nothing is written.
            #[test]
            #[serial]
            fn trust_refuses_a_knock_without_proof_through_the_repl() {
                use crate::mesh::knocks::KNOCK_RECORD_VERSION;
                use crate::mesh::rfc3339_utc;

                let _guard = TestConfigDirGuard::new("repl-mesh-trust-knock-proof");
                let _capture = capture::install();
                let _script = prompt_script::install(&[true; 4]);
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-knock-proof").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let gate = started.runtime.knock_gate();

                    let (legacy_dest, legacy_id, _) = announced_peer();
                    let (forged_dest, _, forged_name_hash) = announced_peer();
                    let (_, wrong_id, _) = announced_peer();
                    for (dest, id, name_hash, name) in [
                        (&legacy_dest, &legacy_id, String::new(), "Old"),
                        (&forged_dest, &wrong_id, forged_name_hash, "Forger"),
                    ] {
                        gate.cache()
                            .append(
                                KnockRecord {
                                    version: KNOCK_RECORD_VERSION,
                                    received_at: rfc3339_utc(now),
                                    identity_hash: id.clone(),
                                    destination_hash: dest.clone(),
                                    name_hash,
                                    display_name: Some(name.to_string()),
                                    intro: None,
                                    hops: 1,
                                },
                                now,
                            )
                            .unwrap();
                    }

                    for flag in ["", " --yes"] {
                        let err =
                            refusal(&mut ctx, &format!(".mesh trust {legacy_dest}{flag}")).await;
                        assert!(
                            err.contains("knocked before its name hash was kept"),
                            "a knock without its name hash teaches what to wait for: {err}"
                        );
                        assert!(!err.contains("added"), "{err}");
                        let err =
                            refusal(&mut ctx, &format!(".mesh trust {forged_dest}{flag}")).await;
                        assert!(
                            err.contains(&forged_dest) && !err.contains("added"),
                            "a forged knock is refused naming the destination: {err}"
                        );
                    }
                    assert!(trust_file(&trust).is_none(), "nothing was written");
                    assert_eq!(
                        trust.authorize(&legacy_id, &legacy_dest).decision,
                        Decision::Refuse
                    );
                    assert_eq!(
                        trust.authorize(&wrong_id, &forged_dest).decision,
                        Decision::Refuse
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// `trust --identity` is a mutation like the rest: without a terminal it refuses
            /// naming the flag, and declined at the prompt it writes nothing.
            #[test]
            #[serial]
            fn trust_identity_without_a_terminal_or_declined_writes_nothing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-trust-identity-refused");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-trust-identity-refused").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());

                    let non_tty = prompt_script::install_non_interactive();
                    let err = refusal(&mut ctx, &format!(".mesh trust --identity {id}")).await;
                    assert!(err.contains("--yes"), "{err}");
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    drop(non_tty);

                    let _script = prompt_script::install(&[false]);
                    let out = out_of(&mut ctx, &format!(".mesh trust --identity {id}"))
                        .await
                        .unwrap();
                    assert!(out.contains(NOTHING_CHANGED), "{out}");
                    assert_eq!(prompt_script::prompts_asked(), 1);
                    assert!(trust_file(&trust).is_none());
                    assert_eq!(trust.authorize(&id, &dest).decision, Decision::Refuse);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// A runtime joined to a fake propagation node that has announced itself.
            async fn runtime_with_fake_node(tag: &str) -> (FakeNode, StartedRuntime) {
                // A `MeshRuntime` joins with `TcpClient`'s default MTU, so the fake matches it.
                let fake = FakeNode::listen_with_mtu(TcpServer::DEFAULT_CLIENT_MTU).await;
                let started = started_runtime_on(tag, fake.listener.port).await;
                fake.announce().await;
                let runtime = started.runtime.clone();
                wait_until("the runtime to file the propagation node", || {
                    runtime.propagation_nodes().select().is_ok()
                })
                .await;
                (fake, started)
            }

            #[test]
            #[serial]
            fn sync_reports_the_counts_from_the_node() {
                let _guard = TestConfigDirGuard::new("repl-mesh-fetch");
                let _capture = capture::install();
                run_async(async {
                    let (fake, started) = runtime_with_fake_node("repl-mesh-fetch").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    fake.script.reply_with([Value::Array(vec![])]);

                    run(&mut ctx, ".mesh sync").await.unwrap();

                    let lines = stdout_lines();
                    let asking = index_of(
                        &lines,
                        "Asking the nearest propagation node for held messages; Ctrl-C cancels...",
                    );
                    let nothing = index_of(
                        &lines,
                        &format!("Nothing held for this node at {}.", short(&fake.hex())),
                    );
                    assert!(asking < nothing, "{lines:?}");
                    assert_eq!(fake.script.seen().len(), 1);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                    fake.stop().await;
                });
            }

            #[test]
            #[serial]
            fn sync_while_a_sync_is_running_prints_one_line_and_succeeds() {
                let _guard = TestConfigDirGuard::new("repl-mesh-fetch-busy");
                let _capture = capture::install();
                run_async(async {
                    let (fake, started) = runtime_with_fake_node("repl-mesh-fetch-busy").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    // Nothing scripted: the fake stays silent and the fetch waits in round 1.
                    let blocked = tokio::spawn({
                        let runtime = started.runtime.clone();
                        async move { runtime.fetch_propagated(&LoggingInboundSink).await }
                    });
                    wait_until("the blocked fetch to reach round 1", || {
                        fake.script.seen().len() == 1
                    })
                    .await;

                    run(&mut ctx, ".mesh sync").await.unwrap();

                    let lines = stdout_lines();
                    index_of(&lines, "A sync is already running; wait for it to finish.");
                    assert!(
                        !lines.iter().any(|line| line.contains("Nothing held")),
                        "{lines:?}"
                    );

                    blocked.abort();
                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                    fake.stop().await;
                });
            }

            #[test]
            #[serial]
            fn sync_with_no_node_heard_is_a_teaching_error() {
                let _guard = TestConfigDirGuard::new("repl-mesh-fetch-no-node");
                run_async(async {
                    let started = started_runtime("repl-mesh-fetch-no-node").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();

                    let err = refusal(&mut ctx, ".mesh sync").await;
                    assert!(
                        err.starts_with("No LXMF propagation node has announced itself"),
                        "{err}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: `announce: false` turns the AUTOMATIC sync
            /// off; `.mesh sync` is still a working manual trigger under that config.
            #[test]
            #[serial]
            fn usage_probe_sync_by_hand_works_while_announce_is_false() {
                let _guard = TestConfigDirGuard::new("repl-mesh-fetch-quiet");
                let _capture = capture::install();
                run_async(async {
                    let (fake, started) = runtime_with_fake_node("repl-mesh-fetch-quiet").await;
                    let mut ctx = ctx_with(
                        MeshConfig {
                            announce: false,
                            ..MeshConfig::default()
                        },
                        true,
                    );
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    fake.script.reply_with([Value::Array(vec![])]);

                    run(&mut ctx, ".mesh sync").await.unwrap();

                    let lines = stdout_lines();
                    index_of(
                        &lines,
                        &format!("Nothing held for this node at {}.", short(&fake.hex())),
                    );
                    assert_eq!(fake.script.seen().len(), 1, "one fetch went to the node");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                    fake.stop().await;
                });
            }

            /// An interval of 0 turns the automatic sync off; `.mesh sync` still fetches.
            #[test]
            #[serial]
            fn sync_by_hand_works_while_the_interval_is_zero() {
                let _guard = TestConfigDirGuard::new("repl-mesh-fetch-zero");
                let _capture = capture::install();
                run_async(async {
                    let (fake, started) = runtime_with_fake_node("repl-mesh-fetch-zero").await;
                    let mut ctx = ctx_with(
                        MeshConfig {
                            propagation_sync_interval_secs: 0,
                            ..MeshConfig::default()
                        },
                        true,
                    );
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    fake.script.reply_with([Value::Array(vec![])]);

                    run(&mut ctx, ".mesh sync").await.unwrap();

                    let lines = stdout_lines();
                    index_of(
                        &lines,
                        &format!("Nothing held for this node at {}.", short(&fake.hex())),
                    );
                    assert_eq!(fake.script.seen().len(), 1, "one fetch went to the node");

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                    fake.stop().await;
                });
            }

            /// Usage probe: a fetch lock held by another process (here: a
            /// second `flock` on the same lock file, which is what `FetchLock` refuses on)
            /// is the other contention variant: one line, `Ok`, nothing asked of the node,
            /// and the next `.mesh sync` after the lock is released runs normally.
            #[test]
            #[serial]
            fn usage_probe_sync_held_by_another_process_prints_one_line_and_succeeds() {
                use crate::mesh::mesh_cache_dir;

                let _guard = TestConfigDirGuard::new("repl-mesh-fetch-held");
                let _capture = capture::install();
                run_async(async {
                    let (fake, started) = runtime_with_fake_node("repl-mesh-fetch-held").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let lock_dir = mesh_cache_dir(started.runtime.cache_dir());
                    fs::create_dir_all(&lock_dir).unwrap();
                    let lock_path = lock_dir.join("propagation.json.lock");
                    let holder = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&lock_path)
                        .unwrap();
                    holder.try_lock().unwrap();

                    run(&mut ctx, ".mesh sync").await.unwrap();

                    let lines = stdout_lines();
                    let held = index_of(&lines, "another Coyote process");
                    assert!(
                        lines[held].contains(&lock_path.display().to_string()),
                        "{lines:?}"
                    );
                    assert!(
                        fake.script.seen().is_empty(),
                        "nothing was asked of the node"
                    );
                    assert!(
                        !lines.iter().any(|line| line.contains("Nothing held")),
                        "{lines:?}"
                    );

                    drop(holder);
                    fake.script.reply_with([Value::Array(vec![])]);
                    run(&mut ctx, ".mesh sync").await.unwrap();
                    let lines = stdout_lines();
                    index_of(
                        &lines,
                        &format!("Nothing held for this node at {}.", short(&fake.hex())),
                    );
                    assert_eq!(fake.script.seen().len(), 1);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                    fake.stop().await;
                });
            }

            /// Usage probe (TASK-100 (d)): like every consenting verb, a knock without a
            /// terminal and without `--yes` is refused naming the flag, after the notice
            /// but before anything is sent; the intro bound is enforced BEFORE consent is
            /// even considered (an over-long intro is refused with no notice and no prompt).
            #[test]
            #[serial]
            fn knock_without_a_terminal_names_the_flag_and_sends_nothing() {
                use crate::mesh::knocks::KNOCK_INTRO_MAX_CHARS;

                let _guard = TestConfigDirGuard::new("repl-mesh-knock-non-tty");
                let _script = prompt_script::install_non_interactive();
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-non-tty").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let (heard, _) = heard_peer(&started.runtime, "Tia", SystemTime::now());

                    let err = refusal(&mut ctx, &format!(".mesh knock {heard}")).await;
                    assert!(err.contains("--yes"), "{err}");
                    let out = stdout_lines();
                    assert!(
                        out.iter()
                            .any(|line| line.starts_with("This knocks on Tia (")),
                        "the notice precedes the consent question: {out:?}"
                    );
                    assert!(
                        !out.iter().any(|line| line.starts_with("Knocking on")),
                        "{out:?}"
                    );

                    let before = stdout_lines().len();
                    let long = "y".repeat(KNOCK_INTRO_MAX_CHARS + 1);
                    let err =
                        refusal(&mut ctx, &format!(".mesh knock {heard} --intro \"{long}\"")).await;
                    assert!(
                        err.contains(&format!(
                            "above the {KNOCK_INTRO_MAX_CHARS}-character limit"
                        )),
                        "{err}"
                    );
                    assert!(
                        !err.contains("--yes"),
                        "the bound, not consent, is the refusal: {err}"
                    );
                    assert!(
                        stdout_lines()[before..].is_empty(),
                        "no notice for a refused intro: {:?}",
                        &stdout_lines()[before..]
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe (TASK-100 (d)): a knock is NOT gated on this node's trust of the
            /// peer. The same heard-but-untrusted destination that `.mesh reply` refuses
            /// with the trust tail reaches the knock's consent notice, and an intro of
            /// exactly the bound is accepted.
            #[test]
            #[serial]
            fn knock_reaches_consent_for_a_peer_this_node_does_not_trust() {
                use crate::mesh::knocks::KNOCK_INTRO_MAX_CHARS;

                let _guard = TestConfigDirGuard::new("repl-mesh-knock-untrusted");
                let _script = prompt_script::install(&[false, false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-untrusted").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let (heard, _) = heard_peer(&started.runtime, "Tia", SystemTime::now());

                    let err = refusal(&mut ctx, &format!(".mesh reply {heard} hi")).await;
                    assert!(
                        err.contains(REPLY_REFUSAL_TAIL),
                        "reply is trust-gated: {err}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    let at_bound = "z".repeat(KNOCK_INTRO_MAX_CHARS);
                    run(
                        &mut ctx,
                        &format!(".mesh knock {heard} --intro \"{at_bound}\""),
                    )
                    .await
                    .unwrap();
                    let out = stdout_lines();
                    let notice = index_of(&out, "This knocks on Tia (");
                    assert!(out[notice].contains(&at_bound), "{out:?}");
                    index_of(&out, "Nothing was sent.");
                    assert_eq!(
                        prompt_script::prompts_asked(),
                        1,
                        "knock asked, reply did not"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Untrusted is not denied: a knock is for a peer that has not trusted us, but
            /// this node's own deny and block lists still stand in its way, before any
            /// notice or question, whether or not the denied destination has been heard.
            #[test]
            #[serial]
            fn knock_to_a_denied_destination_or_blocked_identity_is_refused() {
                let _guard = TestConfigDirGuard::new("repl-mesh-knock-denied");
                let _script = prompt_script::install(&[]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-denied").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let (denied, _) = heard_peer(&started.runtime, "Dot", now);
                    let (blocked, blocked_identity) = heard_peer(&started.runtime, "Bex", now);
                    let unheard_denied = hex_lower(&[0x52; 16]);
                    let trust = started.runtime.trust();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    trust.deny_destination(slot, &denied, None, now).unwrap();
                    trust
                        .deny_destination(slot, &unheard_denied, None, now)
                        .unwrap();
                    trust
                        .block_identity(slot, &blocked_identity, None, now)
                        .unwrap();

                    for (destination, standing) in [
                        (&denied, "denied"),
                        (&unheard_denied, "denied"),
                        (&blocked, "blocked"),
                    ] {
                        let before = stdout_lines().len();
                        let err =
                            refusal(&mut ctx, &format!(".mesh knock {destination} --yes")).await;
                        assert!(
                            err.contains(&format!(" is {standing} in this node's trust list")),
                            "{standing}: {err}"
                        );
                        assert!(
                            !err.contains("has not been heard"),
                            "{standing}: the deny is named, not the missing peer row: {err}"
                        );
                        assert!(err.contains(KNOCK_REFUSAL_TAIL), "{standing}: {err}");
                        assert!(
                            stdout_lines()[before..].is_empty(),
                            "{standing}: refused before the notice: {:?}",
                            &stdout_lines()[before..]
                        );
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe (TASK-100 knock ruling): only denied/blocked stand in a knock's
            /// way. An instance heard under a NEW key (marked `key changed` in the peer
            /// table; `authorize` judges it default-closed, which refuses `.mesh reply`) is
            /// still knockable: the notice shows `trust: untrusted` and the question is
            /// asked; nothing is sent when it is declined.
            #[test]
            #[serial]
            fn usage_probe_knock_passes_the_gate_for_an_instance_whose_key_changed() {
                let _guard = TestConfigDirGuard::new("repl-mesh-knock-key-changed");
                let _script = prompt_script::install(&[false]);
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knock-key-changed").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let runtime = started.runtime.clone();
                    let slot: &dyn LiveMesh = ctx.app.mesh.as_ref();
                    let old_dest = heard_trusted_peer(&runtime, slot);
                    let (new_dest, new_identity) =
                        heard_peer(&runtime, "Tia again", SystemTime::now());
                    let name_hash = runtime.peers().get(&old_dest).unwrap().name_hash;
                    runtime
                        .trust()
                        .note_key_change(&new_identity, &name_hash, SystemTime::now());
                    let verdict = runtime.trust().authorize(&new_identity, &new_dest);
                    assert_eq!(
                        verdict.decision,
                        Decision::Refuse,
                        "fixture: the new key is not trusted"
                    );
                    let peers = out_of(&mut ctx, ".mesh peers").await.unwrap();
                    assert!(
                        peers.contains("key changed:") && peers.contains(short(&new_identity)),
                        "fixture: the peer table marks the key change: {peers}"
                    );

                    let err = refusal(&mut ctx, &format!(".mesh reply {new_dest} hi")).await;
                    assert!(err.contains(REPLY_REFUSAL_TAIL), "reply stays gated: {err}");
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    run(&mut ctx, &format!(".mesh knock {new_dest}"))
                        .await
                        .unwrap();

                    let out = stdout_lines();
                    let notice = index_of(&out, "This knocks on Tia again (");
                    assert!(out[notice].contains("trust: untrusted"), "{out:?}");
                    assert!(
                        !out.iter().any(|line| line.contains(KNOCK_REFUSAL_TAIL)),
                        "{out:?}"
                    );
                    index_of(&out, "Nothing was sent.");
                    assert_eq!(prompt_script::prompts_asked(), 1, "the knock asked");
                    assert!(
                        !out.iter().any(|line| line.starts_with("Knocking on")),
                        "{out:?}"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// The peer is a stub behind the real dispatcher that knows this node's identity
            /// but trusts none of its instances, so the knock over the link is admitted and
            /// refused `NoAccess`: a knock that landed, reported as direct delivery.
            #[test]
            #[serial]
            fn knock_over_a_link_reports_direct_delivery() {
                let _guard = TestConfigDirGuard::new("repl-mesh-knock-direct");
                let _script = prompt_script::install(&[]);
                let _capture = capture::install();
                run_async(async {
                    let stub = PeerStub::listen(
                        "repl-mesh-knock-direct-stub",
                        TcpServer::DEFAULT_CLIENT_MTU,
                    )
                    .await;
                    let started = started_runtime_on("repl-mesh-knock-direct", stub.port()).await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    stub.know_identity(started.runtime.fingerprint());
                    stub.announce(Some("Tia")).await;
                    let peer = stub.destination_hex();
                    let peers = started.runtime.peers();
                    wait_until("the node to file the stub", || peers.get(&peer).is_some()).await;

                    run(
                        &mut ctx,
                        &format!(".mesh knock {peer} --yes --intro \"hi\""),
                    )
                    .await
                    .unwrap();

                    let out = stdout_lines();
                    let notice = index_of(&out, "This knocks on Tia (");
                    assert!(out[notice].contains("trust: untrusted"), "{out:?}");
                    let knocking = index_of(&out, &format!("Knocking on {}", short(&peer)));
                    let landed = index_of(
                        &out,
                        &format!(
                            "Knocked on {} directly; the peer decides whether to trust this instance.",
                            short(&peer)
                        ),
                    );
                    assert!(notice < knocking && knocking < landed, "{out:?}");
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                    stub.stop().await;
                });
            }

            /// A well-formed trust file of the layout this program wrote before the SCOPE
            /// rename (`version: 1`; the trust file is at `2` now), as an operator upgrading
            /// in place would have on disk. The literal `1` is deliberate: a fixture built
            /// from the constant would follow a wrongful bump and could never go red.
            fn pre_scope_trust_file() -> String {
                let (identity, destination) = ("1a".repeat(16), "2b".repeat(16));
                let ts = "2026-01-01T00:00:00Z";
                format!(
                    "version: 1\n\
                     identities:\n  {identity}:\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n    all_destinations: false\n\
                     destinations:\n  {destination}:\n    identity: {identity}\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: Bob\n    note: null\n"
                )
            }

            /// A session carrying an instance id, the shape `.mesh on` needs to start.
            fn session_with_id() -> Session {
                let id = "0123456789abcdef".repeat(2);
                serde_yaml::from_str(&format!(
                    "model: provider:test\nmessages: []\nmesh_instance_id: {id}"
                ))
                .unwrap()
            }

            /// Usage probe (T33 (e), at the consumer surface): a trust file written before
            /// the rename is not "silently empty" — `.mesh on` itself refuses, the message
            /// names the file, both versions and the remedy, the mesh stays off with no
            /// tools exposed, and the file is left exactly as it was. The relay port is
            /// closed so a wrongly-accepted file cannot start a node: the refusal must be
            /// the trust file's, not the join's.
            #[test]
            #[serial]
            fn usage_probe_mesh_on_refuses_a_pre_scope_trust_file_and_stays_off() {
                let guard = TestConfigDirGuard::new("repl-mesh-on-pre-scope-trust");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), guard.path.join("cache"));
                let _capture = capture::install();
                let trust_path = crate::mesh::mesh_config_dir(&guard.path).join("trust.yaml");
                fs::create_dir_all(trust_path.parent().unwrap()).unwrap();
                let old = pre_scope_trust_file();
                fs::write(&trust_path, &old).unwrap();
                run_async(async {
                    let closed_port = {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        listener.local_addr().unwrap().port()
                    };
                    let mut ctx = ctx_with(private_config(closed_port), true);
                    ctx.session = Some(session_with_id());
                    ctx.refresh_tool_scope(create_abort_signal()).await.unwrap();

                    let err = format!("{:#}", run(&mut ctx, ".mesh on --yes").await.unwrap_err());

                    assert!(err.contains(&trust_path.display().to_string()), "{err}");
                    assert!(err.contains("version 1"), "{err}");
                    assert!(err.contains("version 2"), "{err}");
                    assert!(err.contains("move the file aside"), "{err}");
                    assert!(
                        !err.contains("connect") && !err.contains("relay"),
                        "the refusal must be the trust file's, not the closed relay's: {err}"
                    );
                    assert!(ctx.app.mesh.get().is_none(), "no node may be installed");
                    assert!(!ctx.app.config.mesh.enabled);
                    assert!(mesh_tool_names(&ctx).is_empty());
                    assert_eq!(
                        fs::read_to_string(&trust_path).unwrap(),
                        old,
                        "a refused trust file is left as it was"
                    );
                    let out = stdout_lines();
                    assert!(
                        !out.iter()
                            .any(|line| line.contains("Mesh is on for this session")),
                        "{out:?}"
                    );
                });
            }

            /// Usage probe (T33 (e), at the consumer surface): the peer table lives in the
            /// cache and is loaded by the start too; one written before the rename
            /// (`"version":1`, the table is at `2` now) refuses `.mesh on` by name, with the
            /// mesh staying off and the table untouched.
            #[test]
            #[serial]
            fn usage_probe_mesh_on_refuses_a_pre_scope_peer_table_and_stays_off() {
                let guard = TestConfigDirGuard::new("repl-mesh-on-pre-scope-peers");
                let cache_dir = guard.path.join("cache");
                let _cache = EnvVarGuard::set(get_env_name("cache_dir"), &cache_dir);
                let _capture = capture::install();
                let peers_path = crate::mesh::mesh_cache_dir(&cache_dir).join("peers.json");
                fs::create_dir_all(peers_path.parent().unwrap()).unwrap();
                let old = br#"{"version":1,"peers":[]}"#.to_vec();
                fs::write(&peers_path, &old).unwrap();
                run_async(async {
                    let closed_port = {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        listener.local_addr().unwrap().port()
                    };
                    let mut ctx = ctx_with(private_config(closed_port), true);
                    ctx.session = Some(session_with_id());
                    ctx.refresh_tool_scope(create_abort_signal()).await.unwrap();

                    let err = format!("{:#}", run(&mut ctx, ".mesh on --yes").await.unwrap_err());

                    assert!(err.contains(&peers_path.display().to_string()), "{err}");
                    assert!(err.contains("version 1"), "{err}");
                    assert!(err.contains("version 2"), "{err}");
                    assert!(err.contains("move the file aside"), "{err}");
                    assert!(ctx.app.mesh.get().is_none(), "no node may be installed");
                    assert!(!ctx.app.config.mesh.enabled);
                    assert!(mesh_tool_names(&ctx).is_empty());
                    assert_eq!(
                        fs::read(&peers_path).unwrap(),
                        old,
                        "a refused peer table is left as it was"
                    );
                    assert!(!peers_path.with_extension("json.corrupt").exists());
                });
            }

            /// Usage probe (T33 (e), at the consumer surface): the knock cache is read
            /// lazily, so a pre-rename line (`"version":1`; records are at `2` now) surfaces
            /// at `.mesh knocks` — by file and line, naming both versions and the remedy —
            /// instead of listing nothing. The node itself keeps running (the cache is not
            /// a start-time store), and the file is left as it was.
            #[test]
            #[serial]
            fn usage_probe_mesh_knocks_surfaces_a_pre_scope_knock_line_by_file_and_line() {
                use crate::mesh::rfc3339_utc;

                let _guard = TestConfigDirGuard::new("repl-mesh-knocks-pre-scope");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-knocks-pre-scope").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let path = started.runtime.knock_gate().cache().path().to_path_buf();
                    let (dest, id, name_hash) = announced_peer();
                    let mut old = serde_json::to_value(KnockRecord {
                        version: 1,
                        received_at: rfc3339_utc(SystemTime::now()),
                        identity_hash: id,
                        destination_hash: dest,
                        name_hash,
                        display_name: Some("Old".to_string()),
                        intro: None,
                        hops: 1,
                    })
                    .unwrap();
                    // Whatever `KnockRecord` serialises today, the line on disk says 1.
                    old["version"] = serde_json::json!(1);
                    let old = format!("{old}\n");
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, &old).unwrap();

                    let err = format!(
                        "{:#}",
                        run(&mut ctx, ".mesh knocks")
                            .await
                            .expect_err(".mesh knocks")
                    );

                    assert!(err.contains(&path.display().to_string()), "{err}");
                    assert!(err.contains("line 1"), "{err}");
                    assert!(err.contains("version 1"), "{err}");
                    assert!(err.contains("version 2"), "{err}");
                    assert!(err.contains("move the file aside"), "{err}");
                    assert_eq!(
                        fs::read_to_string(&path).unwrap(),
                        old,
                        "a refused knock cache is left as it was"
                    );
                    assert!(
                        ctx.app.mesh.get().is_some(),
                        "a refused knock cache does not take the node down"
                    );

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Restores the mode of a directory the test made read-only, panic or not, so
            /// the config-dir guard can still remove it.
            struct ModeRestore {
                path: std::path::PathBuf,
                mode: u32,
            }

            impl ModeRestore {
                fn read_only(path: &Path) -> Self {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = fs::metadata(path).unwrap().permissions().mode();
                    fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
                    Self {
                        path: path.to_path_buf(),
                        mode,
                    }
                }

                fn restore(&self) {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
                }
            }

            impl Drop for ModeRestore {
                fn drop(&mut self) {
                    self.restore();
                }
            }

            /// Usage probe: the identity-stays-trusted sentence is printed BEFORE the write,
            /// so when the write then fails the human has read the sentence and gets the
            /// write error after it — and nothing is half-done:
            /// memory and disk still agree on "trusted", no deny exists, no hook fired, no
            /// temp file is left behind. The same holds for the forget branch ("This forgets
            /// trusted instance …" then the error, the record intact) and for
            /// `--identity --confirm`. Once the directory is writable again the very same
            /// command succeeds with the same first line.
            #[test]
            #[serial]
            fn usage_probe_untrust_sentence_precedes_a_failed_write_and_memory_stays_with_disk() {
                use crate::mesh::events::{MeshHooks, RecordingHookSink, TrustHookObserver};

                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-failed-write");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-failed-write").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    let (plain, plain_id) = heard_peer(&started.runtime, "Pim", now);
                    out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                        .await
                        .unwrap();
                    out_of(&mut ctx, &format!(".mesh trust {plain} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust).expect("both records are on disk");
                    let hooks = MeshHooks::default();
                    let sink = RecordingHookSink::attach(&hooks);
                    trust.set_observer(Arc::new(TrustHookObserver(hooks)));
                    let mesh_dir = trust.path().parent().unwrap().to_path_buf();
                    let tmp = trust.path().with_added_extension("tmp");
                    let sentence = format!(
                        "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`."
                    );
                    let forgets = format!(
                        "This forgets trusted instance {}; the record for its identity stays.",
                        short(&plain)
                    );

                    let locked = ModeRestore::read_only(&mesh_dir);
                    if fs::File::create(mesh_dir.join("probe-write-check")).is_ok() {
                        // A user that ignores directory modes (root) cannot stage the failure.
                        let _ = fs::remove_file(mesh_dir.join("probe-write-check"));
                        assert!(ctx.app.mesh.stop().await.unwrap());
                        started.relay_handle.abort();
                        return;
                    }

                    // Refuse-in-place branch, both spellings: sentence, then the error.
                    for verb in ["untrust", "forget"] {
                        let printed = stdout_lines().len();
                        let err = refusal(&mut ctx, &format!(".mesh {verb} {dest} --yes")).await;
                        let after: Vec<String> = stdout_lines()[printed..].to_vec();
                        assert_eq!(after, vec![sentence.clone()], "{verb}: {after:?}");
                        assert!(err.contains("Failed to write"), "{verb}: {err}");
                        assert!(
                            err.contains(&trust.path().display().to_string()),
                            "{verb}: the error names the file: {err}"
                        );
                        assert!(!err.contains("Refused"), "{verb}: no success line: {err}");
                        assert_eq!(
                            trust_file(&trust).as_deref(),
                            Some(before.as_slice()),
                            "{verb}: disk unchanged"
                        );
                        assert_eq!(
                            trust.authorize(&id, &dest).rule,
                            Rule::IdentityTrusted,
                            "{verb}: memory agrees with disk"
                        );
                        assert!(trust.denied().is_empty(), "{verb}: {:?}", trust.denied());
                        assert!(!tmp.exists(), "{verb}: no temp file left behind");
                    }

                    // Forget branch: the forgets line, then the error, record intact.
                    let printed = stdout_lines().len();
                    let err = refusal(&mut ctx, &format!(".mesh forget {plain} --yes")).await;
                    let after: Vec<String> = stdout_lines()[printed..].to_vec();
                    assert_eq!(after, vec![forgets.clone()], "{after:?}");
                    assert!(err.contains("Failed to write"), "{err}");
                    assert!(!err.contains("Untrusted"), "{err}");
                    assert_eq!(
                        trust.authorize(&plain_id, &plain).rule,
                        Rule::DestinationTrusted
                    );
                    assert!(trusted_hashes(&trust).contains(&plain));

                    // Identity branch with its token: the error, identity still trusted.
                    let token = format!("untrust-{}", short(&id));
                    let err = refusal(
                        &mut ctx,
                        &format!(".mesh untrust --identity {id} --confirm {token}"),
                    )
                    .await;
                    assert!(err.contains("Failed to write"), "{err}");
                    assert!(
                        trusted_hashes(&trust).contains(&id),
                        "{:?}",
                        trusted_hashes(&trust)
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);
                    assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                    assert!(
                        sink.drain().is_empty(),
                        "a write that never reached disk fires no trust hook"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    // Writable again: the same command, same first line, now completes.
                    locked.restore();
                    drop(locked);
                    let out = out_of(&mut ctx, &format!(".mesh untrust {dest} --yes"))
                        .await
                        .unwrap();
                    assert_eq!(
                        out,
                        format!(
                            "{sentence}\nRefused {}; `.mesh trust {dest}` lifts that.",
                            short(&dest)
                        )
                    );
                    assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationDenied);
                    assert_ne!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                    assert!(!tmp.exists());

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: `forget` asks under its own name ("Forget <short>?") where
            /// `untrust` asks "Untrust <short>?", and under both the sentence has already
            /// been printed when the question comes — a declined
            /// prompt or a missing terminal writes nothing and the verdict stays trusted.
            /// The confirm token is `untrust-<short>` under the alias too: a `forget-<short>`
            /// token is refused with a hint spelling `.mesh forget --identity … --confirm
            /// untrust-<short>`, and an unknown flag is refused identically under both names.
            #[test]
            #[serial]
            fn usage_probe_forget_asks_under_its_own_name_after_the_sentence_and_keeps_the_untrust_token()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-forget-question");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-forget-question").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", now);
                    {
                        let _script = prompt_script::install(&[]);
                        out_of(&mut ctx, &format!(".mesh trust --identity {id} --yes"))
                            .await
                            .unwrap();
                    }
                    let before = trust_file(&trust).expect("the identity record is on disk");
                    let sentence = format!(
                        "Tia's identity stays trusted; this instance is refused until `.mesh trust {dest}`."
                    );

                    // No terminal: the question is the error, after the sentence.
                    for (verb, title) in [("untrust", "Untrust"), ("forget", "Forget")] {
                        let _script = prompt_script::install_non_interactive();
                        let printed = stdout_lines().len();
                        let err = refusal(&mut ctx, &format!(".mesh {verb} {dest}")).await;
                        assert_eq!(
                            stdout_lines()[printed..].to_vec(),
                            vec![sentence.clone()],
                            "{verb}"
                        );
                        assert_eq!(
                            err,
                            format!(
                                "{title} {}? Standard input is not a terminal, so there is no prompt to answer; pass --yes to confirm.",
                                short(&dest)
                            ),
                            "{verb}"
                        );
                        assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                        assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);
                        assert_eq!(prompt_script::prompts_asked(), 0, "{verb}");
                    }

                    // A terminal that answers no: sentence, then "Nothing was changed."
                    for verb in ["untrust", "forget"] {
                        let _script = prompt_script::install(&[false]);
                        let out = out_of(&mut ctx, &format!(".mesh {verb} {dest}"))
                            .await
                            .unwrap();
                        assert_eq!(out, format!("{sentence}\n{NOTHING_CHANGED}"), "{verb}");
                        assert_eq!(prompt_script::prompts_asked(), 1, "{verb}");
                        assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                        assert_eq!(trust.authorize(&id, &dest).rule, Rule::IdentityTrusted);
                        assert!(trust.denied().is_empty(), "{verb}");
                    }

                    // The alias takes the `untrust-` token, and says so under its own name.
                    let _script = prompt_script::install(&[]);
                    let short_id = short(&id);
                    let err = refusal(
                        &mut ctx,
                        &format!(".mesh forget --identity {id} --confirm forget-{short_id}"),
                    )
                    .await;
                    assert!(
                        err.contains(&format!("--confirm untrust-{short_id}")),
                        "{err}"
                    );
                    assert!(err.contains(".mesh forget --identity"), "{err}");
                    assert!(!err.contains(".mesh untrust --identity"), "{err}");
                    assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                    assert!(trusted_hashes(&trust).contains(&id));

                    // An unknown flag is refused the same way under both names.
                    let untrust_err =
                        refusal(&mut ctx, &format!(".mesh untrust {dest} --label x")).await;
                    let forget_err =
                        refusal(&mut ctx, &format!(".mesh forget {dest} --label x")).await;
                    assert!(untrust_err.contains("--label"), "{untrust_err}");
                    // Same first line (the refusal + usage) up to the verb's own name; the
                    // confirm token in the usage stays `untrust-` under the alias.
                    assert_eq!(
                        forget_err.lines().next().unwrap(),
                        untrust_err
                            .lines()
                            .next()
                            .unwrap()
                            .replace(".mesh untrust", ".mesh forget"),
                        "the alias differs from `untrust` only in its name:\n{forget_err}"
                    );
                    assert!(
                        forget_err.contains("--confirm untrust-<identity-short>"),
                        "{forget_err}"
                    );
                    assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: bare `.mesh` lists exactly the 24 verbs, one row each in table
            /// order, `sync` and `forget` among them and none of the withdrawn spellings;
            /// the old `.mesh fetch` and `.mesh help` are unknown verbs
            /// whose single-sentence error points at `.mesh` for the list (it does not
            /// name `sync`), and under the node they touch nothing.
            #[test]
            #[serial]
            fn usage_probe_bare_mesh_lists_the_twenty_four_verbs_and_the_old_fetch_spelling_points_at_it()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-verb-list");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-verb-list").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let (dest, id) = heard_peer(&started.runtime, "Tia", SystemTime::now());
                    out_of(&mut ctx, &format!(".mesh trust {dest} --yes"))
                        .await
                        .unwrap();
                    let before = trust_file(&trust).expect("the record is on disk");

                    let out = out_of(&mut ctx, ".mesh").await.unwrap();
                    let listed: Vec<&str> = out
                        .lines()
                        .filter_map(|line| line.strip_prefix("  .mesh "))
                        .map(|rest| rest.split_whitespace().next().unwrap())
                        .collect();
                    let expected: Vec<&str> = VERBS.iter().map(|(name, _, _)| *name).collect();
                    assert_eq!(listed, expected, "{out}");
                    assert_eq!(listed.len(), 24, "{out}");
                    for present in [
                        "sync", "forget", "untrust", "trust", "block", "unblock", "allow", "deny",
                        "unshare", "shares",
                    ] {
                        assert!(listed.contains(&present), "{present}: {out}");
                    }
                    for absent in ["fetch", "undeny", "help"] {
                        assert!(!listed.contains(&absent), "{absent}: {out}");
                    }
                    assert!(out.contains("  .mesh sync       Sync the messages a propagation node holds for this node now"), "{out}");
                    assert!(
                        out.contains("  .mesh forget     alias of `untrust`: forget this peer"),
                        "{out}"
                    );

                    for old in [".mesh fetch", ".mesh help", ".mesh fetch --yes"] {
                        let printed = stdout_lines().len();
                        let err = refusal(&mut ctx, old).await;
                        let word = old.split_whitespace().nth(1).unwrap();
                        assert_eq!(
                            err,
                            format!("Unknown .mesh command '{word}'. Type `.mesh` for the list."),
                            "{old}"
                        );
                        assert_eq!(stdout_lines().len(), printed, "{old} prints nothing else");
                        assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                        assert_eq!(trust.authorize(&id, &dest).rule, Rule::DestinationTrusted);
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: completion never offers what the verb refuses. With a refused
            /// instance, a blocked identity that had a trusted instance, and a plain trusted
            /// instance side by side, `untrust`/`forget <TAB>` offer only the plain
            /// one (not the refused, not the blocked identity's — which `block` already
            /// swept), `trust <TAB>` never offers the blocked identity's destination,
            /// `trust --identity <TAB>` never the blocked identity, `block <TAB>` never an
            /// already-blocked one and `unblock <TAB>` only that one; every candidate offered
            /// to `untrust`/`forget` is one the verb then accepts.
            #[test]
            #[serial]
            fn usage_probe_completion_after_untrust_forget_and_trust_offers_only_what_the_verb_takes()
             {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-completion-gate");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started = started_runtime("repl-mesh-usage-probe-completion-gate").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let (refused, refused_id) = heard_peer(&started.runtime, "Tia", now);
                    let (blocked_dest, blocked_id) = heard_peer(&started.runtime, "Bob", now);
                    let (plain, plain_id) = heard_peer(&started.runtime, "Pim", now);
                    for line in [
                        format!(".mesh trust --identity {refused_id} --yes"),
                        format!(".mesh untrust {refused} --yes"),
                        format!(".mesh trust {blocked_dest} --yes"),
                        format!(".mesh block {blocked_id} --yes"),
                        format!(".mesh trust {plain} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    assert_eq!(
                        trust.authorize(&refused_id, &refused).rule,
                        Rule::DestinationDenied
                    );
                    assert_eq!(
                        trust.authorize(&blocked_id, &blocked_dest).rule,
                        Rule::IdentityBlocked
                    );
                    assert_eq!(
                        trust.authorize(&plain_id, &plain).rule,
                        Rule::DestinationTrusted
                    );

                    let offers = |ctx: &RequestContext, words: &[&str]| -> Vec<String> {
                        ctx.repl_complete(".mesh", words, "")
                            .into_iter()
                            .map(|(candidate, _)| candidate)
                            .collect()
                    };
                    for verb in ["untrust", "forget"] {
                        let offered = offers(&ctx, &[verb, ""]);
                        assert!(offered.contains(&plain), "{verb}: {offered:?}");
                        assert!(!offered.contains(&refused), "{verb}: refused: {offered:?}");
                        assert!(
                            !offered.contains(&blocked_dest),
                            "{verb}: blocked: {offered:?}"
                        );
                        let identities = offers(&ctx, &[verb, "--identity", ""]);
                        assert!(identities.contains(&refused_id), "{verb}: {identities:?}");
                        assert!(!identities.contains(&blocked_id), "{verb}: {identities:?}");
                        // (An identity trusted at the destination tier only IS offered — the
                        // verb accepts it and removes that record; pinned below by use.)
                        for identity in &identities {
                            let out = out_of(
                                &mut ctx,
                                &format!(".mesh {verb} --identity {identity} --dry-run"),
                            )
                            .await
                            .unwrap_or_else(|err| panic!("{verb} --identity {identity}: {err}"));
                            assert!(out.contains(DRY_RUN_NOTHING_CHANGED), "{out}");
                        }
                    }
                    let trustable = offers(&ctx, &["trust", ""]);
                    assert!(trustable.contains(&refused), "{trustable:?}");
                    assert!(!trustable.contains(&blocked_dest), "{trustable:?}");
                    let trust_identities = offers(&ctx, &["trust", "--identity", ""]);
                    assert!(
                        !trust_identities.contains(&blocked_id),
                        "{trust_identities:?}"
                    );
                    assert!(trust_identities.contains(&plain_id), "{trust_identities:?}");
                    let blockable = offers(&ctx, &["block", ""]);
                    assert!(!blockable.contains(&blocked_id), "{blockable:?}");
                    assert!(blockable.contains(&plain_id), "{blockable:?}");
                    assert_eq!(offers(&ctx, &["unblock", ""]), vec![blocked_id.clone()]);

                    // What `untrust <TAB>` offered, `untrust` takes without a refusal.
                    let before = trust_file(&trust).unwrap();
                    let hashes: Vec<String> = offers(&ctx, &["forget", ""])
                        .into_iter()
                        .filter(|candidate| !candidate.starts_with("--"))
                        .collect();
                    assert_eq!(hashes, vec![plain.clone()], "{hashes:?}");
                    for candidate in hashes {
                        let out = out_of(&mut ctx, &format!(".mesh forget {candidate} --dry-run"))
                            .await
                            .unwrap_or_else(|err| panic!("{candidate}: {err}"));
                        assert!(out.ends_with(DRY_RUN_NOTHING_CHANGED), "{out}");
                        assert!(
                            !out.contains("stays trusted"),
                            "offered as plainly trusted: {out}"
                        );
                    }
                    assert_eq!(trust_file(&trust).as_deref(), Some(before.as_slice()));
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            /// Usage probe: the round trip under the refused shape. With an instance refused
            /// in place, `trust --prune --confirm` for an ancient horizon sweeps a stale
            /// sibling but leaves the refused record where it is (still denied, still
            /// bound), `.mesh peers` shows it as denied, `block` then `unblock` of its identity
            /// leaves the deny standing, and the verb error after that spells the FULL
            /// destination twice.
            #[test]
            #[serial]
            fn usage_probe_prune_then_block_unblock_leave_a_refused_record_and_its_deny_standing() {
                let _guard = TestConfigDirGuard::new("repl-mesh-usage-probe-prune-block-unblock");
                let _capture = capture::install();
                let _script = prompt_script::install(&[]);
                run_async(async {
                    let started =
                        started_runtime("repl-mesh-usage-probe-prune-block-unblock").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let trust = started.runtime.trust();
                    let now = SystemTime::now();
                    let long_ago = now - Duration::from_secs(400 * 24 * 60 * 60);
                    let (refused, id) = heard_peer(&started.runtime, "Tia", now);
                    let (stale, _stale_id) = heard_peer(&started.runtime, "Old", long_ago);
                    trust
                        .trust_destination(
                            ctx.app.mesh.as_ref(),
                            &stale,
                            TrustOptions::default(),
                            long_ago,
                        )
                        .unwrap();
                    for line in [
                        format!(".mesh trust --identity {id} --yes"),
                        format!(".mesh untrust {refused} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    assert_eq!(trust.authorize(&id, &refused).rule, Rule::DestinationDenied);
                    let refused_row = |trust: &TrustStore| {
                        trust
                            .records()
                            .into_iter()
                            .find(|record| record.hash == refused)
                    };
                    assert!(refused_row(&trust).unwrap().denied);

                    let preview = out_of(&mut ctx, ".mesh trust --prune --older-than 1d --dry-run")
                        .await
                        .unwrap();
                    assert!(!preview.contains(short(&refused)), "{preview}");
                    assert!(!preview.contains(&refused), "{preview}");
                    assert!(preview.contains(&stale), "{preview}");
                    assert!(
                        preview.contains("--older-than 1d --confirm prune-1"),
                        "{preview}"
                    );
                    let out = out_of(
                        &mut ctx,
                        ".mesh trust --prune --older-than 1d --confirm prune-1",
                    )
                    .await
                    .unwrap();
                    assert!(out.starts_with("Removed 1 trusted instance(s):"), "{out}");
                    let row = refused_row(&trust).expect("prune left the refused record");
                    assert!(row.denied);
                    assert_eq!(row.identity.as_deref(), Some(id.as_str()));
                    assert_eq!(trust.authorize(&id, &refused).rule, Rule::DestinationDenied);
                    assert!(!trusted_hashes(&trust).contains(&stale));
                    assert!(trusted_hashes(&trust).contains(&id));

                    let peers = out_of(&mut ctx, ".mesh peers").await.unwrap();
                    let tia = peers
                        .lines()
                        .find(|line| line.starts_with("Tia"))
                        .unwrap_or_else(|| panic!("no Tia row in {peers}"));
                    assert!(tia.contains("denied"), "{tia}");

                    for line in [
                        format!(".mesh block {id} --yes"),
                        format!(".mesh unblock {id} --yes"),
                    ] {
                        out_of(&mut ctx, &line).await.unwrap();
                    }
                    assert_eq!(trust.authorize(&id, &refused).rule, Rule::DestinationDenied);
                    assert_eq!(trust.denied().len(), 1);
                    let teaching = format!(
                        "Destination {refused} is refused and its identity is not trusted; `.mesh trust {refused}` lifts that once the peer is heard."
                    );
                    for verb in ["untrust", "forget"] {
                        assert_eq!(
                            refusal(&mut ctx, &format!(".mesh {verb} {refused} --yes")).await,
                            teaching,
                            "{verb}"
                        );
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);

                    assert!(ctx.app.mesh.stop().await.unwrap());
                    started.relay_handle.abort();
                });
            }

            mod share_verbs {
                use super::*;
                use crate::mesh::shares::{RawEntry, ShareLocations};
                use crate::mesh::test_support::{TempDir, snapshot_fixture};

                /// A node whose published snapshot names `root`, a temp workspace seeded
                /// with `files`, as the share root; `locations` are the two share files
                /// as the node resolves them.
                struct ShareFixture {
                    started: StartedRuntime,
                    ctx: RequestContext,
                    root: TempDir,
                    locations: ShareLocations,
                }

                impl ShareFixture {
                    async fn new(tag: &str, files: &[&str]) -> Self {
                        let started = started_runtime(tag).await;
                        let ctx = ctx_with(MeshConfig::default(), true);
                        ctx.app.mesh.install(started.runtime.clone()).unwrap();
                        let root = TempDir::new(&format!("{tag}-root"));
                        seed_files(&root.path, files);
                        publish_root(&ctx, &root.path);
                        let (_, locations) = share_locations(&ctx).unwrap();
                        Self {
                            started,
                            ctx,
                            root,
                            locations,
                        }
                    }

                    fn global(&self) -> String {
                        self.locations.global.display().to_string()
                    }

                    fn workspace(&self) -> String {
                        self.locations.workspace.display().to_string()
                    }

                    fn entries(&self) -> Vec<RawEntry> {
                        ShareSet::load_quietly(self.locations.clone()).0.entries()
                    }

                    fn write_global(&self, yaml: &str) {
                        write_share_file(&self.locations.global, yaml);
                    }

                    fn write_workspace(&self, yaml: &str) {
                        write_share_file(&self.locations.workspace, yaml);
                    }

                    async fn stop(self) {
                        assert!(self.ctx.app.mesh.stop().await.unwrap());
                        self.started.relay_handle.abort();
                    }
                }

                fn seed_files(root: &Path, files: &[&str]) {
                    for relative in files {
                        let path = root.join(relative);
                        fs::create_dir_all(path.parent().unwrap()).unwrap();
                        fs::write(&path, relative).unwrap();
                    }
                }

                fn publish_root(ctx: &RequestContext, root: &Path) {
                    let mut snapshot = snapshot_fixture();
                    snapshot.cwd = root.to_path_buf();
                    ctx.app.mesh.publish(snapshot);
                }

                fn write_share_file(path: &Path, yaml: &str) {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, yaml).unwrap();
                }

                fn allow_entry(layer: Layer, pattern: &str, peer: Option<&str>) -> RawEntry {
                    RawEntry {
                        layer,
                        kind: RawKind::Allow {
                            pattern: pattern.to_string(),
                            peer: peer.map(str::to_string),
                        },
                    }
                }

                fn deny_entry(layer: Layer, pattern: &str) -> RawEntry {
                    RawEntry {
                        layer,
                        kind: RawKind::Deny {
                            pattern: pattern.to_string(),
                        },
                    }
                }

                fn override_entry(layer: Layer, path: &str) -> RawEntry {
                    RawEntry {
                        layer,
                        kind: RawKind::Override {
                            path: path.to_string(),
                        },
                    }
                }

                #[test]
                #[serial]
                fn allow_writes_the_global_file_when_no_workspace_file_exists_and_says_so() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-global-default");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-global-default",
                            &["docs/a.md", "docs/b.md"],
                        )
                        .await;
                        assert!(!fx.locations.workspace.exists());

                        let out = out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();

                        let lines: Vec<&str> = out.lines().collect();
                        assert_eq!(
                            lines,
                            [
                                format!(
                                    "`docs/**` matches 2 file(s) under {}.",
                                    fx.root.path.display()
                                ),
                                format!(
                                    "Will write to {}: allow `docs/**` for every trusted peer.",
                                    fx.global()
                                ),
                                format!(
                                    "Allowed `docs/**` for every trusted peer; written to {}.",
                                    fx.global()
                                ),
                            ],
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        assert!(fx.locations.global.exists());
                        assert!(!fx.locations.workspace.exists());
                        assert_eq!(fx.entries(), [allow_entry(Layer::Global, "docs/**", None)]);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_workspace_creates_the_workspace_file_and_later_bare_allows_land_there() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-workspace-created");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-workspace-created",
                            &["docs/a.md", "notes/n.md"],
                        )
                        .await;

                        let out = out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();
                        assert!(
                            out.contains(&format!(
                                "Will write to {} (created): allow `docs/**` for every trusted peer.",
                                fx.workspace()
                            )),
                            "{out}"
                        );
                        assert!(
                            out.contains(&format!(
                                "Allowed `docs/**` for every trusted peer; written to {} (created).",
                                fx.workspace()
                            )),
                            "{out}"
                        );
                        assert!(fx.locations.workspace.exists());
                        assert!(!fx.locations.global.exists());

                        let out = out_of(&mut fx.ctx, ".mesh allow notes/**").await.unwrap();
                        assert!(
                            out.contains(&format!(
                                "Allowed `notes/**` for every trusted peer; written to {}.",
                                fx.workspace()
                            )),
                            "{out}"
                        );
                        assert!(!out.contains("(created)"), "{out}");
                        assert!(!fx.locations.global.exists());
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Workspace, "docs/**", None),
                                allow_entry(Layer::Workspace, "notes/**", None),
                            ]
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_global_forces_the_global_file_while_a_workspace_file_exists() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-global-flag");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-global-flag",
                            &["docs/a.md", "notes/n.md"],
                        )
                        .await;
                        out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh allow notes/** --global")
                            .await
                            .unwrap();

                        assert!(
                            out.contains(&format!(
                                "Allowed `notes/**` for every trusted peer; written to {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Global, "notes/**", None),
                                allow_entry(Layer::Workspace, "docs/**", None),
                            ]
                        );
                        fx.stop().await;
                    });
                }

                #[test]
                fn breadth_words_say_more_than_the_limit_only_when_the_cap_was_reached() {
                    assert_eq!(
                        breadth_words(&MatchCount {
                            files: BROAD_MATCH_LIMIT + 1,
                            capped: true,
                            truncated: false,
                        }),
                        "more than 100"
                    );
                    assert_eq!(
                        breadth_words(&MatchCount {
                            files: 7,
                            capped: false,
                            truncated: true,
                        }),
                        "at least 7",
                        "a walk cut short before the cap names a floor, not the limit"
                    );
                    assert_eq!(
                        breadth_words(&MatchCount {
                            files: 3,
                            capped: false,
                            truncated: false,
                        }),
                        "3"
                    );
                }

                #[test]
                #[serial]
                fn allow_of_a_broad_pattern_prints_the_count_and_confirms_before_writing() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-broad");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-broad",
                            &["docs/a.md", "docs/b.md", "docs/c.md"],
                        )
                        .await;

                        // `**` is broad by its head alone, even over an empty match.
                        let declined = prompt_script::install(&[false]);
                        let out = out_of(&mut fx.ctx, ".mesh allow **").await.unwrap();
                        assert!(
                            out.contains(&format!(
                                "`**` matches 3 file(s) under {}.",
                                fx.root.path.display()
                            )),
                            "{out}"
                        );
                        assert!(out.contains(NOTHING_CHANGED), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert!(!fx.locations.global.exists());
                        drop(declined);

                        let non_tty = prompt_script::install_non_interactive();
                        let err = refusal(&mut fx.ctx, ".mesh allow **").await;
                        assert!(
                            err.contains("Share 3 files matching `**` with every trusted peer?"),
                            "{err}"
                        );
                        assert!(err.contains("--yes"), "{err}");
                        assert!(!fx.locations.global.exists());
                        drop(non_tty);

                        // Three files under a literal head: written without a question.
                        let _script = prompt_script::install(&[]);
                        let out = out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        assert!(out.contains("Allowed `docs/**`"), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;

                        let many: Vec<String> =
                            (0..101).map(|i| format!("docs/f{i:03}.md")).collect();
                        let many: Vec<&str> = many.iter().map(String::as_str).collect();
                        let mut fx = ShareFixture::new("repl-mesh-allow-broad-many", &many).await;
                        let accepted = prompt_script::install(&[true]);
                        let out = out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        assert!(
                            out.contains(&format!(
                                "`docs/**` matches more than 100 file(s) under {}.",
                                fx.root.path.display()
                            )),
                            "{out}"
                        );
                        assert!(out.contains("Allowed `docs/**`"), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(fx.entries(), [allow_entry(Layer::Global, "docs/**", None)]);
                        drop(accepted);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_dry_run_prints_the_count_and_the_target_and_writes_nothing() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-dry-run");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-dry-run", &["docs/a.md"]).await;

                        for line in [".mesh allow docs/** --dry-run", ".mesh allow ** --dry-run"] {
                            let out = out_of(&mut fx.ctx, line).await.unwrap();
                            assert!(out.contains("matches 1 file(s) under"), "{line}: {out}");
                            assert!(
                                out.contains(&format!("Would write to {}: allow `", fx.global())),
                                "{line}: {out}"
                            );
                            assert!(out.ends_with(DRY_RUN_NOTHING_CHANGED), "{line}: {out}");
                        }
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        assert!(!fx.locations.global.exists());
                        assert!(!fx.locations.workspace.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_force_on_a_built_in_denied_file_writes_allow_and_override_to_the_global_file()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new("repl-mesh-allow-force", &[".env"]).await;

                        let out = out_of(&mut fx.ctx, ".mesh allow .env --force --global")
                            .await
                            .unwrap();

                        assert!(
                            out.contains(&format!(
                                "Will write to {}: allow `.env` for every trusted peer, and an override lifting the built-in deny for it.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(
                            out.contains("The built-in deny for `.env` is lifted by an override in the same file."),
                            "{out}"
                        );
                        let yaml = fs::read_to_string(&fx.locations.global).unwrap();
                        assert!(yaml.contains("pattern: .env"), "{yaml}");
                        assert!(yaml.contains("path: .env"), "{yaml}");
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Global, ".env", None),
                                override_entry(Layer::Global, ".env"),
                            ]
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// The lift is judged on the resolved name, so the written override must
                /// carry it: the effective view shows the file without the built-in mark.
                #[test]
                #[serial]
                fn allow_force_lifts_the_built_in_deny_in_the_effective_view() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force-effective");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-force-effective",
                            &[".env", "docs/a.md"],
                        )
                        .await;

                        out_of(&mut fx.ctx, ".mesh allow .env --force --global")
                            .await
                            .unwrap();
                        let out = out_of(&mut fx.ctx, ".mesh shares --effective")
                            .await
                            .unwrap();

                        let lines: Vec<&str> = out.lines().collect();
                        assert_eq!(
                            lines,
                            [
                                format!(
                                    "Files every trusted peer can fetch from {}:",
                                    fx.root.path.display()
                                )
                                .as_str(),
                                "  .env",
                            ],
                            "{out}"
                        );
                        fx.stop().await;
                    });
                }

                /// An override of the link's text would never match the resolved secret,
                /// so writing it would only print a false lift; the verb names the file
                /// the override has to carry instead and writes nothing.
                #[test]
                #[serial]
                fn allow_force_on_a_link_to_a_secret_names_the_resolved_file_and_writes_nothing() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force-link");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-force-link", &[".env", "docs/a.md"])
                                .await;
                        std::os::unix::fs::symlink("../.env", fx.root.path.join("docs/settings"))
                            .unwrap();

                        let err =
                            refusal(&mut fx.ctx, ".mesh allow docs/settings --force --global")
                                .await;

                        assert_eq!(
                            err,
                            "`docs/settings` resolves to `.env`, which the built-in deny names; `.mesh allow .env --force --global` lifts that file."
                        );
                        assert!(!fx.locations.global.exists());
                        assert!(fx.entries().is_empty());

                        let err = refusal(&mut fx.ctx, ".mesh allow docs/settings").await;
                        assert!(
                            err.starts_with("`docs/settings` is under the built-in deny"),
                            "{err}"
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_force_under_the_workspace_layer_teaches_global() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force-workspace");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-force-workspace",
                            &[".env", "docs/a.md"],
                        )
                        .await;
                        let teaching = format!(
                            "Overrides are honoured from the global share file only, so `--force` cannot be written to {}; pass `--global`.",
                            fx.workspace()
                        );

                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh allow .env --force --workspace").await,
                            teaching
                        );
                        out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();
                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh allow .env --force").await,
                            teaching
                        );

                        assert_eq!(
                            fx.entries(),
                            [allow_entry(Layer::Workspace, "docs/**", None)]
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_force_with_a_glob_is_refused_naming_one_exact_file() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force-glob");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-force-glob", &["k.pem"]).await;

                        let err = refusal(&mut fx.ctx, ".mesh allow *.pem --force --global").await;

                        assert_eq!(
                            err,
                            "An override lifts the built-in deny for one exact file; `*.pem` is a pattern. Name the file."
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_force_on_a_file_the_built_in_deny_does_not_name_is_refused() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force-plain");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-force-plain", &["README.md"]).await;

                        let err =
                            refusal(&mut fx.ctx, ".mesh allow README.md --force --global").await;

                        assert_eq!(
                            err,
                            "`README.md` is not under the built-in deny, so there is nothing for `--force` to lift; drop `--force`."
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_of_a_built_in_denied_file_without_force_teaches_force_global() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-builtin-plain");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-builtin-plain", &[".env"]).await;

                        let err = refusal(&mut fx.ctx, ".mesh allow .env").await;

                        assert_eq!(
                            err,
                            "`.env` is under the built-in deny, so an allow alone would share nothing; `.mesh allow .env --force --global` lifts it for this one file."
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_inside_git_or_the_workspace_config_dir_is_refused_even_with_force() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-protected");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-allow-protected",
                            &[".git/config", ".coyote/sessions/x.yaml"],
                        )
                        .await;

                        for (line, head) in [
                            (".mesh allow .git/**", ".git"),
                            (".mesh allow .git/config --force --global", ".git"),
                            (".mesh allow .coyote/**", ".coyote"),
                        ] {
                            let pattern = line.split_whitespace().nth(2).unwrap();
                            assert_eq!(
                                refusal(&mut fx.ctx, line).await,
                                format!(
                                    "`{pattern}` is under `{head}/`, which is never shared, not even with `--force`; nothing was written."
                                ),
                                "{line}"
                            );
                        }
                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh deny .git/**").await,
                            "`.git/**` is under `.git/`, which is never shared, so no deny is needed; nothing was written."
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                /// The completer's directory value is what a human most likely types next,
                /// so the slash gets its own sentence rather than the empty-segment refusal.
                #[test]
                #[serial]
                fn a_pattern_ending_in_a_slash_teaches_the_double_star_and_writes_nothing() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-trailing-slash");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-trailing-slash", &["docs/a.md"]).await;

                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh allow docs/").await,
                            "`docs/` names a directory; share what is under it with `docs/**`, or one file by its path."
                        );
                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh deny docs/").await,
                            "`docs/` names a directory; keep what is under it back with `docs/**`, or one file by its path."
                        );
                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh unshare docs/").await,
                            "`docs/` names a directory; name what is under it with `docs/**`, or one file by its path."
                        );
                        for line in [".mesh allow /", ".mesh deny //", ".mesh unshare /docs/"] {
                            let err = refusal(&mut fx.ctx, line).await;
                            assert!(
                                err.contains("Share patterns are relative to the workspace root"),
                                "{line}: a root-anchored slash is not a directory to glob under: {err}"
                            );
                        }
                        assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                        assert!(!fx.locations.global.exists());
                        assert!(!fx.locations.workspace.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_of_an_absolute_pattern_is_refused_with_the_relative_teaching() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-absolute");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-absolute", &["docs/a.md"]).await;

                        let err = refusal(&mut fx.ctx, ".mesh allow /etc/passwd").await;

                        assert!(
                            err.contains("Share patterns are relative to the workspace root"),
                            "{err}"
                        );
                        assert!(err.ends_with("drop that prefix."), "{err}");
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_scoped_to_a_non_hash_peer_is_refused_naming_mesh_peers() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-peer");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-peer", &["docs/a.md"]).await;

                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh allow docs/** --peer bob").await,
                            "`bob` is not a peer hash; scope a share to a peer by the 32-hex identity or destination hash `.mesh peers` shows."
                        );
                        assert!(!fx.locations.global.exists());

                        let peer = "ab".repeat(16);
                        let out = out_of(
                            &mut fx.ctx,
                            &format!(".mesh allow docs/** --peer {}", peer.to_uppercase()),
                        )
                        .await
                        .unwrap();
                        assert!(
                            out.contains(&format!(
                                "Allowed `docs/**` for peer {}; written to",
                                short(&peer)
                            )),
                            "{out}"
                        );
                        assert_eq!(
                            fx.entries(),
                            [allow_entry(Layer::Global, "docs/**", Some(&peer))]
                        );
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn allow_already_present_changes_nothing_and_says_so() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-again");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-again", &["docs/a.md"]).await;
                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        let before = fs::read(&fx.locations.global).unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();

                        assert!(
                            out.ends_with(&format!(
                                "`docs/**` is already allowed for every trusted peer in {}; nothing was changed.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(
                            !out.contains("write to"),
                            "no write is announced when nothing needs writing: {out}"
                        );
                        assert_eq!(fs::read(&fx.locations.global).unwrap(), before);
                        fx.stop().await;
                    });
                }

                /// The refusal is pure, so it fires before the mesh gate: an off context
                /// hears it, not `MESH_OFF`, and no share file comes into being.
                #[test]
                #[serial]
                fn allow_force_with_peer_is_refused_before_the_mesh_gate_and_writes_nothing() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-force-peer");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-allow-force-peer-root");
                    seed_files(&root.path, &[".env"]);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    let peer = "ab".repeat(16);

                    let err = err_of(&mut ctx, &format!(".mesh allow .env --force --peer {peer}"));

                    assert_eq!(
                        err,
                        "`--force` lifts the built-in deny for every peer whose allow names `.env`, so it cannot be scoped with `--peer`; drop `--peer` (every trusted peer with a matching allow will see the file)."
                    );
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                    assert!(!locations.global.exists());
                    assert!(!locations.workspace.exists());
                }

                /// Both usage errors come before the mesh gate, so an off context pins them.
                #[test]
                #[serial]
                fn allow_yes_with_dry_run_is_a_usage_error() {
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    for verb in ["allow", "deny", "unshare"] {
                        let err =
                            err_of(&mut ctx, &format!(".mesh {verb} docs/** --yes --dry-run"));
                        assert!(err.starts_with("Unexpected '--yes'"), "{verb}: {err}");
                        assert!(err.contains(&format!(".mesh {verb}")), "{verb}: {err}");
                        assert_ne!(err, MESH_OFF, "{verb}: the usage check comes first");
                    }
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                }

                #[test]
                #[serial]
                fn allow_global_with_workspace_is_a_usage_error() {
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    for verb in ["allow", "deny", "unshare"] {
                        let err = err_of(
                            &mut ctx,
                            &format!(".mesh {verb} docs/** --global --workspace"),
                        );
                        assert!(
                            err.starts_with(
                                "`--global` and `--workspace` name different files; pass one."
                            ),
                            "{verb}: {err}"
                        );
                        assert!(
                            err.contains(&format!("Usage: .mesh {verb}")),
                            "{verb}: {err}"
                        );
                    }
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                }

                #[test]
                #[serial]
                fn allow_of_a_destination_hash_teaches_trust_and_peer() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-hash");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-hash", &["docs/a.md"]).await;
                        let dest = "3f9c2a7b1d4e6f80a1b2c3d4e5f60718";

                        let err =
                            refusal(&mut fx.ctx, &format!(".mesh allow {}", dest.to_uppercase()))
                                .await;

                        assert_eq!(
                            err,
                            format!(
                                "`.mesh allow` takes a file pattern, not a peer; to trust the instance {} run `.mesh trust {dest}`, or scope a share to it with `--peer {dest}`.",
                                short(dest)
                            )
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn deny_of_a_destination_hash_teaches_untrust() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-deny-hash");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new("repl-mesh-deny-hash", &["docs/a.md"]).await;
                        let dest = "3f9c2a7b1d4e6f80a1b2c3d4e5f60718";

                        let err =
                            refusal(&mut fx.ctx, &format!(".mesh deny {}", dest.to_uppercase()))
                                .await;

                        assert_eq!(
                            err,
                            format!(
                                "`.mesh deny` takes a file pattern, not a peer; to refuse the instance {} run `.mesh untrust {dest}`.",
                                short(dest)
                            )
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn deny_writes_the_rule_and_confirms_when_broad() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-deny-writes");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-deny-writes",
                            &["src/vault/k.txt", "docs/a.md"],
                        )
                        .await;

                        let quiet = prompt_script::install(&[]);
                        let out = out_of(&mut fx.ctx, ".mesh deny \"src/vault/*\"")
                            .await
                            .unwrap();
                        assert!(
                            out.contains(&format!(
                                "Will write to {}: deny `src/vault/*` to every peer.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(
                            out.ends_with(&format!(
                                "Denied `src/vault/*` to every peer; written to {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        drop(quiet);

                        let declined = prompt_script::install(&[false]);
                        let out = out_of(&mut fx.ctx, ".mesh deny **").await.unwrap();
                        assert!(out.ends_with(NOTHING_CHANGED), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        drop(declined);

                        let accepted = prompt_script::install(&[true]);
                        let out = out_of(&mut fx.ctx, ".mesh deny **").await.unwrap();
                        assert!(out.contains("Denied `**` to every peer"), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        let out = out_of(&mut fx.ctx, ".mesh deny ** --yes").await.unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "`**` is already denied to every peer in {}; nothing was changed.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        drop(accepted);

                        assert_eq!(
                            fx.entries(),
                            [
                                deny_entry(Layer::Global, "src/vault/*"),
                                deny_entry(Layer::Global, "**"),
                            ]
                        );
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn unshare_removes_the_allow_or_deny_that_holds_the_pattern() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unshare");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-unshare", &["docs/a.md", "secrets/s"])
                                .await;
                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        out_of(&mut fx.ctx, ".mesh deny secrets/**").await.unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/** --dry-run")
                            .await
                            .unwrap();
                        assert!(
                            out.contains(&format!(
                                "Would remove the allow for `docs/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(out.ends_with(DRY_RUN_NOTHING_CHANGED), "{out}");
                        assert_eq!(fx.entries().len(), 2);

                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/**").await.unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Removed the allow for `docs/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(fx.entries(), [deny_entry(Layer::Global, "secrets/**")]);

                        let out = out_of(&mut fx.ctx, ".mesh unshare secrets/**")
                            .await
                            .unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Removed the deny for `secrets/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(fx.entries().is_empty());
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn unshare_with_holders_in_both_files_lists_them_and_confirms() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unshare-both");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-unshare-both", &["docs/a.md"]).await;
                        let quiet = prompt_script::install(&[]);
                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();
                        drop(quiet);
                        let both = [
                            allow_entry(Layer::Global, "docs/**", None),
                            allow_entry(Layer::Workspace, "docs/**", None),
                        ];
                        assert_eq!(fx.entries(), both);

                        let declined = prompt_script::install(&[false]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/**").await.unwrap();
                        let lines: Vec<&str> = out.lines().collect();
                        assert!(lines.contains(&"`docs/**` is held by:"), "{out}");
                        assert!(
                            lines.contains(&format!("  allow in {}", fx.global()).as_str()),
                            "{out}"
                        );
                        assert!(
                            lines.contains(&format!("  allow in {}", fx.workspace()).as_str()),
                            "{out}"
                        );
                        assert!(out.ends_with(NOTHING_CHANGED), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(fx.entries(), both);
                        drop(declined);

                        let accepted = prompt_script::install(&[true]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/**").await.unwrap();
                        assert!(
                            out.contains(&format!(
                                "Removed the allow for `docs/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(
                            out.contains(&format!(
                                "Removed the allow for `docs/**` from {}.",
                                fx.workspace()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert!(fx.entries().is_empty());
                        drop(accepted);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn unshare_under_auto_when_only_the_other_layer_holds_it_teaches_the_flag() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unshare-other-layer");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-unshare-other-layer",
                            &["docs/a.md", "notes/n.md"],
                        )
                        .await;
                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        out_of(&mut fx.ctx, ".mesh allow notes/** --workspace")
                            .await
                            .unwrap();

                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh unshare docs/**").await,
                            format!(
                                "`docs/**` is held by {g} (allow); the write rule reaches {w} — pass `--global` to remove it from {g}.",
                                g = fx.global(),
                                w = fx.workspace()
                            )
                        );
                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh unshare docs/** --workspace").await,
                            format!(
                                "No share rule in {} has the pattern `docs/**`; {} holds it — pass `--global` instead.",
                                fx.workspace(),
                                fx.global()
                            )
                        );
                        assert_eq!(fx.entries().len(), 2);

                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/** --global")
                            .await
                            .unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Removed the allow for `docs/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(
                            fx.entries(),
                            [allow_entry(Layer::Workspace, "notes/**", None)]
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn unshare_of_an_unknown_pattern_is_a_teaching_error_naming_shares() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unshare-unknown");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-unshare-unknown", &["docs/a.md"]).await;

                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh unshare nope/**").await,
                            format!(
                                "No share rule has the pattern `nope/**` in {} or {}; `.mesh shares` lists them.",
                                fx.global(),
                                fx.workspace()
                            )
                        );
                        assert!(!fx.locations.global.exists());
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn unshare_also_drops_the_override_written_by_force() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unshare-override");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-unshare-override", &[".env"]).await;
                        let quiet = prompt_script::install(&[]);
                        out_of(&mut fx.ctx, ".mesh allow .env --force --global")
                            .await
                            .unwrap();
                        assert_eq!(fx.entries().len(), 2);
                        drop(quiet);

                        let accepted = prompt_script::install(&[true]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare .env").await.unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Removed the allow and override for `.env` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert!(fx.entries().is_empty());
                        let yaml = fs::read_to_string(&fx.locations.global).unwrap();
                        assert!(!yaml.contains(".env"), "{yaml}");
                        drop(accepted);
                        fx.stop().await;
                    });
                }

                /// The write lands on the files as they are when the human answers, not on
                /// the copy that was shown: an entry added while the question stood survives.
                #[test]
                #[serial]
                fn allow_applies_to_the_share_list_as_it_is_after_the_prompt_not_before() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-reload");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-reload", &["docs/a.md"]).await;
                        let locations = fx.locations.clone();
                        let _script = prompt_script::install_answering(move |_question| {
                            ShareSet::load_quietly(locations.clone())
                                .0
                                .apply(
                                    Mutation::Allow {
                                        pattern: "notes/**".to_string(),
                                        peer: None,
                                    },
                                    WriteScope::Global,
                                )
                                .unwrap();
                            true
                        });

                        let out = out_of(&mut fx.ctx, ".mesh allow **").await.unwrap();

                        assert!(
                            out.ends_with(&format!(
                                "Allowed `**` for every trusted peer; written to {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Global, "notes/**", None),
                                allow_entry(Layer::Global, "**", None),
                            ]
                        );
                        fx.stop().await;
                    });
                }

                /// "Will write to" and "written to" must name the same file: a workspace
                /// file created while the question stands does not pull the write into it.
                #[test]
                #[serial]
                fn allow_writes_the_layer_it_announced_even_if_the_workspace_file_appears_during_the_prompt()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-allow-announced-layer");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-allow-announced-layer", &["docs/a.md"])
                                .await;
                        let workspace = fx.locations.workspace.clone();
                        let _script = prompt_script::install_answering(move |_question| {
                            write_share_file(
                                &workspace,
                                "version: 1\nallow:\n- pattern: docs/**\n",
                            );
                            true
                        });

                        let out = out_of(&mut fx.ctx, ".mesh allow **").await.unwrap();

                        assert!(
                            out.contains(&format!("Will write to {}:", fx.global())),
                            "{out}"
                        );
                        assert!(
                            out.ends_with(&format!(
                                "Allowed `**` for every trusted peer; written to {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Global, "**", None),
                                allow_entry(Layer::Workspace, "docs/**", None),
                            ]
                        );
                        fx.stop().await;
                    });
                }

                /// What the human confirmed removing must still be there when the answer
                /// comes; a holder that went away meanwhile means the question was stale.
                #[test]
                #[serial]
                fn unshare_refuses_when_the_holders_changed_while_the_prompt_was_open() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unshare-reload");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-unshare-reload", &["docs/a.md"]).await;
                        fx.write_global(
                            "version: 1\nallow:\n- pattern: '**'\n- pattern: docs/**\n",
                        );
                        let locations = fx.locations.clone();
                        let _script = prompt_script::install_answering(move |_question| {
                            ShareSet::load_quietly(locations.clone())
                                .0
                                .apply(
                                    Mutation::Unshare {
                                        pattern: "**".to_string(),
                                    },
                                    WriteScope::Global,
                                )
                                .unwrap();
                            true
                        });

                        let err = refusal(&mut fx.ctx, ".mesh unshare **").await;

                        assert_eq!(
                            err,
                            "The share list changed while the prompt was open; run `.mesh unshare` again."
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(fx.entries(), [allow_entry(Layer::Global, "docs/**", None)]);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn shares_lists_every_rule_with_its_file_and_peer_scope_and_flags_inert_workspace_overrides()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-list");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-shares-list", &["docs/a.md"]).await;
                        let peer = "cd".repeat(16);
                        fx.write_global(&format!(
                            "version: 1\nallow:\n- pattern: docs/**\n- pattern: src/**\n  peer: {peer}\n- pattern: etc/**\n  peer: nobody\ndeny:\n- pattern: secrets/**\noverride:\n- path: .env\n"
                        ));
                        fx.write_workspace(
                            "version: 1\nallow:\n- pattern: notes/**\noverride:\n- path: k.pem\n",
                        );

                        let out = out_of(&mut fx.ctx, ".mesh shares").await.unwrap();

                        let lines: Vec<&str> = out.lines().collect();
                        assert_eq!(
                            lines[0],
                            format!(
                                "Share rules (global {}; workspace {}):",
                                fx.global(),
                                fx.workspace()
                            ),
                            "{out}"
                        );
                        let rows: Vec<Vec<&str>> = lines[1..]
                            .iter()
                            .map(|line| line.split_whitespace().collect())
                            .collect();
                        assert_eq!(
                            rows,
                            [
                                vec!["allow", "docs/**", "global", "every", "trusted", "peer"],
                                vec!["allow", "src/**", "global", "peer", short(&peer)],
                                vec![
                                    "allow",
                                    "etc/**",
                                    "global",
                                    "scoped",
                                    "to",
                                    "no",
                                    "peer",
                                    "(`nobody`",
                                    "is",
                                    "not",
                                    "a",
                                    "peer",
                                    "hash)"
                                ],
                                vec!["deny", "secrets/**", "global"],
                                vec![
                                    "override", ".env", "global", "lifts", "the", "built-in",
                                    "deny", "for", "this", "file"
                                ],
                                vec!["allow", "notes/**", "workspace", "every", "trusted", "peer"],
                                vec![
                                    "override",
                                    "k.pem",
                                    "workspace",
                                    "ignored:",
                                    "overrides",
                                    "are",
                                    "honoured",
                                    "from",
                                    "the",
                                    "global",
                                    "file",
                                    "only"
                                ],
                            ],
                            "{out}"
                        );
                        assert!(!out.contains(&peer), "no full hash in the listing: {out}");

                        let other = "ef".repeat(16);
                        let out = out_of(&mut fx.ctx, &format!(".mesh shares --peer {other}"))
                            .await
                            .unwrap();
                        assert!(out.starts_with("Share rules ("), "{out}");
                        assert!(
                            out.lines()
                                .next()
                                .unwrap()
                                .ends_with(&format!(" for peer {}:", short(&other))),
                            "{out}"
                        );
                        assert!(out.contains("docs/**"), "{out}");
                        assert!(
                            !out.contains("src/**"),
                            "a share scoped to another peer: {out}"
                        );
                        assert!(!out.contains("etc/**"), "a share scoped to nobody: {out}");
                        assert!(
                            out.contains("secrets/**"),
                            "denies apply to every peer: {out}"
                        );

                        let out = out_of(&mut fx.ctx, &format!(".mesh shares --peer {peer}"))
                            .await
                            .unwrap();
                        assert!(out.contains("src/**"), "{out}");

                        assert_eq!(
                            refusal(&mut fx.ctx, ".mesh shares docs/**").await,
                            format!("Unexpected 'docs/**'. {}", render_verb_help("shares"))
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                #[test]
                #[serial]
                fn shares_effective_lists_resolved_files_marking_built_in_and_user_denies() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-effective");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-shares-effective",
                            &["docs/a.md", "docs/b.md", "docs/.env"],
                        )
                        .await;

                        let out = out_of(&mut fx.ctx, ".mesh shares --effective")
                            .await
                            .unwrap();
                        assert_eq!(
                            out,
                            format!(
                                "Nothing resolves: no allow rule names an existing file under {}.",
                                fx.root.path.display()
                            )
                        );

                        fx.write_global(
                            "version: 1\nallow:\n- pattern: docs/**\ndeny:\n- pattern: docs/b.md\n",
                        );
                        let out = out_of(&mut fx.ctx, ".mesh shares --effective")
                            .await
                            .unwrap();
                        let lines: Vec<&str> = out.lines().collect();
                        assert_eq!(
                            lines,
                            [
                                format!(
                                    "Files every trusted peer can fetch from {}:",
                                    fx.root.path.display()
                                )
                                .as_str(),
                                "  docs/.env  (built-in deny)",
                                "  docs/a.md",
                                "  docs/b.md  (denied)",
                            ],
                            "{out}"
                        );

                        let peer = "ab".repeat(16);
                        let out = out_of(
                            &mut fx.ctx,
                            &format!(".mesh shares --effective --peer {peer}"),
                        )
                        .await
                        .unwrap();
                        assert!(
                            out.starts_with(&format!("Files peer {} can fetch from", short(&peer))),
                            "{out}"
                        );
                        assert!(!out.contains(&peer), "{out}");
                        fx.stop().await;
                    });
                }

                /// `shares` is inspection and reads the files the node would serve from
                /// even while it is off; the mutations refuse after their usage checks and
                /// before any prompt, like every other node verb.
                #[test]
                #[serial]
                fn shares_works_while_the_mesh_is_off_and_the_mutations_do_not() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-off");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[true; 4]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-shares-off-root");
                    seed_files(&root.path, &["docs/a.md"]);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();

                    run_async(run(&mut ctx, ".mesh shares")).unwrap();
                    let out = stdout_lines().join("\n");
                    assert!(
                        out.starts_with(&format!(
                            "No share rules in {} or {}.",
                            locations.global.display(),
                            locations.workspace.display()
                        )),
                        "{out}"
                    );
                    write_share_file(
                        &locations.global,
                        "version: 1\nallow:\n- pattern: docs/**\n",
                    );
                    run_async(run(&mut ctx, ".mesh shares --effective")).unwrap();
                    let out = stdout_lines().join("\n");
                    assert!(out.contains("  docs/a.md"), "{out}");

                    for line in [
                        ".mesh allow docs/**",
                        ".mesh allow ** --yes",
                        ".mesh deny docs/**",
                        ".mesh unshare docs/**",
                    ] {
                        assert_eq!(err_of(&mut ctx, line), MESH_OFF, "{line}");
                    }
                    for line in [
                        ".mesh allow docs/** --bogus",
                        ".mesh deny docs/** --peer x",
                        ".mesh unshare docs/** --force",
                    ] {
                        let err = err_of(&mut ctx, line);
                        assert!(err.starts_with("Unexpected '"), "{line}: {err}");
                        assert_ne!(err, MESH_OFF, "{line}: the usage check comes first");
                    }
                    for (line, teaching) in [
                        (
                            ".mesh allow /etc/passwd",
                            "Share patterns are relative to the workspace root",
                        ),
                        (
                            ".mesh unshare /etc/passwd",
                            "Share patterns are relative to the workspace root",
                        ),
                        (
                            ".mesh allow 3f9c2a7b1d4e6f80a1b2c3d4e5f60718",
                            "takes a file pattern, not a peer",
                        ),
                        (".mesh allow x --peer bob", "`bob` is not a peer hash"),
                        (".mesh deny docs/", "`docs/` names a directory"),
                        (".mesh unshare docs/", "`docs/` names a directory"),
                        (".mesh allow *.pem --force", "`*.pem` is a pattern"),
                    ] {
                        let err = err_of(&mut ctx, line);
                        assert!(err.contains(teaching), "{line}: {err}");
                        assert_ne!(err, MESH_OFF, "{line}: the pattern checks come first");
                    }
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    assert_eq!(
                        ShareSet::load_quietly(locations).0.entries(),
                        [allow_entry(Layer::Global, "docs/**", None)]
                    );
                }

                /// Rules exist but none reaches the named peer: the listing says so rather
                /// than claiming the files are empty.
                #[test]
                #[serial]
                fn shares_for_a_peer_no_rule_reaches_says_so_instead_of_no_rules_at_all() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-peer-no-rows");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-shares-peer-no-rows-root");
                    seed_files(&root.path, &["docs/a.md"]);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    let other = "ef".repeat(16);
                    write_share_file(
                        &locations.global,
                        &format!("version: 1\nallow:\n- pattern: docs/**\n  peer: {other}\n"),
                    );
                    let peer = "ab".repeat(16);

                    run_async(run(&mut ctx, &format!(".mesh shares --peer {peer}"))).unwrap();

                    assert_eq!(
                        stdout_lines().join("\n"),
                        format!(
                            "No share rule applies to peer {}; `.mesh shares` lists every rule.",
                            short(&peer)
                        )
                    );
                    assert_eq!(prompt_script::prompts_asked(), 0);
                }

                /// What the node will protect, the off-path views protect too: a configured
                /// inbox inside the share root is never entered under `**`, as the walk a
                /// running node does never enters it, so it is neither listed nor counted.
                #[test]
                #[serial]
                fn shares_and_the_on_preview_protect_the_configured_inbox_while_the_mesh_is_off() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-off-inbox");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false]);
                    let root = TempDir::new("repl-mesh-shares-off-inbox-root");
                    seed_files(&root.path, &["docs/a.md", "inbox/fetched.md"]);
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        fetch: crate::config::mesh_config::MeshFetch {
                            inbox_dir: Some(root.path.join("inbox")),
                            ..Default::default()
                        },
                        ..MeshConfig::default()
                    };
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(&locations.global, "version: 1\nallow:\n- pattern: '**'\n");

                    run_async(run(&mut ctx, ".mesh shares --effective")).unwrap();
                    let out = stdout_lines().join("\n");
                    assert!(out.ends_with("\n  docs/a.md"), "{out}");
                    assert!(!out.contains("inbox/"), "{out}");

                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines();
                    let files = index_of(
                        &out,
                        "  files: 1 path(s) are shared with trusted peers (`.mesh shares`)",
                    );
                    assert!(files < index_of(&out, "Mesh stays off"), "{out:?}");
                }

                /// The preview names what the share files already hand every trusted
                /// peer, so the user sees it before consenting; nothing is said when
                /// nothing is shared or no snapshot names the root.
                #[test]
                #[serial]
                fn mesh_on_preview_names_the_shared_path_count_only_when_the_effective_set_is_non_empty()
                 {
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        ..MeshConfig::default()
                    };
                    let clause = |shared: Option<MatchCount>| {
                        render_on_preview(&config, "work", false, shared)
                            .lines()
                            .find(|line| line.starts_with("  files:"))
                            .map(str::to_string)
                    };
                    assert_eq!(
                        clause(Some(MatchCount {
                            files: 3,
                            capped: false,
                            truncated: false,
                        }))
                        .as_deref(),
                        Some("  files: 3 path(s) are shared with trusted peers (`.mesh shares`)")
                    );
                    assert_eq!(
                        clause(Some(MatchCount {
                            files: LIST_PAGE_SIZE,
                            capped: true,
                            truncated: false,
                        }))
                        .as_deref(),
                        Some(
                            "  files: 1000 or more path(s) are shared with trusted peers (`.mesh shares`)"
                        )
                    );
                    assert_eq!(
                        clause(Some(MatchCount {
                            files: 7,
                            capped: false,
                            truncated: true,
                        }))
                        .as_deref(),
                        Some(
                            "  files: at least 7 path(s) are shared with trusted peers (`.mesh shares`)"
                        ),
                        "a walk cut short names a floor, not a count"
                    );
                    assert_eq!(
                        clause(Some(MatchCount {
                            files: LIST_PAGE_SIZE,
                            capped: true,
                            truncated: true,
                        }))
                        .as_deref(),
                        Some(
                            "  files: 1000 or more path(s) are shared with trusted peers (`.mesh shares`)"
                        ),
                        "the cap wins when both hold"
                    );
                    assert_eq!(
                        clause(Some(MatchCount {
                            files: 0,
                            capped: false,
                            truncated: false,
                        })),
                        None
                    );
                    assert_eq!(clause(None), None);

                    let _guard = TestConfigDirGuard::new("repl-mesh-on-preview-shares");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false]);
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    let root = TempDir::new("repl-mesh-on-preview-shares-root");
                    seed_files(
                        &root.path,
                        &["docs/a.md", "docs/b.md", "docs/.env", "src/main.rs"],
                    );
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(
                        &locations.global,
                        "version: 1\nallow:\n- pattern: docs/**\n",
                    );

                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines();
                    let files = index_of(
                        &out,
                        "  files: 2 path(s) are shared with trusted peers (`.mesh shares`)",
                    );
                    assert!(files < index_of(&out, "Mesh stays off"), "{out:?}");
                }

                #[test]
                #[serial]
                fn share_verbs_without_a_share_root_teach_that_it_is_unknown_until_a_turn_completes()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-no-root");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut off = off_ctx();
                    for line in [".mesh shares", ".mesh shares --effective"] {
                        assert_eq!(err_of(&mut off, line), SHARE_ROOT_UNKNOWN, "{line}");
                    }
                    run_async(async {
                        let started = started_runtime("repl-mesh-shares-no-root").await;
                        let mut ctx = ctx_with(MeshConfig::default(), true);
                        ctx.app.mesh.install(started.runtime.clone()).unwrap();
                        for line in [
                            ".mesh allow docs/**",
                            ".mesh deny docs/**",
                            ".mesh unshare docs/**",
                        ] {
                            assert_eq!(refusal(&mut ctx, line).await, SHARE_ROOT_UNKNOWN, "{line}");
                        }
                        assert!(ctx.app.mesh.stop().await.unwrap());
                        started.relay_handle.abort();
                    });
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                    assert_eq!(prompt_script::prompts_asked(), 0);
                }

                #[test]
                #[serial]
                fn a_refused_share_file_blocks_the_mutations_and_shares_says_so() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-refused");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[true; 4]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-shares-refused", &["docs/a.md"]).await;
                        fx.write_global("version: 99\n");
                        let before = fs::read(&fx.locations.global).unwrap();

                        for line in [
                            ".mesh allow docs/**",
                            ".mesh deny docs/**",
                            ".mesh unshare docs/**",
                        ] {
                            let err = refusal(&mut fx.ctx, line).await;
                            assert!(err.contains(&fx.global()), "{line}: {err}");
                            assert!(err.ends_with("Nothing was written."), "{line}: {err}");
                        }
                        assert_eq!(fs::read(&fx.locations.global).unwrap(), before);
                        assert!(!fx.locations.workspace.exists());

                        let out = out_of(&mut fx.ctx, ".mesh shares").await.unwrap();
                        assert_eq!(out, "Nothing is shared until it is fixed.");
                        let warned = stderr_lines();
                        assert_eq!(warned.len(), 1, "{warned:?}");
                        assert!(warned[0].contains(&fx.global()), "{warned:?}");
                        assert!(warned[0].contains("version"), "{warned:?}");
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: "more than one file/kind holds it" includes one file holding
                /// the pattern as both an allow and a deny; `unshare` names each holder
                /// with its kind, asks once, removes nothing on no and both on yes; a
                /// dry run previews both removals and writes nothing.
                #[test]
                #[serial]
                fn usage_probe_unshare_with_an_allow_and_a_deny_in_one_file_lists_both_kinds_and_confirms()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-unshare-kinds");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-probe-unshare-kinds", &["docs/a.md"])
                                .await;
                        fx.write_global(
                            "version: 1\nallow:\n- pattern: docs/**\ndeny:\n- pattern: docs/**\n",
                        );
                        let both = [
                            allow_entry(Layer::Global, "docs/**", None),
                            deny_entry(Layer::Global, "docs/**"),
                        ];
                        assert_eq!(fx.entries(), both);

                        let quiet = prompt_script::install(&[]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/** --dry-run")
                            .await
                            .unwrap();
                        assert!(out.contains("allow"), "{out}");
                        assert!(out.contains("deny"), "{out}");
                        assert!(out.ends_with(DRY_RUN_NOTHING_CHANGED), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        assert_eq!(fx.entries(), both);
                        drop(quiet);

                        let declined = prompt_script::install(&[false]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/**").await.unwrap();
                        let lines: Vec<&str> = out.lines().collect();
                        assert!(lines.contains(&"`docs/**` is held by:"), "{out}");
                        assert!(
                            lines.contains(&format!("  allow in {}", fx.global()).as_str()),
                            "{out}"
                        );
                        assert!(
                            lines.contains(&format!("  deny in {}", fx.global()).as_str()),
                            "{out}"
                        );
                        assert!(out.ends_with(NOTHING_CHANGED), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(fx.entries(), both);
                        drop(declined);

                        let accepted = prompt_script::install(&[true]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/**").await.unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Removed the allow and deny for `docs/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert!(fx.entries().is_empty());
                        drop(accepted);

                        // The pattern is reusable once removed.
                        let _quiet = prompt_script::install(&[]);
                        let out = out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        assert!(out.contains("Allowed `docs/**`"), "{out}");
                        assert_eq!(fx.entries(), [allow_entry(Layer::Global, "docs/**", None)]);
                        fx.stop().await;
                    });
                }

                /// Usage probe: when both files hold the pattern, the layer flag stands in for
                /// the question — `--global` removes the global copy only, without a prompt,
                /// and the workspace copy keeps serving.
                #[test]
                #[serial]
                fn usage_probe_unshare_layer_flag_resolves_holders_in_both_files_without_a_prompt()
                {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-unshare-layer");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-probe-unshare-layer", &["docs/a.md"])
                                .await;
                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();
                        assert_eq!(fx.entries().len(), 2);

                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/** --global")
                            .await
                            .unwrap();

                        assert!(
                            out.ends_with(&format!(
                                "Removed the allow for `docs/**` from {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(!out.contains(&fx.workspace()), "{out}");
                        assert_eq!(
                            fx.entries(),
                            [allow_entry(Layer::Workspace, "docs/**", None)]
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);

                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/** --workspace")
                            .await
                            .unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Removed the allow for `docs/**` from {}.",
                                fx.workspace()
                            )),
                            "{out}"
                        );
                        assert!(fx.entries().is_empty());
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: the completer offers override paths under `unshare`, so the
                /// verb takes one — an inert workspace override is removed like any other
                /// entry, and `.mesh shares` stops flagging it.
                #[test]
                #[serial]
                fn usage_probe_unshare_removes_a_lone_override_entry() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-unshare-override-only");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-unshare-override-only",
                            &["docs/a.md"],
                        )
                        .await;
                        fx.write_workspace("version: 1\noverride:\n- path: k.pem\n");
                        let out = out_of(&mut fx.ctx, ".mesh shares").await.unwrap();
                        assert!(out.contains("ignored: overrides are honoured"), "{out}");

                        let out = out_of(&mut fx.ctx, ".mesh unshare k.pem").await.unwrap();

                        assert!(out.contains("k.pem"), "{out}");
                        assert!(out.contains(&fx.workspace()), "{out}");
                        assert!(fx.entries().is_empty(), "{:?}", fx.entries());
                        let out = out_of(&mut fx.ctx, ".mesh shares").await.unwrap();
                        assert!(!out.contains("k.pem"), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: a `deny` dry run prints the match count and the target file
                /// and writes nothing, like `allow`'s.
                #[test]
                #[serial]
                fn usage_probe_deny_dry_run_prints_the_count_and_target_and_writes_nothing() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-deny-dry-run");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-deny-dry-run",
                            &["docs/a.md", "docs/b.md"],
                        )
                        .await;

                        let out = out_of(&mut fx.ctx, ".mesh deny docs/** --dry-run")
                            .await
                            .unwrap();

                        assert!(
                            out.contains(&format!(
                                "`docs/**` matches 2 file(s) under {}.",
                                fx.root.path.display()
                            )),
                            "{out}"
                        );
                        assert!(
                            out.contains(&format!(
                                "Would write to {}: deny `docs/**`",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(out.ends_with(DRY_RUN_NOTHING_CHANGED), "{out}");
                        assert!(!fx.locations.global.exists());
                        assert!(!fx.locations.workspace.exists());
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: a peer-scoped allow resolves for that peer only — the
                /// every-trusted-peer listing leaves it out, `--peer` brings it in — and the
                /// listing never prints what the files contain.
                #[test]
                #[serial]
                fn usage_probe_shares_effective_scopes_peer_allows_and_never_prints_contents() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-effective-peer");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-effective-peer",
                            &["docs/a.md", "src/main.rs"],
                        )
                        .await;
                        const SECRET: &str = "PROBE-FILE-CONTENT-7f3a";
                        fs::write(fx.root.path.join("docs/a.md"), SECRET).unwrap();
                        fs::write(fx.root.path.join("src/main.rs"), SECRET).unwrap();
                        let peer = "ab".repeat(16);
                        let other = "ef".repeat(16);
                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        out_of(&mut fx.ctx, &format!(".mesh allow src/** --peer {peer}"))
                            .await
                            .unwrap();

                        let everyone = out_of(&mut fx.ctx, ".mesh shares --effective")
                            .await
                            .unwrap();
                        assert!(everyone.contains("  docs/a.md"), "{everyone}");
                        assert!(!everyone.contains("src/main.rs"), "{everyone}");

                        let scoped = out_of(
                            &mut fx.ctx,
                            &format!(".mesh shares --effective --peer {peer}"),
                        )
                        .await
                        .unwrap();
                        assert!(scoped.contains("  docs/a.md"), "{scoped}");
                        assert!(scoped.contains("  src/main.rs"), "{scoped}");

                        let unscoped = out_of(
                            &mut fx.ctx,
                            &format!(".mesh shares --effective --peer {other}"),
                        )
                        .await
                        .unwrap();
                        assert!(unscoped.contains("  docs/a.md"), "{unscoped}");
                        assert!(!unscoped.contains("src/main.rs"), "{unscoped}");

                        let rules = out_of(&mut fx.ctx, ".mesh shares").await.unwrap();
                        for out in [&everyone, &scoped, &unscoped, &rules] {
                            assert!(!out.contains(SECRET), "{out}");
                            assert!(!out.contains(&peer), "{out}");
                        }
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: `--effective` is bounded like `/list` — more files than one
                /// page shows stop at the page size with a line saying the listing was
                /// capped, rather than scrolling every file past the user.
                #[test]
                #[serial]
                fn usage_probe_shares_effective_is_bounded_like_list() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-effective-bound");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let many: Vec<String> = (0..LIST_PAGE_SIZE + 1)
                            .map(|i| format!("docs/f{i:04}.md"))
                            .collect();
                        let many: Vec<&str> = many.iter().map(String::as_str).collect();
                        let mut fx =
                            ShareFixture::new("repl-mesh-probe-effective-bound", &many).await;
                        out_of(&mut fx.ctx, ".mesh allow docs/** --yes")
                            .await
                            .unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh shares --effective")
                            .await
                            .unwrap();

                        let files = out
                            .lines()
                            .filter(|line| line.starts_with("  docs/"))
                            .count();
                        assert_eq!(files, LIST_PAGE_SIZE, "{}", out.lines().count());
                        assert!(
                            !out.contains(&format!("docs/f{:04}.md", LIST_PAGE_SIZE)),
                            "{out}"
                        );
                        assert!(out.contains("capped"), "{out}");
                        assert!(out.contains(&LIST_PAGE_SIZE.to_string()), "{out}");
                        fx.stop().await;
                    });
                }

                /// Usage probe (B-22): the preview counts the files the PEER-LESS allows reach
                /// after the user denies — a peer-scoped allow and a denied file add nothing —
                /// and says nothing at all when only peer-scoped allows exist.
                #[test]
                #[serial]
                fn usage_probe_on_preview_count_skips_peer_scoped_allows_and_user_denied_files() {
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        ..MeshConfig::default()
                    };
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-on-preview-count");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false, false]);
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    let root = TempDir::new("repl-mesh-probe-on-preview-count-root");
                    seed_files(
                        &root.path,
                        &["docs/a.md", "docs/b.md", "docs/c.md", "src/main.rs"],
                    );
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    let peer = "ab".repeat(16);

                    write_share_file(
                        &locations.global,
                        &format!(
                            "version: 1\nallow:\n- pattern: docs/**\n- pattern: src/**\n  peer: {peer}\ndeny:\n- pattern: docs/b.md\n"
                        ),
                    );
                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines();
                    let files = index_of(
                        &out,
                        "  files: 2 path(s) are shared with trusted peers (`.mesh shares`)",
                    );
                    assert!(files < index_of(&out, "Mesh stays off"), "{out:?}");
                    assert_eq!(
                        out.iter()
                            .filter(|line| line.contains("shared with trusted peers"))
                            .count(),
                        1,
                        "{out:?}"
                    );

                    let before = stdout_lines().len();
                    write_share_file(
                        &locations.global,
                        &format!("version: 1\nallow:\n- pattern: src/**\n  peer: {peer}\n"),
                    );
                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines()[before..].to_vec();
                    assert!(
                        !out.iter()
                            .any(|line| line.contains("shared with trusted peers")),
                        "{out:?}"
                    );
                    assert!(
                        out.iter().any(|line| line.contains("Mesh stays off")),
                        "{out:?}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 2);
                }

                /// Usage probe: bare `allow`/`deny`/`unshare` print usage and touch nothing
                /// whatever the node's state — on without a snapshot, on with one.
                #[test]
                #[serial]
                fn usage_probe_bare_share_verbs_print_usage_while_on_with_or_without_a_root() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-bare-on");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let started = started_runtime("repl-mesh-probe-bare-on").await;
                        let mut ctx = ctx_with(MeshConfig::default(), true);
                        ctx.app.mesh.install(started.runtime.clone()).unwrap();
                        for verb in ["allow", "deny", "unshare"] {
                            let out = out_of(&mut ctx, &format!(".mesh {verb}")).await.unwrap();
                            assert!(
                                out.contains(&format!("Usage: .mesh {verb}")),
                                "{verb}: {out}"
                            );
                        }
                        let root = TempDir::new("repl-mesh-probe-bare-on-root");
                        seed_files(&root.path, &["docs/a.md"]);
                        publish_root(&ctx, &root.path);
                        for verb in ["allow", "deny", "unshare"] {
                            let out = out_of(&mut ctx, &format!(".mesh {verb}")).await.unwrap();
                            assert!(
                                out.contains(&format!("Usage: .mesh {verb}")),
                                "{verb}: {out}"
                            );
                        }
                        let (_, locations) = share_locations(&ctx).unwrap();
                        assert!(!locations.global.exists());
                        assert!(!locations.workspace.exists());
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        assert!(ctx.app.mesh.stop().await.unwrap());
                        started.relay_handle.abort();
                    });
                }

                /// Usage probe: the withdrawn-spelling classification (`deny <32-hex>`) comes
                /// before the mesh gate, so an off context hears the teaching, not `MESH_OFF`;
                /// `shares --peer` refuses a non-hash rather than listing for "bob".
                #[test]
                #[serial]
                fn usage_probe_classification_errors_precede_mesh_off_and_shares_refuses_a_non_hash_peer()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-classify-off");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let dest = "3f9c2a7b1d4e6f80a1b2c3d4e5f60718";

                    let err = err_of(&mut ctx, &format!(".mesh deny {dest}"));
                    assert!(err.contains(&format!(".mesh untrust {dest}")), "{err}");
                    assert_ne!(err, MESH_OFF);

                    let root = TempDir::new("repl-mesh-probe-classify-off-root");
                    seed_files(&root.path, &["docs/a.md"]);
                    publish_root(&ctx, &root.path);
                    let err = err_of(&mut ctx, ".mesh shares --peer bob");
                    assert!(err.contains("`bob`"), "{err}");
                    assert!(err.contains("peer hash"), "{err}");
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                }

                /// Usage probe (B-22): the live preview count stops at the page size and says
                /// "1000 or more" instead of walking every file past the user.
                #[test]
                #[serial]
                fn usage_probe_on_preview_count_is_capped_at_the_page_size() {
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        ..MeshConfig::default()
                    };
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-on-preview-cap");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false]);
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    let root = TempDir::new("repl-mesh-probe-on-preview-cap-root");
                    let many: Vec<String> = (0..LIST_PAGE_SIZE + 1)
                        .map(|i| format!("docs/f{i:04}.md"))
                        .collect();
                    let many: Vec<&str> = many.iter().map(String::as_str).collect();
                    seed_files(&root.path, &many);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(
                        &locations.global,
                        "version: 1\nallow:\n- pattern: docs/**\n",
                    );

                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines();
                    let files = index_of(
                        &out,
                        "  files: 1000 or more path(s) are shared with trusted peers (`.mesh shares`)",
                    );
                    assert!(files < index_of(&out, "Mesh stays off"), "{out:?}");
                }

                /// The cap is on what peers get, not on what the allows reach: a page of
                /// denied files sorting ahead of the one shared file leaves the count at 1.
                #[test]
                #[serial]
                fn mesh_on_preview_counts_shared_files_only_when_denied_ones_fill_the_page_first() {
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        ..MeshConfig::default()
                    };
                    let _guard = TestConfigDirGuard::new("repl-mesh-on-preview-denied-first");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false]);
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    let root = TempDir::new("repl-mesh-on-preview-denied-first-root");
                    let mut many: Vec<String> = (0..LIST_PAGE_SIZE + 1)
                        .map(|i| format!("a-denied/f{i:04}.md"))
                        .collect();
                    many.push("z-shared/one.md".to_string());
                    let many: Vec<&str> = many.iter().map(String::as_str).collect();
                    seed_files(&root.path, &many);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(
                        &locations.global,
                        "version: 1\nallow:\n- pattern: '**'\ndeny:\n- pattern: a-denied/**\n",
                    );

                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines();
                    let files = index_of(
                        &out,
                        "  files: 1 path(s) are shared with trusted peers (`.mesh shares`)",
                    );
                    assert!(files < index_of(&out, "Mesh stays off"), "{out:?}");
                }

                /// Usage probe: the match count a user confirms against counts regular files
                /// the pattern reaches under the root — a symlinked directory is not followed
                /// and `.git/` is never entered — so `**` over a tree of links stays small
                /// and asks only because of its head.
                #[cfg(unix)]
                #[test]
                #[serial]
                fn usage_probe_match_count_skips_symlinked_dirs_and_git() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-count-links");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-count-links",
                            &["docs/a.md", ".git/objects/aa", ".git/objects/bb"],
                        )
                        .await;
                        let outside = TempDir::new("repl-mesh-probe-count-links-outside");
                        let many: Vec<String> = (0..150).map(|i| format!("f{i:03}")).collect();
                        let many: Vec<&str> = many.iter().map(String::as_str).collect();
                        seed_files(&outside.path, &many);
                        std::os::unix::fs::symlink(&outside.path, fx.root.path.join("linked"))
                            .unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh allow ** --dry-run")
                            .await
                            .unwrap();
                        assert!(
                            out.contains(&format!(
                                "`**` matches 1 file(s) under {}.",
                                fx.root.path.display()
                            )),
                            "{out}"
                        );
                        let out = out_of(&mut fx.ctx, ".mesh allow linked/** --dry-run")
                            .await
                            .unwrap();
                        assert!(out.contains("matches 0 file(s) under"), "{out}");
                        assert!(out.ends_with(DRY_RUN_NOTHING_CHANGED), "{out}");

                        // The linked tree is not served either.
                        out_of(&mut fx.ctx, ".mesh allow linked/**").await.unwrap();
                        let out = out_of(&mut fx.ctx, ".mesh shares --effective")
                            .await
                            .unwrap();
                        assert!(!out.contains("linked/f"), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: a `**` head anywhere in the first segment is broad (`**/*.md`
                /// asks even over one file); an `unshare` that needs the question fails
                /// without a terminal naming `--yes`, and `--yes` stands in for it; a malformed
                /// or parent-escaping pattern is refused and writes nothing.
                #[test]
                #[serial]
                fn usage_probe_broad_head_variants_non_tty_unshare_and_malformed_patterns() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-broad-variants");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-probe-broad-variants", &["docs/a.md"])
                                .await;

                        let declined = prompt_script::install(&[false]);
                        let out = out_of(&mut fx.ctx, ".mesh allow **/*.md").await.unwrap();
                        assert!(out.contains("matches 1 file(s) under"), "{out}");
                        assert!(out.ends_with(NOTHING_CHANGED), "{out}");
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert!(!fx.locations.global.exists());
                        drop(declined);

                        let quiet = prompt_script::install(&[]);
                        for line in [
                            ".mesh allow docs/[",
                            ".mesh allow docs/../x",
                            ".mesh allow ./docs/**",
                        ] {
                            let err = refusal(&mut fx.ctx, line).await;
                            assert_ne!(err, MESH_OFF, "{line}");
                            assert!(!err.is_empty(), "{line}");
                        }
                        assert!(!fx.locations.global.exists());
                        assert!(!fx.locations.workspace.exists());

                        out_of(&mut fx.ctx, ".mesh allow docs/**").await.unwrap();
                        out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();
                        assert_eq!(fx.entries().len(), 2);
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        drop(quiet);

                        let non_tty = prompt_script::install_non_interactive();
                        let err = refusal(&mut fx.ctx, ".mesh unshare docs/**").await;
                        assert!(err.contains("--yes"), "{err}");
                        assert_eq!(fx.entries().len(), 2);
                        drop(non_tty);

                        let _quiet = prompt_script::install(&[]);
                        let out = out_of(&mut fx.ctx, ".mesh unshare docs/** --yes")
                            .await
                            .unwrap();
                        assert!(out.contains(&fx.global()), "{out}");
                        assert!(out.contains(&fx.workspace()), "{out}");
                        assert!(fx.entries().is_empty());
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe: a flag missing its value is a usage error ahead of the mesh gate.
                #[test]
                #[serial]
                fn usage_probe_peer_flag_without_a_value_is_a_usage_error_before_the_gate() {
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    for line in [".mesh allow docs/** --peer", ".mesh shares --peer"] {
                        let err = err_of(&mut ctx, line);
                        assert_ne!(err, MESH_OFF, "{line}");
                        assert!(err.contains("--peer"), "{line}: {err}");
                    }
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                }

                /// Usage probe (B-22): a share file the node refuses leaves the effective set
                /// empty, so the preview says nothing about shares and still reaches the
                /// consent question.
                #[test]
                #[serial]
                fn usage_probe_on_preview_says_nothing_when_the_share_file_is_refused() {
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        ..MeshConfig::default()
                    };
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-on-preview-refused");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false]);
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    let root = TempDir::new("repl-mesh-probe-on-preview-refused-root");
                    seed_files(&root.path, &["docs/a.md"]);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(&locations.global, "version: 99\n");

                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines();
                    assert!(
                        !out.iter()
                            .any(|line| line.contains("shared with trusted peers")),
                        "{out:?}"
                    );
                    assert!(
                        out.iter().any(|line| line.contains("Mesh stays off")),
                        "{out:?}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 1);
                }

                /// The checks that need the loaded share set (protected head, built-in
                /// deny, the walk behind a broad pattern, the holders behind an unshare)
                /// wait for the mesh gate, and no prompt stands while the mesh is off; the
                /// off context creates no share file either.
                #[test]
                #[serial]
                fn usage_probe_share_set_checks_and_prompts_wait_behind_the_mesh_gate() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-gate-order");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-probe-gate-order-root");
                    seed_files(
                        &root.path,
                        &[".env", ".git/HEAD", "docs/a.md", "src/main.rs"],
                    );
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(&locations.global, "version: 1\nallow:\n- pattern: '**'\n");
                    write_share_file(
                        &locations.workspace,
                        "version: 1\nallow:\n- pattern: '**'\n",
                    );
                    let workspace_before = fs::read(&locations.workspace).unwrap();

                    for line in [
                        ".mesh allow .git/HEAD",
                        ".mesh deny .git/HEAD",
                        ".mesh allow .env",
                        ".mesh allow .env --force --global",
                        ".mesh allow docs/a.md --force --global",
                        ".mesh allow **",
                        ".mesh allow **/*.rs",
                        ".mesh deny **",
                        ".mesh unshare **",
                        ".mesh allow docs/** --workspace",
                        ".mesh allow docs/** --dry-run",
                    ] {
                        assert_eq!(err_of(&mut ctx, line), MESH_OFF, "{line}");
                    }

                    assert_eq!(
                        prompt_script::prompts_asked(),
                        0,
                        "no prompt stands while off"
                    );
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                    assert_eq!(fs::read(&locations.workspace).unwrap(), workspace_before);
                    assert_eq!(
                        ShareSet::load_quietly(locations).0.entries(),
                        [
                            allow_entry(Layer::Global, "**", None),
                            allow_entry(Layer::Workspace, "**", None),
                        ]
                    );
                }

                /// The workspace share list lives in the repository, so a clone chooses
                /// what sits at its name: a link there is refused rather than written
                /// through, and a planted link under the old temp name is never followed
                /// because every write opens a fresh temp name with `create_new`.
                #[test]
                #[serial]
                fn usage_probe_workspace_share_file_link_is_refused_and_a_planted_temp_link_is_never_followed()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-ws-link");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-ws-link",
                            &["docs/a.md", "src/main.rs"],
                        )
                        .await;
                        let elsewhere = TempDir::new("repl-mesh-probe-ws-link-elsewhere");
                        let victim = elsewhere.path.join("victim.yaml");
                        let victim_yaml = "version: 1\nallow:\n- pattern: zzz/**\n";
                        fs::write(&victim, victim_yaml).unwrap();
                        fs::create_dir_all(fx.locations.workspace.parent().unwrap()).unwrap();
                        std::os::unix::fs::symlink(&victim, &fx.locations.workspace).unwrap();

                        let err = refusal(&mut fx.ctx, ".mesh allow docs/** --workspace").await;

                        assert!(err.contains("is a symlink"), "{err}");
                        assert!(
                            !err.contains("written to"),
                            "no write is announced on a refusal: {err}"
                        );
                        assert_eq!(fs::read_to_string(&victim).unwrap(), victim_yaml);
                        assert!(
                            fs::symlink_metadata(&fx.locations.workspace)
                                .unwrap()
                                .file_type()
                                .is_symlink(),
                            "the link is neither replaced nor removed"
                        );
                        assert!(
                            !fx.locations.global.exists(),
                            "nothing fell through to global"
                        );

                        fs::remove_file(&fx.locations.workspace).unwrap();
                        let planted_target = elsewhere.path.join("planted.txt");
                        fs::write(&planted_target, "untouched").unwrap();
                        let planted = fx.locations.workspace.with_added_extension("tmp");
                        std::os::unix::fs::symlink(&planted_target, &planted).unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh allow docs/** --workspace")
                            .await
                            .unwrap();

                        assert!(
                            out.ends_with(&format!(
                                "Allowed `docs/**` for every trusted peer; written to {} (created).",
                                fx.workspace()
                            )),
                            "{out}"
                        );
                        assert_eq!(fs::read_to_string(&planted_target).unwrap(), "untouched");
                        assert!(
                            fs::symlink_metadata(&planted)
                                .unwrap()
                                .file_type()
                                .is_symlink(),
                            "the planted link is left where it was"
                        );
                        assert!(
                            fs::symlink_metadata(&fx.locations.workspace)
                                .unwrap()
                                .file_type()
                                .is_file(),
                            "the workspace file is a regular file"
                        );
                        let leftovers: Vec<String> =
                            fs::read_dir(fx.locations.workspace.parent().unwrap())
                                .unwrap()
                                .map(|entry| {
                                    entry.unwrap().file_name().to_string_lossy().into_owned()
                                })
                                .filter(|name| {
                                    name.ends_with(".tmp") && *name != "mesh-shares.yaml.tmp"
                                })
                                .collect();
                        assert!(
                            leftovers.is_empty(),
                            "no temp file survives the write: {leftovers:?}"
                        );

                        let out = out_of(&mut fx.ctx, ".mesh deny src/**").await.unwrap();
                        assert!(
                            out.ends_with(&format!(
                                "Denied `src/**` to every peer; written to {}.",
                                fx.workspace()
                            )),
                            "a bare deny lands in the workspace file once it exists: {out}"
                        );
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Workspace, "docs/**", None),
                                deny_entry(Layer::Workspace, "src/**"),
                            ]
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Nothing leaves this machine and nothing lands on it before consent: the
                /// off-path `shares`, `shares --effective` and the `.mesh on` preview read
                /// the share root's case folding without writing a probe file, and create
                /// no share file of their own. The root directory's mtime is the oracle: a
                /// probe that creates and removes a file bumps it, a read-only hint does not.
                #[test]
                #[serial]
                fn usage_probe_pre_consent_views_leave_the_share_root_and_share_files_untouched() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-no-probe-file");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[false]);
                    let root = TempDir::new("repl-mesh-probe-no-probe-file-root");
                    seed_files(&root.path, &["docs/a.md", "README.md", "src/lib.rs"]);
                    let config = MeshConfig {
                        interfaces: vec![MeshInterface::Lan],
                        ..MeshConfig::default()
                    };
                    let mut ctx = ctx_with(config, true);
                    ctx.session = Some(Session::default());
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(&locations.global, "version: 1\nallow:\n- pattern: '**'\n");
                    let global_before = fs::read(&locations.global).unwrap();
                    let listing = |dir: &Path| -> Vec<String> {
                        fn walk(dir: &Path, prefix: &str, into: &mut Vec<String>) {
                            for entry in fs::read_dir(dir).unwrap() {
                                let entry = entry.unwrap();
                                let name =
                                    format!("{prefix}{}", entry.file_name().to_string_lossy());
                                if entry.file_type().unwrap().is_dir() {
                                    walk(&entry.path(), &format!("{name}/"), into);
                                } else {
                                    into.push(name);
                                }
                            }
                        }
                        let mut names = Vec::new();
                        walk(dir, "", &mut names);
                        names.sort();
                        names
                    };
                    let before = listing(&root.path);
                    assert_eq!(before, ["README.md", "docs/a.md", "src/lib.rs"]);
                    let pinned = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
                    let dir = fs::File::open(&root.path).unwrap();
                    dir.set_modified(pinned).unwrap();
                    drop(dir);
                    assert_eq!(
                        fs::metadata(&root.path).unwrap().modified().unwrap(),
                        pinned
                    );

                    run_async(run(&mut ctx, ".mesh shares")).unwrap();
                    run_async(run(&mut ctx, ".mesh shares --effective")).unwrap();
                    let out = stdout_lines().join("\n");
                    assert!(out.contains("  src/lib.rs"), "{out}");
                    run_async(run(&mut ctx, ".mesh on")).unwrap();
                    assert!(ctx.app.mesh.get().is_none());
                    let out = stdout_lines().join("\n");
                    assert!(
                        out.lines().any(|line| line
                            == "  files: 3 path(s) are shared with trusted peers (`.mesh shares`)"),
                        "{out}"
                    );
                    assert_eq!(prompt_script::prompts_asked(), 1);

                    assert_eq!(
                        fs::metadata(&root.path).unwrap().modified().unwrap(),
                        pinned,
                        "nothing was created or removed under the share root before consent"
                    );
                    assert_eq!(
                        listing(&root.path),
                        before,
                        "no probe file under the share root"
                    );
                    assert!(
                        !locations.workspace.exists(),
                        "inspection creates no workspace file"
                    );
                    assert_eq!(fs::read(&locations.global).unwrap(), global_before);
                }

                /// A rule already held prints its sentence and asks nothing even when the
                /// pattern is broad; `--force` on a file whose allow is held but whose
                /// override is not completes the pair once, and the built-in deny names
                /// the file by text, so the file need not exist on disk.
                #[test]
                #[serial]
                fn usage_probe_an_already_held_broad_rule_asks_nothing_and_force_completes_a_half_held_pair()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-already-broad");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-already-broad",
                            &["docs/a.md", "docs/b.md", "src/main.rs"],
                        )
                        .await;
                        fx.write_global(
                            "version: 1\nallow:\n- pattern: '**'\n- pattern: .env\ndeny:\n- pattern: '**/*.rs'\n",
                        );
                        let before = fs::read(&fx.locations.global).unwrap();

                        let out = out_of(&mut fx.ctx, ".mesh allow **").await.unwrap();
                        // The match-count line precedes the sentence (the in-tree test
                        // accepts it with `ends_with`); what the criterion forbids is an
                        // announced write and a prompt.
                        assert_eq!(
                            out.lines().last().unwrap(),
                            &format!(
                                "`**` is already allowed for every trusted peer in {}; nothing was changed.",
                                fx.global()
                            ),
                            "{out}"
                        );
                        assert!(
                            !out.contains("rite to") && !out.contains("Allowed `"),
                            "no write is announced for a held rule: {out}"
                        );
                        let out = out_of(&mut fx.ctx, ".mesh deny **/*.rs").await.unwrap();
                        assert_eq!(
                            out.lines().last().unwrap(),
                            &format!(
                                "`**/*.rs` is already denied to every peer in {}; nothing was changed.",
                                fx.global()
                            )
                        );
                        assert!(
                            !out.contains("rite to") && !out.contains("Denied `"),
                            "no write is announced for a held rule: {out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        assert_eq!(fs::read(&fx.locations.global).unwrap(), before);

                        let out = out_of(&mut fx.ctx, ".mesh allow .env --force --global")
                            .await
                            .unwrap();
                        assert!(
                            out.contains(&format!("written to {}", fx.global())),
                            "the missing override is written: {out}"
                        );
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Global, "**", None),
                                allow_entry(Layer::Global, ".env", None),
                                deny_entry(Layer::Global, "**/*.rs"),
                                override_entry(Layer::Global, ".env"),
                            ],
                            "the allow is not duplicated; the override joins it"
                        );

                        let out = out_of(&mut fx.ctx, ".mesh allow .env --force --global")
                            .await
                            .unwrap();
                        assert_eq!(
                            out.lines().last().unwrap(),
                            &format!(
                                "`.env` is already allowed for every trusted peer in {}; nothing was changed.",
                                fx.global()
                            ),
                            "a held allow+override pair is a no-op"
                        );
                        assert!(
                            !out.contains("rite to") && !out.contains("Allowed `"),
                            "no write is announced for a held pair: {out}"
                        );

                        let out = out_of(&mut fx.ctx, ".mesh allow id_rsa --force --global")
                            .await
                            .unwrap();
                        assert!(
                            out.contains(&format!("written to {}", fx.global())),
                            "the built-in deny names `id_rsa` by text; it need not exist: {out}"
                        );
                        assert!(
                            fx.entries()
                                .contains(&override_entry(Layer::Global, "id_rsa"))
                        );
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        fx.stop().await;
                    });
                }

                /// Usage probe (d): `deny` keeps the layer it announced like `allow` does,
                /// and a concurrent edit to the announced file made while the question
                /// stands survives the write.
                #[test]
                #[serial]
                fn usage_probe_deny_writes_the_layer_it_announced_and_keeps_a_concurrent_edit() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-deny-announced");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-probe-deny-announced", &["docs/a.md"])
                                .await;
                        let locations = fx.locations.clone();
                        let _script = prompt_script::install_answering(move |question| {
                            assert!(question.contains("**"), "{question}");
                            // Auto would now resolve to the workspace layer...
                            write_share_file(
                                &locations.workspace,
                                "version: 1\nallow:\n- pattern: docs/**\n",
                            );
                            // ...and the announced (global) file gained a rule meanwhile.
                            ShareSet::load_quietly(locations.clone())
                                .0
                                .apply(
                                    Mutation::Deny {
                                        pattern: "secrets/**".to_string(),
                                    },
                                    WriteScope::Global,
                                )
                                .unwrap();
                            true
                        });

                        let out = out_of(&mut fx.ctx, ".mesh deny **").await.unwrap();

                        assert!(
                            out.contains(&format!(
                                "Will write to {}: deny `**` to every peer.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert!(
                            out.ends_with(&format!(
                                "Denied `**` to every peer; written to {}.",
                                fx.global()
                            )),
                            "{out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(
                            fx.entries(),
                            [
                                deny_entry(Layer::Global, "secrets/**"),
                                deny_entry(Layer::Global, "**"),
                                allow_entry(Layer::Workspace, "docs/**", None),
                            ],
                            "the concurrent deny survives, the new deny lands in the announced file, the workspace file is untouched"
                        );
                        fx.stop().await;
                    });
                }

                /// Usage probe (d): the announced layer is kept in the other direction too —
                /// a workspace file that vanishes while the question stands does not pull
                /// the write back into the global file.
                #[test]
                #[serial]
                fn usage_probe_allow_keeps_the_announced_workspace_layer_when_its_file_vanishes_during_the_prompt()
                 {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-allow-ws-vanishes");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-probe-allow-ws-vanishes", &["docs/a.md"])
                                .await;
                        fx.write_workspace("version: 1\nallow:\n- pattern: notes/**\n");
                        let workspace = fx.locations.workspace.clone();
                        let _script = prompt_script::install_answering(move |_question| {
                            fs::remove_file(&workspace).unwrap();
                            true
                        });

                        let out = out_of(&mut fx.ctx, ".mesh allow **").await.unwrap();

                        assert!(
                            out.contains(&format!("Will write to {}:", fx.workspace())),
                            "{out}"
                        );
                        let last = out.lines().last().unwrap_or_default();
                        assert!(
                            last.starts_with("Allowed `**` for every trusted peer; written to ")
                                && last.contains(&fx.workspace()),
                            "{out}"
                        );
                        assert!(
                            !last.contains(&fx.global()),
                            "never the layer re-resolved after the prompt: {out}"
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert!(!fx.locations.global.exists(), "global file untouched");
                        assert_eq!(
                            fx.entries(),
                            [allow_entry(Layer::Workspace, "**", None)],
                            "the vanished workspace file is re-created with the announced rule (the concurrent removal of notes/** is not undone)"
                        );
                        fx.stop().await;
                    });
                }

                /// Usage probe (d): "holders changed" covers a holder that APPEARED in the
                /// other layer while the question stood, not only one that went away — the
                /// human confirmed removing it from one file, not from two.
                #[test]
                #[serial]
                fn usage_probe_unshare_refuses_when_a_second_holder_appears_during_the_prompt() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-unshare-holder-appears");
                    let _capture = capture::install();
                    run_async(async {
                        let mut fx = ShareFixture::new(
                            "repl-mesh-probe-unshare-holder-appears",
                            &["docs/a.md"],
                        )
                        .await;
                        fx.write_global("version: 1\nallow:\n- pattern: '**'\n");
                        let workspace = fx.locations.workspace.clone();
                        let _script = prompt_script::install_answering(move |_question| {
                            write_share_file(&workspace, "version: 1\ndeny:\n- pattern: '**'\n");
                            true
                        });

                        let err = refusal(&mut fx.ctx, ".mesh unshare **").await;

                        assert_eq!(
                            err,
                            "The share list changed while the prompt was open; run `.mesh unshare` again."
                        );
                        assert_eq!(prompt_script::prompts_asked(), 1);
                        assert_eq!(
                            fx.entries(),
                            [
                                allow_entry(Layer::Global, "**", None),
                                deny_entry(Layer::Workspace, "**"),
                            ],
                            "nothing was removed from either file"
                        );
                        fx.stop().await;
                    });
                }

                /// Usage probe (a)+(c): handing `allow` a destination hash is a pure
                /// classification error — it fires with the mesh OFF (before `MESH_OFF`),
                /// with `--peer` alongside, and writes nothing in either case.
                #[test]
                #[serial]
                fn usage_probe_allow_of_a_hash_teaches_trust_before_the_mesh_gate_and_with_peer() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-allow-hash-off");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-probe-allow-hash-off-root");
                    seed_files(&root.path, &["docs/a.md"]);
                    publish_root(&ctx, &root.path);
                    let dest = "3f9c2a7b1d4e6f80a1b2c3d4e5f60718";
                    let other = "ab".repeat(16);

                    let err = err_of(&mut ctx, &format!(".mesh allow {dest}"));
                    assert_ne!(err, MESH_OFF, "a pure check fires before the mesh gate");
                    assert!(err.contains(&format!(".mesh trust {dest}")), "{err}");
                    assert!(err.contains(&format!("--peer {dest}")), "{err}");

                    let err = err_of(&mut ctx, &format!(".mesh allow {dest} --peer {other}"));
                    assert_ne!(err, MESH_OFF, "{err}");
                    assert!(
                        err.contains(&format!(".mesh trust {dest}")),
                        "the positional is still a hash, whatever follows: {err}"
                    );

                    let err = err_of(&mut ctx, &format!(".mesh allow {dest} --dry-run"));
                    assert_ne!(err, MESH_OFF, "{err}");
                    assert!(err.contains(&format!(".mesh trust {dest}")), "{err}");

                    assert_eq!(prompt_script::prompts_asked(), 0);
                    assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                    let (_, locations) = share_locations(&ctx).unwrap();
                    assert!(!locations.global.exists());
                    assert!(!locations.workspace.exists());

                    // The same hash is a fine `--peer` VALUE: with the mesh off the verb then
                    // reaches the gate, not the hash teaching.
                    let err = err_of(&mut ctx, &format!(".mesh allow docs/** --peer {dest}"));
                    assert_eq!(err, MESH_OFF);
                }

                /// Usage probe (g)+(b): `undeny` stays unknown and the three mutating verbs
                /// print usage (never `MESH_OFF`, never a write) when bare while OFF.
                #[test]
                #[serial]
                fn usage_probe_undeny_is_unknown_and_bare_mutators_print_usage_while_off() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-probe-undeny-off");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-probe-undeny-off-root");
                    seed_files(&root.path, &["docs/a.md"]);
                    publish_root(&ctx, &root.path);

                    let err = err_of(&mut ctx, ".mesh undeny docs/**");
                    assert_ne!(err, MESH_OFF, "{err}");
                    assert!(err.contains("undeny"), "{err}");

                    for verb in ["allow", "deny", "unshare"] {
                        run_async(run(&mut ctx, &format!(".mesh {verb}"))).unwrap();
                    }
                    let out = stdout_lines().join("\n");
                    for verb in ["allow", "deny", "unshare"] {
                        assert!(out.contains(&render_verb_help(verb)), "{verb}: {out}");
                    }
                    assert!(!out.contains(MESH_OFF), "{out}");
                    assert!(stderr_lines().is_empty(), "{:?}", stderr_lines());
                    assert_eq!(prompt_script::prompts_asked(), 0);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    assert!(!locations.global.exists());
                    assert!(!locations.workspace.exists());
                }

                /// A root the node could not probe serves nothing, so the verbs say that
                /// rather than guess how its rules fold and describe shares that do not
                /// exist. Root writes anywhere, so under root there is nothing to show.
                #[test]
                #[serial]
                fn allow_and_shares_effective_refuse_a_root_the_node_could_not_probe() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-unprobeable-root");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    run_async(async {
                        let mut fx =
                            ShareFixture::new("repl-mesh-unprobeable-root", &["docs/a.md"]).await;
                        let locked = ModeRestore::read_only(&fx.root.path);
                        if fs::File::create(fx.root.path.join("probe-write-check")).is_ok() {
                            locked.restore();
                            fx.stop().await;
                            return;
                        }
                        let sentence = format!(
                            "The share root {} could not be probed, so this node serves nothing from it; fix the directory's permissions and run `.mesh off` then `.mesh on`.",
                            fx.root.path.display()
                        );

                        let err = refusal(&mut fx.ctx, ".mesh allow docs/**").await;
                        assert_eq!(err, sentence);
                        assert!(!fx.locations.global.exists());
                        assert!(!fx.locations.workspace.exists());

                        fx.write_global("version: 1\nallow:\n- pattern: docs/**\n");
                        let err = refusal(&mut fx.ctx, ".mesh shares --effective").await;
                        assert_eq!(err, sentence);

                        assert!(stdout_lines().is_empty(), "{:?}", stdout_lines());
                        assert_eq!(prompt_script::prompts_asked(), 0);
                        locked.restore();
                        fx.stop().await;
                    });
                }

                /// While the mesh is off the hint is all there is, and a root with no
                /// ASCII letter in any name gives none; the view says it judged the
                /// patterns case-sensitively rather than passing the guess off as fact.
                #[test]
                #[serial]
                fn shares_effective_while_off_says_when_the_case_hint_could_not_be_read() {
                    let _guard = TestConfigDirGuard::new("repl-mesh-shares-off-no-hint");
                    let _capture = capture::install();
                    let _script = prompt_script::install(&[]);
                    let mut ctx = off_ctx();
                    let root = TempDir::new("repl-mesh-shares-off-no-hint-root");
                    seed_files(&root.path, &["1/2.md"]);
                    publish_root(&ctx, &root.path);
                    let (_, locations) = share_locations(&ctx).unwrap();
                    write_share_file(&locations.global, "version: 1\nallow:\n- pattern: '**'\n");

                    run_async(run(&mut ctx, ".mesh shares --effective")).unwrap();

                    let out = stdout_lines().join("\n");
                    assert_eq!(
                        out.lines().collect::<Vec<_>>(),
                        [
                            "The share root's case folding could not be read; patterns were matched case-sensitively.",
                            format!(
                                "Files every trusted peer can fetch from {}:",
                                root.path.display()
                            )
                            .as_str(),
                            "  1/2.md",
                        ]
                    );
                }
            }
        }
    }
}
