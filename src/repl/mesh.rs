use crate::config::mesh_config::{MeshBrief, MeshInterface, render_mesh_info};
use crate::config::{MeshConfig, RequestContext};
use crate::function::mesh::trust_label;
use crate::mesh::card::{CardSource, DISPLAY_NAME_MAX_CHARS, StatusHandler, render_for_human};
use crate::mesh::knocks::KnockRecord;
use crate::mesh::message::{
    BroadcastOutcome, OutboundPeer, PEER_CONTENT_MAX_CHARS, PeerKind, PeerMessage, PeerVia,
    RecipientOutcome,
};
use crate::mesh::pending::{Correlation, InboundRecord, PendingState};
use crate::mesh::trust::{Decision, Rule, Tier, TrustRecord, Verdict};
use crate::mesh::{
    MESH_ALREADY_ON, MeshPaths, MeshRuntime, NodeOptions, PeerRecord, PropagationNodeRecord,
    age_text, canonical_hash, display_text, parse_rfc3339, short,
};
use crate::supervisor::mailbox::EnvelopePayload;
use crate::utils::{AbortSignal, drain_stale_tty_input, wait_user_interrupt};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use inquire::Confirm;
use log::debug;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

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
];

pub(crate) const MESH_OFF: &str = "Mesh is off. Run `.mesh on` first.";
const BROADCAST_NOTICE: &str = "This sends a bulletin to every peer this node trusts that has a known path right now. Peers you have not trusted receive nothing.";
const REPLY_REFUSAL_TAIL: &str = "Nothing is sent to a destination this node does not trust.";
const STATUS_REFUSAL_TAIL: &str = "Status is only requested from trusted destinations.";
const INBOX_CONTENT_MAX_CHARS: usize = 200;

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
        other => bail!("Unknown .mesh command '{other}'. Type `.mesh` for the list."),
    }
}

async fn turn_on(ctx: &mut RequestContext, rest: Option<&str>) -> Result<()> {
    let args = parse_args(rest, &["--yes", "--fresh"], "on")?;
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
    let fresh = args.has("--fresh");
    out_text(&render_on_preview(&config, &session_name, fresh));
    if let Some(warning) = cwd_warning(&cwd, dirs::home_dir().as_deref()) {
        err_text(&warning);
    }
    if !confirm_or_flag(&on_question(&config), "--yes", args.has("--yes"))? {
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
        NodeOptions::default(),
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
    let mut rows: Vec<PeerRow> = records
        .into_iter()
        .map(|peer| {
            let label = trust_label(trust.authorize(&peer.identity_hash, &peer.destination_hash));
            PeerRow::Heard(peer, label)
        })
        .collect();
    rows.extend(deny_only_rows(trust.records(), &heard));
    out_text(&render_peers(&rows, now));
    Ok(())
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
            "  {:<28}{}\n",
            "reach",
            reach_line(&ctx.app.config.mesh)
        ));
        match ctx.app.mesh.get() {
            Some(runtime) => text.push_str(&render_node_facts(&runtime, SystemTime::now())),
            None => text.push_str("  node                        off\n"),
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
            debug!("knock cache unreadable while describing a known peer: {err:#}");
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
            out_text(&sending_notice(&record.peer_destination));
            ctx.app.mesh.answer_inbound(id, text).await?;
            out_text(&format!("Answered {}.", short(id)));
        }
        (AnswerRoute::Outbound, _, Some(correlation)) => {
            let destination = &correlation.record.peer_destination;
            out_text(&sending_notice(destination));
            let out = OutboundPeer::new(PeerKind::Reply, text, None, Some(id), None)?;
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
        let verdict = runtime.trust().authorize("", destination);
        if verdict.rule == Rule::DestinationDenied
            && let Some(refusal) = trust_refusal(destination, verdict, only)
        {
            bail!(refusal);
        }
        bail!(
            "Destination {destination} has not been heard from: it is not in the peer table. Only peers this node has heard announce can be contacted; check `.mesh peers`."
        );
    };
    let verdict = runtime.trust().authorize(&peer.identity_hash, destination);
    if let Some(refusal) = trust_refusal(destination, verdict, only) {
        bail!(refusal);
    }
    Ok(peer)
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
    //! terminal state from the guard and answers each prompt from a queue, counting every
    //! prompt asked. The script is process-global: tests using it must be `#[serial]`.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const INACTIVE: usize = 0;
    const TTY: usize = 1;
    const NON_TTY: usize = 2;

    static STATE: AtomicUsize = AtomicUsize::new(INACTIVE);
    static ASKED: AtomicUsize = AtomicUsize::new(0);
    static ANSWERS: Mutex<Vec<bool>> = Mutex::new(Vec::new());

    /// Forces a terminal on stdin and answers the prompts from `answers`, front to back;
    /// a prompt beyond the scripted answers panics.
    #[must_use]
    pub fn install(answers: &[bool]) -> ScriptGuard {
        install_with_state(TTY, answers)
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
        "Mesh commands (each is session-scoped; nothing here writes config.yaml):".to_string(),
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

/// What `.mesh on` prints before anything leaves the machine.
fn render_on_preview(config: &MeshConfig, session_name: &str, fresh: bool) -> String {
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

/// One `.mesh peers` line: a node heard on the mesh with its trust label, or a denied
/// destination nothing has announced from yet, listed so the deny is visible.
enum PeerRow {
    Heard(PeerRecord, &'static str),
    DenyOnly(TrustRecord),
}

fn deny_only_rows(records: Vec<TrustRecord>, heard: &[String]) -> Vec<PeerRow> {
    records
        .into_iter()
        .filter(|record| {
            record.denied && record.tier == Tier::Destination && !heard.contains(&record.hash)
        })
        .map(PeerRow::DenyOnly)
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
            PeerRow::Heard(peer, trust) => {
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
            }
            PeerRow::DenyOnly(record) => lines.push(format!(
                "{:<20} {:<10} {:<10} {:<10} {:>4}  {}",
                "-",
                short(&record.hash),
                record.identity.as_deref().map(short).unwrap_or("-"),
                "denied",
                "-",
                "never"
            )),
        }
    }
    lines.push(format!(
        "{} peer(s). Names are peer-supplied text. Full hashes: `.mesh info <dest>`.",
        rows.len()
    ));
    lines.join("\n")
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

fn render_node_facts(runtime: &MeshRuntime, now: SystemTime) -> String {
    let mut output = String::new();
    let mut row = |name: &str, value: String| output.push_str(&format!("  {name:<28}{value}\n"));
    row("node", "on".to_string());
    row("identity", runtime.fingerprint().to_string());
    row("destination", runtime.current_destination_hash());
    row("instance", runtime.current_instance_id());
    row("joined", runtime.interfaces().join(", "));
    output.push_str(&render_propagation_nodes(
        runtime.propagation_nodes().snapshot(),
        now,
    ));
    output
}

/// Nearest first, the most recently heard breaking ties, so the row the node would
/// pick for store-and-forward is the top one.
fn render_propagation_nodes(mut nodes: Vec<PropagationNodeRecord>, now: SystemTime) -> String {
    nodes.sort_by_key(|node| (node.hops, std::cmp::Reverse(node.last_seen)));
    let mut output = String::new();
    if nodes.is_empty() {
        output.push_str("  propagation_nodes           none heard yet\n");
    }
    for (i, record) in nodes.iter().enumerate() {
        let name = format!("propagation_nodes[{i}]");
        output.push_str(&format!(
            "  {name:<28}{} ({} hop(s), {})\n",
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
    if escalated.is_empty() {
        lines.push("  none".to_string());
    }
    for record in escalated {
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
    use crate::mesh::message::{RawPeerMessage, RecipientReport};
    use crate::mesh::pending::{INBOUND_RECORD_VERSION, PENDING_RECORD_VERSION, PendingRecord};
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
            version: 1,
            received_at: received_at.to_string(),
            identity_hash: "ef".repeat(16),
            destination_hash: "12".repeat(16),
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
        let text = render_on_preview(&config, "work", false);
        assert!(text.contains("session 'work'"), "{text}");
        assert!(text.contains("config.yaml is not changed"), "{text}");
        assert!(text.contains("announce:"), "{text}");
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
        let text = render_on_preview(&quiet, "work", true);
        assert!(text.contains("announce: nothing until"), "{text}");
        assert!(text.contains("display name: none"), "{text}");
        assert!(
            text.contains("fresh id: this session gets a new mesh id and destination"),
            "{text}"
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
            PeerRow::Heard(peer(Some("Ann"), 5, now), "trusted"),
            PeerRow::Heard(old, "denied"),
            PeerRow::Heard(peer(None, 30, now), "untrusted"),
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
        let mut rows = vec![PeerRow::Heard(heard.clone(), "trusted")];
        rows.extend(deny_only_rows(records, &heard_hashes));
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

    fn correlation(id: &str, state: PendingState) -> Correlation {
        Correlation {
            record: PendingRecord {
                version: PENDING_RECORD_VERSION,
                id: id.to_string(),
                peer_destination: "ab".repeat(16),
                peer_identity: "cd".repeat(16),
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
            question: "may I read the plan?".to_string(),
            envoy_question: envoy_question.to_string(),
            received_at: "2026-09-21T14:13:20Z".to_string(),
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
            PeerRow::Heard(old, "denied"),
            PeerRow::Heard(peer(Some("Bad"), 5, now), "blocked"),
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
        fn bare_mesh_and_verb_help_never_error() {
            let _capture = capture::install();
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
            ] {
                run_async(run(&mut ctx, line)).unwrap_or_else(|err| panic!("{line}: {err}"));
            }
            let out = stdout_lines().join("\n");
            for example in [
                ".mesh answer <id>",
                ".mesh reply <destination>",
                ".mesh broadcast",
            ] {
                assert!(out.contains(example), "{example} missing from {out}");
            }
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
                PeerSighting, loopback_relay, private_config, started_runtime,
            };
            use crate::mesh::trust::{LiveMesh, TrustOptions};
            use crate::testing::EnvVarGuard;
            use crate::utils::get_env_name;
            use parking_lot::Mutex;
            use std::sync::atomic::{AtomicUsize, Ordering};

            /// Puts a heard, trusted peer named "Tia" in `runtime`'s peer table and returns
            /// its destination hash. Trusting verifies identity + name hash -> destination,
            /// so the destination is derived for real.
            fn heard_trusted_peer(runtime: &MeshRuntime, slot: &dyn LiveMesh) -> String {
                use rand_core::OsRng;
                use rns_transport::destination::{DestinationName, SingleInputDestination};
                use rns_transport::identity::PrivateIdentity;

                let now = SystemTime::now();
                let name = DestinationName::new("coyote", "mesh.probe");
                let announced =
                    SingleInputDestination::new(PrivateIdentity::new_from_rand(OsRng), name);
                let hash = announced.desc.address_hash.to_hex_string();
                runtime.peers().observe(
                    PeerSighting {
                        destination_hash: hash.clone(),
                        identity_hash: announced.desc.identity.address_hash.to_hex_string(),
                        name_hash: hex_lower(name.as_name_hash_slice()),
                        display_name: Some("Tia".to_string()),
                        protocol_version: 1,
                        hops: 1,
                    },
                    now,
                );
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
                        let head = format!("  {name:<28}");
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
                use crate::mesh::pending::{INBOUND_RECORD_VERSION, PENDING_RECORD_VERSION};
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
                                question: "may I read the plan?".to_string(),
                                envoy_question: String::new(),
                                received_at: rfc3339_utc(now),
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
                use crate::mesh::test_support::PeerSighting;
                use crate::mesh::trust::{LiveMesh, TrustOptions};
                use rand_core::OsRng;
                use rns_transport::destination::{DestinationName, SingleInputDestination};
                use rns_transport::identity::PrivateIdentity;

                let _guard = TestConfigDirGuard::new("repl-mesh-status-unresolvable");
                let _capture = capture::install();
                run_async(async {
                    let started = started_runtime("repl-mesh-status-unresolvable").await;
                    let mut ctx = ctx_with(MeshConfig::default(), true);
                    ctx.app.mesh.install(started.runtime.clone()).unwrap();
                    let now = SystemTime::now();
                    let name = DestinationName::new("coyote", "mesh.probe");
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
                use crate::mesh::test_support::PeerSighting;
                use crate::mesh::trust::{LiveMesh, TrustOptions};
                use rand_core::OsRng;
                use rns_transport::destination::{DestinationName, SingleInputDestination};
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
                    let name = DestinationName::new("coyote", "mesh.probe");
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
        }
    }
}
