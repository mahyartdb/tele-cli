use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use crate::client::{self, ClientGuard};
use crate::error::{TeleError, TeleResult};
use crate::executor::{require_explicit_selection, GlobalFlags};
use crate::output;
use clap::Args;
use grammers_client::tl::{self, Serializable};
use grammers_session::types::{PeerId, PeerKind};
use grammers_session::updates::State;
#[derive(Args)]
pub struct ListenArgs {
    #[arg(
        long = "timeout-secs",
        default_value_t = 0,
        help = "max listen duration in seconds (0 = forever)"
    )]
    timeout_secs: u64,
    #[arg(
        long = "count",
        value_name = "N",
        help = "exit after emitting N event rows (all accounts combined)"
    )]
    count: Option<u64>,
    #[arg(
        long = "until",
        value_name = "RFC3339|unix-ts|YYYY-MM-DD",
        help = "exit after the first event at/after this timestamp"
    )]
    until: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "NewMessage",
        help = "event types: NewMessage, MessageEdited, MessageDeleted, Raw, Album, Gap, Service, ChatAction, UserUpdate, CallbackQuery (ChatAction/UserUpdate/CallbackQuery are parsed out of grammers' raw-update wrapper)"
    )]
    events: Vec<String>,
    #[arg(
        long,
        help = "also emit raw TL updates alongside the parsed event allowlist"
    )]
    raw: bool,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "CHAT",
        help = "only show events from these chats (repeatable or comma-separated)"
    )]
    chat: Vec<String>,
    #[arg(
        long,
        value_name = "USER",
        help = "only show events sent by these users (repeatable)"
    )]
    from: Vec<String>,
    #[arg(
        long,
        conflicts_with = "out",
        help = "only incoming messages (message events only)"
    )]
    r#in: bool,
    #[arg(long, help = "only outgoing messages (message events only)")]
    out: bool,
    #[arg(
        long,
        value_name = "RE",
        action = clap::ArgAction::Append,
        help = "only messages whose text matches one of these Rust regexes, case-sensitive (repeatable; message events only)"
    )]
    pattern: Vec<String>,
}
const VALID_EVENTS: &[&str] = &[
    "NewMessage",
    "MessageEdited",
    "MessageDeleted",
    "Raw",
    "Album",
    "Gap",
    "Service",
    "ChatAction",
    "UserUpdate",
    "CallbackQuery",
];
const MAX_RECONNECT_BACKOFF: u32 = 30;
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const ALBUM_FLUSH_MILLIS: u64 = 500;
const MAX_ALBUM_LEN: usize = 20;
const ALBUM_BUFFER_CAP: usize = 20;
const OBSERVED_PEER_CAP: usize = 10_000;
const GAP_TRACKER_CAP: usize = 10_000;
const LISTEN_OUTPUT_CAPACITY: usize = 256;
/// grammers hands the client an unbounded update channel: an account task
/// parked forever on a stalled stdout would let it grow without limit. Past
/// this budget the row is dropped and counted instead.
const LISTEN_STALL_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
const LISTEN_FLUSH_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, Default)]
struct EventFilter {
    chats: Vec<PeerId>,
    senders: Vec<PeerId>,
    direction: Option<Direction>,
    patterns: Vec<regex::Regex>,
}

impl EventFilter {
    fn chat_allows(&self, peer: Option<PeerId>) -> bool {
        match self.chats.as_slice() {
            [] => true,
            targets => peer.is_some_and(|p| targets.contains(&p)),
        }
    }

    fn message_allows(&self, sender: Option<PeerId>, out: Option<bool>) -> bool {
        if !self.sender_allows(sender) {
            return false;
        }
        self.direction_allows(out)
    }

    fn text_allows(&self, text: Option<&str>) -> bool {
        match self.patterns.as_slice() {
            [] => true,
            patterns => text.is_some_and(|t| patterns.iter().any(|re| re.is_match(t))),
        }
    }

    fn sender_allows(&self, sender: Option<PeerId>) -> bool {
        match self.senders.as_slice() {
            [] => true,
            targets => sender.is_some_and(|s| targets.contains(&s)),
        }
    }

    fn direction_allows(&self, out: Option<bool>) -> bool {
        match self.direction {
            None => true,
            Some(Direction::In) => out == Some(false),
            Some(Direction::Out) => out == Some(true),
        }
    }

    fn raw_allows(&self, peer: Option<PeerId>) -> bool {
        self.senders.is_empty()
            && self.direction.is_none()
            && self.patterns.is_empty()
            && self.chat_allows(peer)
    }

    fn action_allows(&self, peer: Option<PeerId>, sender: Option<PeerId>) -> bool {
        // Consistency with raw/deletion rows: direction and pattern filters
        // structurally cannot apply to chat-action-family rows (no text, no
        // direction), so when they are active the rows are suppressed instead
        // of silently bypassing the filter. Sender/chat filters do apply.
        self.direction.is_none()
            && self.patterns.is_empty()
            && self.chat_allows(peer)
            && self.sender_allows(sender)
    }

    fn deletions_pass(&self) -> bool {
        self.senders.is_empty() && self.direction.is_none() && self.patterns.is_empty()
    }
}

fn deletion_match_set(
    messages: &[i32],
    channel_id: Option<i64>,
    observed: &ObservedPeers,
    targets: &[PeerId],
) -> Option<Vec<i32>> {
    if targets.is_empty() {
        return Some(messages.to_vec());
    }
    let mut matched: Vec<i32> = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();
    for target in targets {
        let hits: Vec<i32> = if target.kind() == PeerKind::Channel {
            if deleted_matches(channel_id, *target) {
                messages.to_vec()
            } else {
                Vec::new()
            }
        } else {
            observed_deletion_ids(messages, observed, *target)
        };
        for id in hits {
            if seen.insert(id) {
                matched.push(id);
            }
        }
    }
    (!matched.is_empty()).then_some(matched)
}

fn sole_chat_label(targets: &[PeerId]) -> Option<i64> {
    match targets {
        [only] => only.bare_id(),
        _ => None,
    }
}

fn message_sender(msg: &tl::enums::Message) -> Option<PeerId> {
    match msg {
        tl::enums::Message::Message(m) => m.from_id.as_ref().map(PeerId::from),
        tl::enums::Message::Service(m) => m.from_id.as_ref().map(PeerId::from),
        tl::enums::Message::Empty(_) => None,
    }
}

fn message_outgoing(msg: &tl::enums::Message) -> Option<bool> {
    match msg {
        tl::enums::Message::Message(m) => Some(m.out),
        tl::enums::Message::Service(m) => Some(m.out),
        tl::enums::Message::Empty(_) => None,
    }
}

fn update_sender(u: &tl::enums::Update) -> Option<PeerId> {
    match u {
        tl::enums::Update::NewMessage(x) => message_sender(&x.message),
        tl::enums::Update::NewChannelMessage(x) => message_sender(&x.message),
        tl::enums::Update::EditMessage(x) => message_sender(&x.message),
        tl::enums::Update::EditChannelMessage(x) => message_sender(&x.message),
        _ => None,
    }
}

fn update_outgoing(u: &tl::enums::Update) -> Option<bool> {
    match u {
        tl::enums::Update::NewMessage(x) => message_outgoing(&x.message),
        tl::enums::Update::NewChannelMessage(x) => message_outgoing(&x.message),
        tl::enums::Update::EditMessage(x) => message_outgoing(&x.message),
        tl::enums::Update::EditChannelMessage(x) => message_outgoing(&x.message),
        _ => None,
    }
}

fn message_text(msg: &tl::enums::Message) -> Option<&str> {
    match msg {
        tl::enums::Message::Message(m) => Some(m.message.as_str()),
        _ => None,
    }
}

fn update_text(u: &tl::enums::Update) -> Option<&str> {
    match u {
        tl::enums::Update::NewMessage(x) => message_text(&x.message),
        tl::enums::Update::NewChannelMessage(x) => message_text(&x.message),
        tl::enums::Update::EditMessage(x) => message_text(&x.message),
        tl::enums::Update::EditChannelMessage(x) => message_text(&x.message),
        _ => None,
    }
}

fn compile_pattern(patterns: &[String]) -> TeleResult<Vec<regex::Regex>> {
    patterns
        .iter()
        .map(|p| {
            regex::Regex::new(p)
                .map_err(|e| TeleError::Usage(format!("invalid --pattern {p:?}: {e}")))
        })
        .collect()
}

fn resolution_usage_error(flag: &str, target: &str, cause: &TeleError) -> TeleError {
    TeleError::Usage(format!(
        "cannot resolve {flag} {target}: {}",
        cause.message()
    ))
}

fn validate_listen_inputs(chats: &[String], senders: &[String]) -> TeleResult<()> {
    for target in chats {
        if target.trim().is_empty() {
            return Err(TeleError::Usage(format!(
                "empty --chat/--from target: {target:?}"
            )));
        }
        crate::chat_target::ChatTarget::parse_flag(target, "chat")?;
    }
    for target in senders {
        if target.trim().is_empty() {
            return Err(TeleError::Usage(format!(
                "empty --chat/--from target: {target:?}"
            )));
        }
        crate::chat_target::ChatTarget::parse_flag(target, "from")?;
    }
    Ok(())
}

async fn resolve_filter(
    client: &grammers_client::Client,
    session: &grammers_session::storages::SqliteSession,
    chats: &[String],
    senders: &[String],
    direction: Option<Direction>,
) -> TeleResult<EventFilter> {
    let mut resolved_chats = Vec::with_capacity(chats.len());
    for target in chats {
        let peer = crate::entities::resolve_peer(client, session, target)
            .await
            .map_err(|e| resolution_usage_error("--chat", target, &e))?;
        resolved_chats.push(peer.id());
    }
    let mut resolved_senders = Vec::with_capacity(senders.len());
    for target in senders {
        let peer = crate::entities::resolve_peer(client, session, target)
            .await
            .map_err(|e| resolution_usage_error("--from", target, &e))?;
        resolved_senders.push(peer.id());
    }
    Ok(EventFilter {
        chats: resolved_chats,
        senders: resolved_senders,
        direction,
        patterns: Vec::new(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StateBoxKey {
    Common,
    Channel(i64),
}

struct PtsPoint {
    box_key: StateBoxKey,
    pts: i32,
    pts_count: i32,
}

struct GapSignal {
    box_key: StateBoxKey,
    expected_pts: i32,
    observed_pts: i32,
}

struct GapTracker {
    last: crate::capped_map::CappedMap<StateBoxKey, (i32, i32)>,
}

impl Default for GapTracker {
    fn default() -> Self {
        GapTracker {
            last: crate::capped_map::CappedMap::new(GAP_TRACKER_CAP),
        }
    }
}

impl GapTracker {
    fn observe(&mut self, raw: &tl::enums::Update) -> Option<GapSignal> {
        let point = pts_point(raw)?;
        match self.last.get_mut(&point.box_key) {
            None => {
                self.last
                    .insert(point.box_key, (point.pts, point.pts_count.max(0)));
                None
            }
            Some(&mut (last_pts, _last_count)) => {
                let expected = last_pts.saturating_add(point.pts_count.max(0));
                let signal = (point.pts > expected).then_some(GapSignal {
                    box_key: point.box_key,
                    expected_pts: expected,
                    observed_pts: point.pts,
                });
                if point.pts > last_pts {
                    self.last
                        .insert(point.box_key, (point.pts, point.pts_count.max(0)));
                }
                signal
            }
        }
    }
}

fn pts_point(raw: &tl::enums::Update) -> Option<PtsPoint> {
    let common = |pts: i32, pts_count: i32| {
        Some(PtsPoint {
            box_key: StateBoxKey::Common,
            pts,
            pts_count,
        })
    };
    let channel_of_message = |message: &tl::enums::Message| -> Option<i64> {
        match message {
            tl::enums::Message::Message(m) => peer_channel_id(&m.peer_id),
            tl::enums::Message::Service(s) => peer_channel_id(&s.peer_id),
            tl::enums::Message::Empty(_) => None,
        }
    };
    match raw {
        tl::enums::Update::NewMessage(x) => common(x.pts, x.pts_count),
        tl::enums::Update::EditMessage(x) => common(x.pts, x.pts_count),
        tl::enums::Update::DeleteMessages(x) => common(x.pts, x.pts_count),
        tl::enums::Update::NewChannelMessage(x) => {
            channel_of_message(&x.message).map(|channel_id| PtsPoint {
                box_key: StateBoxKey::Channel(channel_id),
                pts: x.pts,
                pts_count: x.pts_count,
            })
        }
        tl::enums::Update::EditChannelMessage(x) => {
            channel_of_message(&x.message).map(|channel_id| PtsPoint {
                box_key: StateBoxKey::Channel(channel_id),
                pts: x.pts,
                pts_count: x.pts_count,
            })
        }
        tl::enums::Update::DeleteChannelMessages(x) => Some(PtsPoint {
            box_key: StateBoxKey::Channel(x.channel_id),
            pts: x.pts,
            pts_count: x.pts_count,
        }),
        _ => None,
    }
}

fn peer_channel_id(peer: &tl::enums::Peer) -> Option<i64> {
    match peer {
        tl::enums::Peer::Channel(c) => Some(c.channel_id),
        _ => None,
    }
}

fn gap_row(account: &str, gap: &GapSignal, state: &State) -> serde_json::Value {
    let mut row = event_row("Gap", account, None, None, None);
    if let serde_json::Value::Object(map) = &mut row {
        map.insert("reason".into(), serde_json::Value::from("pts_jump"));
        map.insert("expected_pts".into(), serde_json::json!(gap.expected_pts));
        map.insert("observed_pts".into(), serde_json::json!(gap.observed_pts));
        if let StateBoxKey::Channel(channel_id) = gap.box_key {
            map.insert("channel_id".into(), serde_json::json!(channel_id));
        }
        map.insert("state".into(), state_to_json(state));
    }
    row
}

fn album_member(row: &serde_json::Value) -> Option<(i64, i64)> {
    let grouped_id = row.get("grouped_id")?.as_i64()?;
    let chat_id = row.get("peer")?.get("id")?.as_i64()?;
    Some((chat_id, grouped_id))
}

struct PendingAlbum {
    chat_id: i64,
    grouped_id: i64,
    rows: Vec<serde_json::Value>,
    deadline: tokio::time::Instant,
}

type AlbumKey = (i64, i64);
type AlbumBuffer = HashMap<AlbumKey, PendingAlbum>;

fn album_ingest(
    account: &str,
    buf: &mut AlbumBuffer,
    row: serde_json::Value,
    chat_id: i64,
    grouped_id: i64,
    now: tokio::time::Instant,
) -> Vec<serde_json::Value> {
    let mut flushed = Vec::new();
    let key = (chat_id, grouped_id);
    if let Some(pending) = buf.get_mut(&key) {
        if pending.rows.len() >= MAX_ALBUM_LEN {
            flushed.push(album_complete(account, pending));
            buf.remove(&key);
        }
    } else if buf.len() >= ALBUM_BUFFER_CAP {
        if let Some(oldest) = buf
            .values()
            .min_by_key(|p| (p.deadline, p.chat_id, p.grouped_id))
            .map(|p| (p.chat_id, p.grouped_id))
        {
            if let Some(pending) = buf.remove(&oldest) {
                flushed.push(album_complete(account, &pending));
            }
        }
    }
    let entry = buf.entry(key).or_insert_with(|| PendingAlbum {
        chat_id,
        grouped_id,
        rows: Vec::new(),
        deadline: now,
    });
    entry.rows.push(row);
    entry.deadline = now + std::time::Duration::from_millis(ALBUM_FLUSH_MILLIS);
    flushed
}

fn album_flush(account: &str, buf: &mut AlbumBuffer) -> Vec<serde_json::Value> {
    let mut keys: Vec<AlbumKey> = buf.keys().copied().collect();
    keys.sort_by_key(|k| (buf[k].deadline, *k));
    keys.into_iter()
        .filter_map(|k| buf.remove(&k).map(|p| album_complete(account, &p)))
        .collect()
}

/// Emit a buffered row without consulting the count budget. Returns true when
/// the stream must stop (stdout is gone); a row outside the `--until` window
/// is skipped and also returns false.
///
/// The `--until` window still applies, with one exception: an `Album` row
/// carries its first member's date, so an album whose members straddle the
/// boundary is written as one unit rather than split.
async fn emit_direct(
    writer: &ListenWriter,
    stopper: &StreamStopper,
    account: &str,
    value: serde_json::Value,
) -> TeleResult<bool> {
    if stopper.past_until(&value) {
        return Ok(false);
    }
    match writer.emit(account, value).await {
        Ok(_) => Ok(false),
        Err(e) => match on_emit_failure(stopper, account, &e) {
            EmitFailure::Usage => Err(e),
            EmitFailure::Stop => Ok(true),
        },
    }
}

/// Emit pending albums after a stop was decided. Returns true when stdout is
/// gone and the account task must end.
///
/// The stop decision already consumed the `--count` budget (or hit the
/// date boundary), so re-entering `emit_row_or_stop` here would veto every
/// row before it was written. These members were already received off the
/// wire, so dropping them is silent data loss: they would appear neither
/// as an `Album` row nor as individual `NewMessage` rows.
async fn flush_albums_on_stop(
    writer: &ListenWriter,
    stopper: &StreamStopper,
    account: &str,
    album: &mut AlbumBuffer,
) -> TeleResult<bool> {
    for row in album_flush(account, album) {
        if emit_direct(writer, stopper, account, row).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Emit a row, flushing pending albums when the stream stops.
async fn emit_or_stop(
    writer: &ListenWriter,
    stopper: &StreamStopper,
    account: &str,
    album: &mut AlbumBuffer,
    value: serde_json::Value,
) -> TeleResult<bool> {
    if emit_row_or_stop(writer, stopper, account, value)
        .await?
        .stop
    {
        flush_albums_on_stop(writer, stopper, account, album).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Raw rows are independent of the parsed event allowlist: `--events Raw`
/// and `--raw` must report message updates even when their parsed family is
/// not selected.
struct RawEmitCtx<'a> {
    filter: &'a EventFilter,
    raw_on: bool,
    update: &'a tl::enums::Update,
    state: &'a State,
}

async fn emit_raw_update(
    writer: &ListenWriter,
    stopper: &StreamStopper,
    account: &str,
    album: &mut AlbumBuffer,
    ctx: RawEmitCtx<'_>,
) -> TeleResult<bool> {
    if !ctx.raw_on || is_empty_update(ctx.update) || !ctx.filter.raw_allows(update_peer(ctx.update))
    {
        return Ok(false);
    }
    emit_or_stop(
        writer,
        stopper,
        account,
        album,
        raw_row(account, ctx.update, ctx.state),
    )
    .await
}

/// Emit the due albums a sweep already removed from the buffer.
///
/// A stop must write the swept rows before the buffer's pending albums: the
/// sweep removes rows in deadline order, so flushing the buffer first would
/// emit a later-deadline album before an earlier-due one, and dropping the
/// leftovers is silent data loss.
async fn emit_due_albums(
    writer: &ListenWriter,
    stopper: &StreamStopper,
    account: &str,
    album: &mut AlbumBuffer,
    swept: Vec<serde_json::Value>,
) -> TeleResult<bool> {
    for (index, done) in swept.iter().enumerate() {
        let step = emit_row_or_stop(writer, stopper, account, done.clone()).await?;
        if !step.stop {
            continue;
        }
        // A stop decided before the write (count spent by another account, a
        // date boundary, a dead writer) must not drop this row: it left the
        // buffer already, so nothing else will emit it.
        if !step.wrote && emit_direct(writer, stopper, account, done.clone()).await? {
            return Ok(true);
        }
        for leftover in swept_after_stop(&swept, index) {
            if emit_direct(writer, stopper, account, leftover.clone()).await? {
                return Ok(true);
            }
        }
        flush_albums_on_stop(writer, stopper, account, album).await?;
        return Ok(true);
    }
    Ok(false)
}

fn album_flush_chat(
    account: &str,
    buf: &mut AlbumBuffer,
    chat_id: Option<i64>,
) -> Vec<serde_json::Value> {
    let mut keys: Vec<AlbumKey> = buf
        .keys()
        .copied()
        .filter(|k| Some(k.0) == chat_id)
        .collect();
    keys.sort_by_key(|k| (buf[k].deadline, *k));
    keys.into_iter()
        .filter_map(|k| buf.remove(&k).map(|p| album_complete(account, &p)))
        .collect()
}

/// Rows after the one that triggered a stop, from a list the sweep already
/// removed from its buffer. They were received off the wire, so emitting them
/// directly is the only way they survive the stop.
fn swept_after_stop(swept: &[serde_json::Value], stop_index: usize) -> &[serde_json::Value] {
    swept.get(stop_index + 1..).unwrap_or(&[])
}

fn album_sweep(
    account: &str,
    buf: &mut AlbumBuffer,
    now: tokio::time::Instant,
) -> Vec<serde_json::Value> {
    let mut due: Vec<AlbumKey> = buf
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(k, _)| *k)
        .collect();
    due.sort_by_key(|k| (buf[k].deadline, *k));
    due.into_iter()
        .filter_map(|k| buf.remove(&k).map(|p| album_complete(account, &p)))
        .collect()
}

fn min_album_deadline(buf: &AlbumBuffer) -> Option<tokio::time::Instant> {
    buf.values().map(|p| p.deadline).min()
}

fn album_complete(account: &str, pending: &PendingAlbum) -> serde_json::Value {
    let ids: Vec<i32> = pending
        .rows
        .iter()
        .filter_map(|r| {
            r.get("id")
                .and_then(|v| v.as_i64())
                .and_then(|v| i32::try_from(v).ok())
        })
        .collect();
    let mut row = event_row("Album", account, Some(pending.chat_id), None, None);
    if let serde_json::Value::Object(map) = &mut row {
        map.insert("grouped_id".into(), serde_json::json!(pending.grouped_id));
        map.insert("ids".into(), serde_json::json!(ids));
        if let Some(first) = pending.rows.first().and_then(|r| r.get("date").cloned()) {
            // The row is one event dated by its first member, so a straddling
            // album is emitted whole rather than split at --until.
            map.insert("date".into(), first);
        }
        map.insert(
            "messages".into(),
            serde_json::Value::Array(pending.rows.clone()),
        );
    }
    row
}

struct ObservedPeers {
    by_id: crate::capped_map::CappedMap<(PeerId, i32), PeerId>,
}

impl ObservedPeers {
    fn new() -> Self {
        ObservedPeers {
            by_id: crate::capped_map::CappedMap::new(OBSERVED_PEER_CAP),
        }
    }

    fn observe(&mut self, id: i32, peer: PeerId) {
        let key = (peer, id);
        if !self.by_id.contains(&key) {
            self.by_id.insert(key, peer);
        }
    }

    fn peer_of(&self, peer: PeerId, id: i32) -> Option<PeerId> {
        self.by_id.get(&(peer, id)).copied()
    }
}

fn observed_deletion_ids(ids: &[i32], observed: &ObservedPeers, target: PeerId) -> Vec<i32> {
    ids.iter()
        .copied()
        .filter(|id| observed.peer_of(target, *id) == Some(target))
        .collect()
}

const LISTEN_DEDUPE_CAP: usize = 10_000;

type ListenDedupe = super::serve::CappedDedupe<(i64, i32, i32)>;

pub(crate) fn dedupe_key(
    chat_id: Option<i64>,
    msg_id: i32,
    raw: &tl::enums::Update,
) -> Option<(i64, i32, i32)> {
    let point = pts_point(raw)?;
    Some((chat_id.unwrap_or(0), msg_id, point.pts))
}

#[cfg(test)]
use super::serve::pts_from_state;

/// Parse `--until` into both bounds: the wall-clock deadline (only for a
/// FUTURE timestamp) and the event-date limit.
///
/// A past timestamp gets NO wall-clock deadline. The old code turned the
/// negative offset into `Duration::ZERO`, so the deadline was already
/// expired and the stream stopped before its first row — the documented
/// "first event at/after that timestamp" filter never had a chance to
/// match the backlog it was meant to bound.
fn until_bounds(
    value: Option<&str>,
) -> TeleResult<(
    Option<std::time::Instant>,
    Option<chrono::DateTime<chrono::Utc>>,
)> {
    match value {
        None => Ok((None, None)),
        Some(v) => {
            let dt = crate::commands::msg::download::parse_download_date("--until", v)?;
            let deadline = (dt - chrono::Utc::now())
                .to_std()
                .ok()
                .map(|d| std::time::Instant::now() + d);
            Ok((deadline, Some(dt)))
        }
    }
}

pub async fn run(args: &ListenArgs, flags: &GlobalFlags) -> TeleResult<i32> {
    let config_path = flags.config_path.clone();
    use grammers_client::update::Update;
    let mut events: Vec<String> = args.events.clone();
    events.retain(|e| VALID_EVENTS.contains(&e.as_str()));
    if events.len() != args.events.len() {
        return Err(TeleError::Usage(format!(
            "unknown event name in --events (valid: {})",
            VALID_EVENTS.join(", ")
        )));
    }
    if args.raw && !events.iter().any(|e| e == "Raw") {
        events.push("Raw".to_string());
    }
    let filter_patterns = compile_pattern(&args.pattern)?;
    require_explicit_selection("listen", flags)?;
    let names = crate::executor::select_accounts(flags)?;
    if names.is_empty() {
        return Err(TeleError::Usage(
            "no accounts selected: use --account <name> or --tag <tag>".to_string(),
        ));
    }
    let chat_targets = args.chat.clone();
    let from_targets = args.from.clone();
    validate_listen_inputs(&chat_targets, &from_targets)?;
    if flags.dry_run {
        output::log_line("info", "[dry-run] would stream updates");
        if flags.json || flags.jsonl {
            for name in &names {
                output::print_json_result(&dry_run_row(&events, name))?;
            }
        }
        return Ok(crate::error::EXIT_OK);
    }
    if !flags.json && !flags.jsonl {
        output::log_line("info", "listen streams JSONL events on stdout");
    }
    let timeout_secs = args.timeout_secs;
    let (until, until_date) = until_bounds(args.until.as_deref())?;
    if args.count == Some(0) {
        // Zero emitted rows is the goal, and no event can ever be emitted:
        // connect is pure cost, and without a timeout the stream would never
        // end on its own.
        output::log_line("info", "listen --count 0: emitting no rows");
        return Ok(crate::error::EXIT_OK);
    }
    let stopper = Arc::new(StreamStopper::new(args.count, until, until_date));
    let writer = spawn_listen_writer();
    let direction = if args.out {
        Some(Direction::Out)
    } else if args.r#in {
        Some(Direction::In)
    } else {
        None
    };
    let cfg = crate::config::load_config(config_path.as_deref())?;
    let parallel = crate::executor::effective_parallel(flags.parallel, cfg.parallel_max)? as usize;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut tasks = tokio::task::JoinSet::new();
    for name in names {
        let config_path = config_path.clone();
        let chat_targets = chat_targets.clone();
        let from_targets = from_targets.clone();
        let events = events.clone();
        let filter_patterns = filter_patterns.clone();
        let semaphore = Arc::clone(&semaphore);
        let stopper = Arc::clone(&stopper);
        let writer = writer.clone();
        tasks.spawn(async move {
            let result: TeleResult<()> = async {
                let creds =
                    crate::config::credentials().map_err(|e| TeleError::Config(e.to_string()))?;
                let deadline = if timeout_secs > 0 {
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs))
                } else {
                    None
                };
                let mut filter = EventFilter::default();
                let mut targets_resolved = false;
                let mut failures: u32 = 0;
                let mut dedupe = ListenDedupe::new(LISTEN_DEDUPE_CAP);
                let rate_limiter = match ClientGuard::account_rate_limiter(
                    &name,
                    config_path.as_deref(),
                ) {
                    Ok(rl) => rl,
                    Err(e) => return Err(TeleError::from(e)),
                };
                // Gap/peer/album state outlives reconnects: recreating them
                // per attempt would blind gap detection and deletion signals
                // across outages.
                let mut gaps = GapTracker::default();
                let mut observed = ObservedPeers::new();
                loop {
                    if let Some(d) = deadline {
                        if std::time::Instant::now() >= d {
                            break;
                        }
                    }
                    if stopper.stop_requested() || stopper.time_up() {
                        // Another account spent the shared count budget or
                        // the shared wall-clock --until bound expired while
                        // this one was reconnecting; the pending albums were
                        // flushed by the inner loop before it broke.
                        break;
                    }
                    let mut guard = {
                        let _permit = Arc::clone(&semaphore).acquire_owned().await;
                        match ClientGuard::connect_with_limiter(
                            &name,
                            creds.api_id,
                            config_path.as_deref(),
                            std::sync::Arc::clone(&rate_limiter),
                        )
                        .await
                        {
                            Ok(guard) => {
                                if let Err(e) = client::authorize(&guard.client).await {
                                    let ctx =
                                        StreamFailureCtx::before_connect(deadline, Some(&stopper));
                                    handle_stream_failure(
                                        &name,
                                        e,
                                        &mut failures,
                                        ctx,
                                        MAX_RECONNECT_ATTEMPTS,
                                    )
                                    .await?;
                                    continue;
                                }
                                guard
                            }
                            Err(e) => {
                                let ctx =
                                    StreamFailureCtx::before_connect(deadline, Some(&stopper));
                                handle_stream_failure(
                                    &name,
                                    TeleError::from(e),
                                    &mut failures,
                                    ctx,
                                    MAX_RECONNECT_ATTEMPTS,
                                )
                                .await?;
                                continue;
                            }
                        }
                    };
                    if stopper.stop_requested() || stopper.time_up() {
                        break;
                    }
                    if !targets_resolved {
                        filter = resolve_filter(
                            &guard.client,
                            &guard.session,
                            &chat_targets,
                            &from_targets,
                            direction,
                        )
                        .await?;
                        filter.patterns = filter_patterns.clone();
                        targets_resolved = true;
                    }
                    if let Err(e) = guard
                        .client
                        .invoke(&tl::functions::updates::GetState {})
                        .await
                    {
                        let err = getstate_probe_error(e);
                        if is_auth_error(&err) {
                            output::log_line(
                                "error",
                                &format!("{name}: not authorized, stopping stream"),
                            );
                            return Err(err);
                        }
                        let ctx = StreamFailureCtx::before_connect(deadline, Some(&stopper));
                        handle_stream_failure(
                            &name,
                            err,
                            &mut failures,
                            ctx,
                            MAX_RECONNECT_ATTEMPTS,
                        )
                        .await?;
                        continue;
                    }
                    let receiver = std::mem::replace(
                        &mut guard.updates,
                        tokio::sync::mpsc::unbounded_channel().1,
                    );
                    let mut stream = match guard
                        .client
                        .stream_updates(
                            receiver,
                            grammers_client::client::UpdatesConfiguration {
                                catch_up: true,
                                update_queue_limit: Some(1000),
                            },
                        )
                        .await
                    {
                        Ok(stream) => stream,
                        Err(e) => {
                            let ctx =
                                StreamFailureCtx::before_connect(deadline, Some(&stopper));
                            handle_stream_failure(
                                &name,
                                TeleError::Other(e.to_string()),
                                &mut failures,
                                ctx,
                                MAX_RECONNECT_ATTEMPTS,
                            )
                            .await?;
                            continue;
                        }
                    };
                    let connected_at = std::time::Instant::now();
                    // gaps/observed are declared before the reconnect loop
                    // (see below) so gap detection and peer bookkeeping
                    // survive an outage instead of resetting blind. The album
                    // buffer is per attempt and every break path flushes it.
                    let mut album = AlbumBuffer::new();
                    let gap_on = events.iter().any(|e| e == "Gap");
                    let album_on = events.iter().any(|e| e == "Album");
                    let new_message_on = events.iter().any(|e| e == "NewMessage");
                    let service_on = events.iter().any(|e| e == "Service");
                    let chat_action_on = events.iter().any(|e| e == "ChatAction");
                    let user_update_on = events.iter().any(|e| e == "UserUpdate");
                    let callback_query_on = events.iter().any(|e| e == "CallbackQuery");
                    let raw_on = events.iter().any(|e| e == "Raw");
                    loop {
                        if let Some(d) = deadline {
                            if std::time::Instant::now() >= d {
                                // The deadline already ends the run, so these
                                // rows are written directly: routing them
                                // through the stopper would veto them once
                                // the count budget is spent.
                                if flush_albums_on_stop(&writer, &stopper, &name, &mut album)
                                    .await?
                                {
                                    return Ok(());
                                }
                                break;
                            }
                        }
                        enum Tick {
                            Update(Box<grammers_client::update::Update>),
                            StreamError(grammers_client::InvocationError),
                            Idle,
                            AlbumDue,
                            StopRequested,
                        }
                        let album_timer = async {
                            match min_album_deadline(&album) {
                                Some(due) => tokio::time::sleep_until(due).await,
                                None => std::future::pending::<()>().await,
                            }
                        };
                        let tick = tokio::select! {
                            _ = album_timer => Tick::AlbumDue,
                            _ = stopper.wait_for_stop() => Tick::StopRequested,
                            res = tokio::time::timeout(
                                poll_timeout(deadline, std::time::Instant::now()),
                                stream.next(),
                            ) => match res {
                                Ok(Ok(u)) => Tick::Update(Box::new(u)),
                                Ok(Err(e)) => Tick::StreamError(e),
                                Err(_) => Tick::Idle,
                            },
                        };
                        let update = match tick {
                            Tick::StopRequested => {
                                // The stop decision already spent the count
                                // budget (or hit the boundary), so these rows
                                // are written directly: re-entering the
                                // stopper would veto them (see the deadline
                                // arm above).
                                if flush_albums_on_stop(&writer, &stopper, &name, &mut album)
                                    .await?
                                {
                                    return Ok(());
                                }
                                break;
                            }
                            Tick::AlbumDue => {
                                let swept: Vec<serde_json::Value> =
                                    album_sweep(&name, &mut album, tokio::time::Instant::now());
                                if emit_due_albums(&writer, &stopper, &name, &mut album, swept)
                                    .await?
                                {
                                    return Ok(());
                                }
                                continue;
                            }
                            Tick::Idle => continue,
                            Tick::StreamError(e) => {
                                if crate::error::invocation_is_unauthorized(&e) {
                                    output::log_line(
                                        "error",
                                        &format!("{name}: not authorized, stopping stream"),
                                    );
                                    return Err(crate::error::invocation_error(e));
                                }
                                // The stream is about to reconnect; pending
                                // album members are written directly so a
                                // spent count budget cannot drop them.
                                if flush_albums_on_stop(&writer, &stopper, &name, &mut album)
                                    .await?
                                {
                                    return Ok(());
                                }
                                let ctx = StreamFailureCtx::live(
                                    connected_at.elapsed(),
                                    deadline,
                                    Some(&stopper),
                                );
                                handle_stream_failure(
                                    &name,
                                    crate::error::invocation_error(e),
                                    &mut failures,
                                    ctx,
                                    MAX_RECONNECT_ATTEMPTS,
                                )
                                .await?;
                                break;
                            }
                            Tick::Update(u) => *u,
                        };
                        failures = streak_after_update(failures, is_empty_update(update.raw()));
                        if let Err(e) = stream.sync_update_state().await {
                            output::log_line(
                                "warn",
                                &format!("{name}: sync_update_state failed: {e}"),
                            );
                        }
                        if gap_on {
                            if let Some(signal) = gaps.observe(update.raw()) {
                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, gap_row(&name, &signal, update.state())).await? { return Ok(()); }

                            }
                        }
                        if emit_raw_update(
                            &writer,
                            &stopper,
                            &name,
                            &mut album,
                            RawEmitCtx {
                                filter: &filter,
                                raw_on,
                                update: update.raw(),
                                state: update.state(),
                            },
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        match &update {
                            Update::NewMessage(m) => {
                                let service_flavored =
                                    matches!(&(**m).raw, tl::enums::Message::Service(_));
                                if !message_event_applies(
                                    new_message_on,
                                    album_on,
                                    service_on,
                                    service_flavored,
                                ) {
                                    continue;
                                }
                                let peer = update_peer(&m.raw);
                                if !filter.chat_allows(peer) {
                                    continue;
                                }
                                if is_empty_update(&m.raw) {
                                    continue;
                                }
                                if !filter
                                    .message_allows(update_sender(&m.raw), update_outgoing(&m.raw))
                                {
                                    continue;
                                }
                                if !filter.text_allows(update_text(&m.raw)) {
                                    continue;
                                }
                                if !filter.chats.is_empty() {
                                    if let Some(peer) = m.peer() {
                                        observed.observe(m.id(), peer.id());
                                    }
                                }
                                let chat_id = peer.and_then(|p| p.bare_id());
                                if let Some(key) = dedupe_key(chat_id, m.id(), update.raw()) {
                                    if dedupe.check(key) {
                                        continue;
                                    }
                                }
                                if routes_to_service(service_on, service_flavored) {
                                    if let tl::enums::Message::Service(svc) = &(**m).raw {
                                        let mut svc_row = match streamed_message_row(m) {
                                            Ok(r) => r,
                                            Err(e) => {
                                                output::log_line(
                                                    "error",
                                                    &format!(
                                                        "{name}: failed to serialize message {}: {}",
                                                        m.id(),
                                                        e.message()
                                                    ),
                                                );
                                                continue;
                                            }
                                        };
                                        crate::serialize::ensure_outer_peer_sender(
                                            &mut svc_row,
                                            peer,
                                            None,
                                        );
                                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, service_row(
                                            &name, chat_id, svc_row, &svc.action,
                                        )).await? { return Ok(()); }

                                        continue;
                                    }
                                }
                                let mut row = match streamed_message_row(m) {
                                    Ok(r) => r,
                                    Err(e) => {
                                        output::log_line(
                                            "error",
                                            &format!(
                                                "{name}: failed to serialize message {}: {}",
                                                m.id(),
                                                e.message()
                                            ),
                                        );
                                        continue;
                                    }
                                };
                                crate::serialize::ensure_outer_peer_sender(&mut row, peer, None);
                                let grouped = if album_on { album_member(&row) } else { None };
                                match (grouped, chat_id) {
                                    (Some((member_chat, gid)), _) => {
                                        let swept = album_ingest(
                                            &name,
                                            &mut album,
                                            row,
                                            member_chat,
                                            gid,
                                            tokio::time::Instant::now(),
                                        );
                                        if emit_due_albums(
                                            &writer, &stopper, &name, &mut album, swept,
                                        )
                                        .await?
                                        {
                                            return Ok(());
                                        }
                                    }
                                    (None, _) => {
                                        if !new_message_on {
                                            continue;
                                        }
                                        let swept = album_flush_chat(&name, &mut album, chat_id);
                                        if emit_due_albums(
                                            &writer, &stopper, &name, &mut album, swept,
                                        )
                                        .await?
                                        {
                                            return Ok(());
                                        }
                                        if emit_or_stop(
                                            &writer,
                                            &stopper,
                                            &name,
                                            &mut album,
                                            event_row("NewMessage", &name, chat_id, None, Some(row)),
                                        )
                                        .await?
                                        {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                            Update::MessageEdited(m) => {
                                let service_flavored =
                                    matches!(&(**m).raw, tl::enums::Message::Service(_));
                                if !routes_to_service(service_on, service_flavored)
                                    && !events.iter().any(|e| e == "MessageEdited")
                                {
                                    continue;
                                }
                                let peer = update_peer(&m.raw);
                                if !filter.chat_allows(peer) {
                                    continue;
                                }
                                if is_empty_update(&m.raw) {
                                    continue;
                                }
                                if !filter
                                    .message_allows(update_sender(&m.raw), update_outgoing(&m.raw))
                                {
                                    continue;
                                }
                                if !filter.text_allows(update_text(&m.raw)) {
                                    continue;
                                }
                                if !filter.chats.is_empty() {
                                    if let Some(peer) = m.peer() {
                                        observed.observe(m.id(), peer.id());
                                    }
                                }
                                let chat_id = peer.and_then(|p| p.bare_id());
                                if let Some(key) = dedupe_key(chat_id, m.id(), update.raw()) {
                                    if dedupe.check(key) {
                                        continue;
                                    }
                                }
                                if routes_to_service(service_on, service_flavored) {
                                    if let tl::enums::Message::Service(svc) = &(**m).raw {
                                        let mut svc_row = match streamed_message_row(m) {
                                            Ok(r) => r,
                                            Err(e) => {
                                                output::log_line(
                                                    "error",
                                                    &format!(
                                                        "{name}: failed to serialize message {}: {}",
                                                        m.id(),
                                                        e.message()
                                                    ),
                                                );
                                                continue;
                                            }
                                        };
                                        crate::serialize::ensure_outer_peer_sender(
                                            &mut svc_row,
                                            peer,
                                            None,
                                        );
                                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, service_row(
                                            &name, chat_id, svc_row, &svc.action,
                                        )).await? { return Ok(()); }

                                        continue;
                                    }
                                }
                                let mut row = match streamed_message_row(m) {
                                    Ok(r) => r,
                                    Err(e) => {
                                        output::log_line(
                                            "error",
                                            &format!(
                                                "{name}: failed to serialize message {}: {}",
                                                m.id(),
                                                e.message()
                                            ),
                                        );
                                        continue;
                                    }
                                };
                                crate::serialize::ensure_outer_peer_sender(&mut row, peer, None);
                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, event_row(
                                    "MessageEdited",
                                    &name,
                                    chat_id,
                                    None,
                                    Some(row),
                                )).await? { return Ok(()); }

                            }
                            Update::MessageDeleted(d) => {
                                if !events.iter().any(|e| e == "MessageDeleted") {
                                    continue;
                                }
                                if !filter.deletions_pass() {
                                    continue;
                                }
                                let matched = match filter.chats.as_slice() {
                                    [] => Some(d.messages().to_vec()),
                                    targets => deletion_match_set(
                                        d.messages(),
                                        d.channel_id(),
                                        &observed,
                                        targets,
                                    ),
                                };
                                let Some(matched) = matched else {
                                    continue;
                                };
                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, event_row(
                                    "MessageDeleted",
                                    &name,
                                    sole_chat_label(&filter.chats),
                                    Some(&matched),
                                    None,
                                )).await? { return Ok(()); }

                            }
                            _ => {
                                if chat_action_on {
                                    if let Some((peer, sender, row)) =
                                        chat_action_row(&name, update.raw())
                                    {
                                        if !filter.action_allows(peer, sender) {
                                            continue;
                                        }
                                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, row).await? { return Ok(()); }

                                        continue;
                                    }
                                }
                                if user_update_on {
                                    if let Some((peer, sender, row)) =
                                        user_update_row(&name, update.raw())
                                    {
                                        if !filter.action_allows(peer, sender) {
                                            continue;
                                        }
                                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, row).await? { return Ok(()); }

                                        continue;
                                    }
                                }
                                if callback_query_on {
                                    if let Some((peer, sender, row)) =
                                        callback_query_row(&name, update.raw())
                                    {
                                        if !filter.action_allows(peer, sender) {
                                            continue;
                                        }
                                                                                if emit_or_stop(&writer, &stopper, &name, &mut album, row).await? { return Ok(()); }

                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = &result {
                output::log_line("error", &format!("{name}: {}", e.message()));
            }
            result
        });
    }
    let mut ok_count = 0usize;
    let mut failed: Vec<i32> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => ok_count += 1,
            Ok(Err(e)) => failed.push(e.exit_code()),
            Err(e) if e.is_cancelled() => failed.push(crate::error::EXIT_ALL_FAILED),
            Err(e) => {
                let msg = match e.try_into_panic() {
                    Ok(payload) => {
                        if let Some(s) = payload.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "stream task panicked".to_string()
                        }
                    }
                    Err(_) => "stream task panicked".to_string(),
                };
                output::log_line("error", &format!("a listen stream task panicked: {msg}"));
                failed.push(TeleError::TaskPanic(msg.clone()).exit_code());
            }
        }
    }
    writer.finish().await;
    if until.is_some() && stopper.time_up() && stopper.report_until_reached() {
        output::log_line("info", "listen --until reached");
    }
    if timeout_secs > 0 {
        output::log_line("info", "listen timeout reached");
    }
    // A projection failure applies to the whole run, so a partial exit would
    // hide that some emitted shapes could never be projected.
    Ok(listen_exit_code(stopper.usage_failure(), ok_count, &failed))
}

/// A projection failure promotes a successful or partial run to usage, because
/// the row shapes are unusable as a whole. When every account failed, the
/// normal precedence still decides: auth and Telegram/IO failures outrank a
/// usage exit and are more actionable for the caller.
fn listen_exit_code(usage_failure: bool, ok_count: usize, failed: &[i32]) -> i32 {
    let code = crate::error::aggregate_exit_code(ok_count, failed);
    if usage_failure && matches!(code, crate::error::EXIT_OK | crate::error::EXIT_PARTIAL) {
        crate::error::EXIT_USAGE
    } else {
        code
    }
}

enum EmitOutcome {
    /// The row was handed to the writer thread; it will be written in order.
    Queued,
    /// The stdout consumer stayed stalled past the budget and the row was
    /// dropped. The drop is counted and stamped on the next written row.
    Dropped,
}

#[derive(Clone)]
struct ListenWriter {
    tx: tokio::sync::mpsc::Sender<String>,
    dropped: std::sync::Arc<std::sync::Mutex<u32>>,
    error: std::sync::Arc<std::sync::Mutex<Option<TeleError>>>,
    handle: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,
    stall_budget: std::time::Duration,
}

impl ListenWriter {
    /// Hand one row to the writer thread, waiting at most `stall_budget`.
    ///
    /// The handoff is bounded because grammers hands the client an UNBOUNDED
    /// update channel: parking here forever would let it grow without limit
    /// while stdout is stalled. A row that cannot be handed over in time is
    /// dropped, counted, and surfaced on the next row that is.
    async fn emit(&self, account: &str, value: serde_json::Value) -> TeleResult<EmitOutcome> {
        let pending = self.take_dropped();
        let line = match stream_row_line_with_dropped(&value, pending) {
            Ok(line) => line,
            Err(e) => {
                self.add_dropped(pending);
                return Err(e);
            }
        };
        match tokio::time::timeout(self.stall_budget, self.tx.send(line)).await {
            Ok(Ok(())) => Ok(EmitOutcome::Queued),
            Ok(Err(_)) => {
                self.add_dropped(pending);
                Err(self.writer_error())
            }
            Err(_) => {
                let dropped = self.add_dropped(pending.saturating_add(1));
                output::log_line(
                    "warn",
                    &format!(
                        "listen: account {account} dropped a row: stdout stayed stalled for {}s ({dropped} dropped since the last written row)",
                        self.stall_budget.as_secs()
                    ),
                );
                Ok(EmitOutcome::Dropped)
            }
        }
    }

    /// Close the writer and wait until every queued row is flushed. Account
    /// tasks must be joined first so their writer clones are gone.
    async fn finish(self) {
        let stranded = self.take_dropped();
        let ListenWriter {
            tx, error, handle, ..
        } = self;
        drop(tx);
        let handle = handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            // The writer thread can be inside a write to a consumer that
            // stopped reading, so the join is bounded: a reader that never
            // drains must not keep the process alive after the stream ends.
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let spawned = std::thread::Builder::new()
                .name("listen-writer-join".to_string())
                .spawn(move || {
                    let _ = handle.join();
                    let _ = done_tx.send(());
                });
            match spawned {
                Ok(_) => match tokio::time::timeout(LISTEN_FLUSH_BUDGET, done_rx).await {
                    Ok(_) => {}
                    Err(_) => output::log_line(
                        "warn",
                        &format!(
                            "listen: stdout did not drain within {}s at shutdown",
                            LISTEN_FLUSH_BUDGET.as_secs()
                        ),
                    ),
                },
                Err(e) => output::log_line(
                    "warn",
                    &format!("listen: could not join the jsonl writer: {e}"),
                ),
            }
        }
        if stranded > 0 {
            output::log_line(
                "warn",
                &format!("listen: {stranded} row(s) were dropped before the stream ended"),
            );
        }
        let err = error.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(err) = err {
            if !err.is_broken_pipe() {
                output::log_line(
                    "error",
                    &format!("listen: stdout writer failed: {}", err.message()),
                );
            }
        }
    }

    fn take_dropped(&self) -> u32 {
        let mut guard = self.dropped.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }

    fn add_dropped(&self, delta: u32) -> u32 {
        let mut guard = self.dropped.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.saturating_add(delta);
        *guard
    }

    fn writer_error(&self) -> TeleError {
        let guard = self.error.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(e) if e.is_broken_pipe() => TeleError::BrokenPipe,
            Some(e) => TeleError::Other(e.message()),
            None => TeleError::TaskPanic("listen writer thread stopped".to_string()),
        }
    }
}

/// One writer thread per run owns stdout for every account task, so rows keep
/// the order they were submitted in and the queue bound is the only buffer
/// between grammers and the terminal.
fn spawn_writer<F, W>(open: F, capacity: usize, stall_budget: std::time::Duration) -> ListenWriter
where
    F: FnOnce() -> W + Send + 'static,
    W: Write,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(capacity);
    let error = std::sync::Arc::new(std::sync::Mutex::new(None));
    let writer_error = std::sync::Arc::clone(&error);
    let handle = std::thread::Builder::new()
        .name("listen-jsonl-writer".to_string())
        .spawn(move || {
            let mut out = open();
            while let Some(line) = rx.blocking_recv() {
                let written = writeln!(out, "{line}")
                    .and_then(|()| out.flush())
                    .map_err(TeleError::from);
                if let Err(e) = written {
                    *writer_error
                        .lock()
                        .unwrap_or_else(|inner| inner.into_inner()) = Some(e);
                    break;
                }
            }
        })
        .expect("spawn listen jsonl writer thread");
    ListenWriter {
        tx,
        dropped: std::sync::Arc::new(std::sync::Mutex::new(0)),
        error,
        handle: std::sync::Arc::new(std::sync::Mutex::new(Some(handle))),
        stall_budget,
    }
}

fn spawn_listen_writer() -> ListenWriter {
    spawn_writer(
        || std::io::stdout().lock(),
        LISTEN_OUTPUT_CAPACITY,
        LISTEN_STALL_BUDGET,
    )
}

fn stream_row_line_with_dropped(value: &serde_json::Value, dropped: u32) -> TeleResult<String> {
    let mut projected = crate::output::apply_output_fields(value)?;
    if dropped > 0 {
        if let serde_json::Value::Object(map) = &mut projected {
            map.insert("dropped".to_string(), serde_json::Value::from(dropped));
        }
    }
    Ok(serde_json::to_string(&projected)?)
}

#[cfg(test)]
fn stream_row_line(value: &serde_json::Value) -> TeleResult<String> {
    stream_row_line_with_dropped(value, 0)
}

#[derive(Default)]
pub(crate) struct StreamStopper {
    remaining: std::sync::Mutex<Option<u64>>,
    until: std::sync::Mutex<Option<std::time::Instant>>,
    until_date: std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    stopped: std::sync::atomic::AtomicBool,
    usage_failure: std::sync::atomic::AtomicBool,
    until_reported: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl StreamStopper {
    fn new(
        count: Option<u64>,
        until: Option<std::time::Instant>,
        until_date: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        StreamStopper {
            remaining: std::sync::Mutex::new(count),
            until: std::sync::Mutex::new(until),
            until_date: std::sync::Mutex::new(until_date),
            stopped: std::sync::atomic::AtomicBool::new(false),
            usage_failure: std::sync::atomic::AtomicBool::new(false),
            until_reported: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn request_stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }

    fn request_usage_stop(&self) {
        self.usage_failure
            .store(true, std::sync::atomic::Ordering::Release);
        self.request_stop();
    }

    fn usage_failure(&self) -> bool {
        self.usage_failure
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Claim the one-shot "listen --until reached" stderr notice.
    fn report_until_reached(&self) -> bool {
        !self
            .until_reported
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Sleep until the run is over: `request_stop()` from another account or
    /// the `--until` wall-clock deadline.
    ///
    /// `notified()` snapshots the `notify_waiters` counter when it is created
    /// (tokio 1.54 `Notify::notified`), so a stop requested before the first
    /// poll is still observed. The 500ms poll bounds `time_up()`, which has no
    /// store to notify about.
    async fn wait_for_stop(&self) {
        loop {
            let notified = self.notify.notified();
            if self.stop_requested() || self.time_up() {
                return;
            }
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {},
            }
        }
    }

    fn stop_requested(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    fn time_up(&self) -> bool {
        let guard = self.until.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some(deadline) => std::time::Instant::now() >= deadline,
            None => false,
        }
    }

    /// Documented --until contract: stop after the first event at/after the
    /// timestamp.
    ///
    /// Message rows carry an RFC3339 `date`; Raw and Gap rows carry
    /// `state.date` (epoch seconds). Rows with neither (ChatAction,
    /// UserUpdate, CallbackQuery, MessageDeleted) can never match a date
    /// filter, so they never stop the stream — `--timeout-secs` bounds
    /// those runs.
    fn past_until(&self, value: &serde_json::Value) -> bool {
        let guard = self.until_date.lock().unwrap_or_else(|e| e.into_inner());
        let Some(limit) = *guard else {
            return false;
        };
        let row_date = value
            .get("date")
            .and_then(|d| d.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .or_else(|| {
                value
                    .get("state")
                    .and_then(|s| s.get("date"))
                    .and_then(|d| d.as_i64())
                    .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
            });
        match row_date {
            Some(d) => d >= limit,
            None => false,
        }
    }

    /// Reserve one event row against the shared `--count` budget before the
    /// row is written. The decrement and the "was there room" decision happen
    /// under one lock: checking the budget and decrementing after the write
    /// lets two accounts both pass the check and overshoot `--count`.
    fn reserve(&self) -> CountSlot {
        let mut guard = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            None => CountSlot::Reserved { last: false },
            Some(0) => CountSlot::Exhausted,
            Some(n) => {
                *n -= 1;
                CountSlot::Reserved { last: *n == 0 }
            }
        }
    }

    /// Give a reserved slot back when the row never reached stdout.
    fn release(&self) {
        let mut guard = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = guard.as_mut() {
            *n += 1;
        }
    }

    #[cfg(test)]
    fn is_exhausted(&self) -> bool {
        let guard = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        matches!(*guard, Some(0))
    }

    /// Returns true when this emit consumed the last allowed row.
    #[cfg(test)]
    fn emitted(&self) -> bool {
        matches!(
            self.reserve(),
            CountSlot::Reserved { last: true } | CountSlot::Exhausted
        )
    }
}

enum CountSlot {
    Reserved { last: bool },
    Exhausted,
}

/// What became of one row offered to the stream.
///
/// `stop` and `wrote` are independent: a date boundary stops without writing,
/// a spent count slot written by this row stops after writing, and a stalled
/// stdout drops a row without either.
#[derive(Clone, Copy)]
struct EmitStep {
    wrote: bool,
    stop: bool,
}

impl EmitStep {
    const WRITTEN: Self = Self {
        wrote: true,
        stop: false,
    };
    const SKIPPED: Self = Self {
        wrote: false,
        stop: false,
    };
    const STOPPED: Self = Self {
        wrote: false,
        stop: true,
    };
    const WRITTEN_LAST: Self = Self {
        wrote: true,
        stop: true,
    };
}

async fn emit_row_or_stop(
    writer: &ListenWriter,
    stopper: &StreamStopper,
    account: &str,
    value: serde_json::Value,
) -> TeleResult<EmitStep> {
    // A date stop is shared: once one account reaches the --until boundary
    // the whole run ends, instead of leaving quiet accounts streaming on.
    if stopper.past_until(&value) {
        stopper.request_stop();
        if stopper.report_until_reached() {
            output::log_line("info", "listen --until reached");
        }
        return Ok(EmitStep::STOPPED);
    }
    // time_up() only fires for a FUTURE --until (a past timestamp has no
    // wall-clock deadline), where it bounds the run even if no dated row
    // ever arrives.
    if stopper.stop_requested() || stopper.time_up() {
        return Ok(EmitStep::STOPPED);
    }
    let last = match stopper.reserve() {
        // A budget spent by another account stops only this account. The
        // spender's write can still be dropped (a stalled stdout releases the
        // slot), so an observer must not make the stop permanent.
        CountSlot::Exhausted => return Ok(EmitStep::STOPPED),
        CountSlot::Reserved { last } => last,
    };
    match writer.emit(account, value).await {
        Ok(EmitOutcome::Queued) => {}
        Ok(EmitOutcome::Dropped) => {
            stopper.release();
            return Ok(EmitStep::SKIPPED);
        }
        Err(e) => {
            stopper.release();
            return match on_emit_failure(stopper, account, &e) {
                EmitFailure::Usage => Err(e),
                EmitFailure::Stop => Ok(EmitStep::STOPPED),
            };
        }
    }
    if last {
        // The count budget is shared: without the broadcast the accounts
        // that never saw an event stay parked on their next poll (up to 1h).
        stopper.request_stop();
        output::log_line("info", "listen --count reached");
        return Ok(EmitStep::WRITTEN_LAST);
    }
    Ok(EmitStep::WRITTEN)
}

enum EmitFailure {
    Usage,
    Stop,
}

fn on_emit_failure(stopper: &StreamStopper, account: &str, err: &TeleError) -> EmitFailure {
    if matches!(err, TeleError::Usage(_)) {
        // --fields is one value for the whole run; a row shape without the
        // requested path makes the projection unusable. Stop every account
        // instead of streaming only the shapes that happen to match.
        stopper.request_usage_stop();
        return EmitFailure::Usage;
    }
    // The broadcast is what wakes accounts parked on a quiet stream; without
    // it they sit on their next poll (up to an hour) on a dead stdout.
    stopper.request_stop();
    if !emit_stops_stream(err) {
        // stdout is unusable: stop instead of logging once per row.
        output::log_line(
            "error",
            &format!("{account}: emit failed: {}", err.message()),
        );
    }
    EmitFailure::Stop
}

fn emit_stops_stream(err: &TeleError) -> bool {
    err.is_broken_pipe()
}

fn flood_wait_secs(err: &TeleError) -> Option<std::time::Duration> {
    match err {
        TeleError::Rpc(_, 420, _, Some(s)) => Some(std::time::Duration::from_secs(u64::from(*s))),
        TeleError::Invocation(_, Some(s)) => Some(std::time::Duration::from_secs(u64::from(*s))),
        _ => None,
    }
}

pub(crate) struct StreamFailureCtx<'a> {
    /// ZERO when no connection was established before the failure.
    pub stream_uptime: std::time::Duration,
    /// Wall-clock end of the run, when one is set.
    pub deadline: Option<std::time::Instant>,
    /// Shared stop source; serve runs without one.
    pub stopper: Option<&'a StreamStopper>,
}

impl<'a> StreamFailureCtx<'a> {
    pub(crate) fn before_connect(
        deadline: Option<std::time::Instant>,
        stopper: Option<&'a StreamStopper>,
    ) -> Self {
        Self {
            stream_uptime: std::time::Duration::ZERO,
            deadline,
            stopper,
        }
    }

    pub(crate) fn live(
        stream_uptime: std::time::Duration,
        deadline: Option<std::time::Instant>,
        stopper: Option<&'a StreamStopper>,
    ) -> Self {
        Self {
            stream_uptime,
            deadline,
            stopper,
        }
    }
}

pub(crate) async fn handle_stream_failure(
    account: &str,
    err: TeleError,
    failures: &mut u32,
    ctx: StreamFailureCtx<'_>,
    max_attempts: u32,
) -> TeleResult<()> {
    let StreamFailureCtx {
        stream_uptime,
        deadline,
        stopper,
    } = ctx;
    if is_auth_error(&err) {
        return Err(err);
    }
    if stopper.is_some_and(|s| s.stop_requested() || s.time_up()) {
        return Ok(());
    }
    *failures = on_failure(streak_before_failure(*failures, stream_uptime));
    if *failures > max_attempts {
        return Err(match err {
            TeleError::Rpc(msg, code, name, seconds) => TeleError::Rpc(
                format!("{account}: updates stream failed {failures} consecutive times, giving up: {msg}"),
                code,
                name,
                seconds,
            ),
            other => TeleError::Other(format!(
                "{account}: updates stream failed {failures} consecutive times, giving up: {}",
                other.message()
            )),
        });
    }
    let base_delay = next_delay(*failures);
    let delay = match flood_wait_secs(&err) {
        Some(wait) => base_delay.max(wait),
        None => base_delay,
    };
    let sleep_for = match deadline {
        Some(d) => delay.min(d.saturating_duration_since(std::time::Instant::now())),
        None => delay,
    };
    if let Some(wait) = flood_wait_secs(&err) {
        if let Some(d) = deadline {
            if wait > d.saturating_duration_since(std::time::Instant::now()) {
                return Err(err);
            }
        }
    }
    output::log_line(
        "error",
        &reconnect_message(
            account,
            *failures,
            sleep_for.as_secs() as u32,
            &err.message(),
        ),
    );
    match stopper {
        Some(stopper) => {
            // A backoff or FloodWait sleep must not outlive a stop that another
            // account's last row already triggered; without this the run stays
            // alive for the whole wait (FloodWait can be an hour).
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                _ = stopper.wait_for_stop() => {}
            }
        }
        None => tokio::time::sleep(sleep_for).await,
    }
    Ok(())
}

fn is_auth_error(e: &TeleError) -> bool {
    matches!(e, TeleError::Auth(_))
}

pub(crate) fn getstate_probe_error(e: grammers_client::InvocationError) -> TeleError {
    if crate::error::invocation_is_unauthorized(&e) {
        crate::error::invocation_error(e)
    } else {
        match &e {
            grammers_client::InvocationError::Rpc(_) => {
                let mut err = crate::error::invocation_error(e);
                if let TeleError::Rpc(msg, _, _, _) = &mut err {
                    *msg = format!("initial GetState failed: {msg}");
                }
                err
            }
            other => TeleError::Other(format!("initial GetState failed: {other}")),
        }
    }
}

fn next_delay(attempt: u32) -> std::time::Duration {
    let secs = if attempt == 0 {
        0
    } else {
        (1u32 << (attempt - 1).min(5)).min(MAX_RECONNECT_BACKOFF)
    };
    std::time::Duration::from_secs(secs.into())
}

pub(crate) fn update_peer(u: &tl::enums::Update) -> Option<PeerId> {
    match u {
        tl::enums::Update::NewMessage(x) => message_peer(&x.message),
        tl::enums::Update::NewChannelMessage(x) => message_peer(&x.message),
        tl::enums::Update::EditMessage(x) => message_peer(&x.message),
        tl::enums::Update::EditChannelMessage(x) => message_peer(&x.message),
        tl::enums::Update::DeleteChannelMessages(x) => PeerId::channel(x.channel_id),
        _ => None,
    }
}

fn message_peer(msg: &tl::enums::Message) -> Option<PeerId> {
    match msg {
        tl::enums::Message::Message(m) => Some(PeerId::from(&m.peer_id)),
        tl::enums::Message::Service(m) => Some(PeerId::from(&m.peer_id)),
        tl::enums::Message::Empty(_) => None,
    }
}

fn is_empty_message(msg: &tl::enums::Message) -> bool {
    matches!(msg, tl::enums::Message::Empty(_))
}

pub(crate) fn is_empty_update(u: &tl::enums::Update) -> bool {
    match u {
        tl::enums::Update::NewMessage(x) => is_empty_message(&x.message),
        tl::enums::Update::NewChannelMessage(x) => is_empty_message(&x.message),
        tl::enums::Update::EditMessage(x) => is_empty_message(&x.message),
        tl::enums::Update::EditChannelMessage(x) => is_empty_message(&x.message),
        _ => false,
    }
}

fn deleted_matches(bare_id: Option<i64>, resolved: PeerId) -> bool {
    if resolved.kind() == PeerKind::User {
        return false;
    }
    match (bare_id, resolved.bare_id()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
fn reconnect_allowed(consecutive_failures: u32) -> bool {
    consecutive_failures <= MAX_RECONNECT_ATTEMPTS
}

fn on_failure(failures: u32) -> u32 {
    failures + 1
}

// A connection that stayed up for a minute was healthy; its later death is a
// new outage, not another consecutive connect failure. Without this, a quiet
// account accumulates one failure per network blip over days and gives up
// while it is serving fine.
const HEALTHY_STREAM_UPTIME: std::time::Duration = std::time::Duration::from_secs(60);

/// An established connection is not proof the stream works: a stream that
/// connects and immediately errors must not have its streak reset.
fn streak_before_failure(failures: u32, stream_uptime: std::time::Duration) -> u32 {
    if stream_uptime >= HEALTHY_STREAM_UPTIME {
        0
    } else {
        failures
    }
}

/// Only a delivered, non-empty update proves the stream works. Empty updates
/// are tombstones, so treating them as delivery would let a connect-then-drop
/// loop never reach the give-up limit.
pub(crate) fn streak_after_update(failures: u32, empty: bool) -> u32 {
    if empty {
        failures
    } else {
        0
    }
}

#[cfg(test)]
fn give_up(failures: u32) -> bool {
    !reconnect_allowed(failures)
}

fn reconnect_message(account: &str, failures: u32, backoff: u32, cause: &str) -> String {
    format!(
        "{account}: updates stream error ({cause}), reconnecting (attempt {failures}/{MAX_RECONNECT_ATTEMPTS}) in {backoff}s"
    )
}

pub(crate) fn poll_timeout(
    deadline: Option<std::time::Instant>,
    now: std::time::Instant,
) -> std::time::Duration {
    match deadline {
        Some(d) => std::time::Duration::from_secs(3600).min(d.saturating_duration_since(now)),
        None => std::time::Duration::from_secs(3600),
    }
}

fn dry_run_row(events: &[String], account: &str) -> serde_json::Value {
    let label = events.join(",");
    let would = format!("stream {label} updates from account {account}");
    event_row(
        &label,
        account,
        None,
        None,
        Some(serde_json::json!({
            "dry_run": true,
            "would": would,
        })),
    )
}

pub(crate) fn event_row(
    event: &str,
    account: &str,
    chat_id: Option<i64>,
    ids: Option<&[i32]>,
    message: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut row = match message {
        Some(value) => value.as_object().cloned().unwrap_or_default(),
        None => serde_json::Map::new(),
    };
    row.insert("event".into(), serde_json::Value::from(event));
    row.insert("account".into(), serde_json::Value::from(account));
    if let Some(chat_id) = chat_id {
        row.insert("chat_id".into(), serde_json::json!(chat_id));
    }
    if let Some(ids) = ids {
        row.insert("ids".into(), serde_json::json!(ids));
    }
    serde_json::Value::Object(row)
}

fn raw_row(account: &str, raw: &tl::enums::Update, state: &State) -> serde_json::Value {
    let mut row = event_row("Raw", account, None, None, None);
    if let serde_json::Value::Object(map) = &mut row {
        map.insert(
            "raw".into(),
            serde_json::Value::from({
                use base64::engine::general_purpose::STANDARD;
                use base64::Engine;
                STANDARD.encode(raw.to_bytes())
            }),
        );
        map.insert("state".into(), state_to_json(state));
    }
    row
}

fn state_to_json(state: &State) -> serde_json::Value {
    use grammers_session::updates::MessageBox;
    let mut out = serde_json::Map::new();
    out.insert("date".into(), serde_json::json!(state.date));
    out.insert("seq".into(), serde_json::json!(state.seq));
    match &state.message_box {
        Some(MessageBox::Common { pts }) => {
            out.insert("pts".into(), serde_json::json!(pts));
        }
        Some(MessageBox::Secondary { qts }) => {
            out.insert("qts".into(), serde_json::json!(qts));
        }
        Some(MessageBox::Channel { channel_id, pts }) => {
            out.insert("channel_id".into(), serde_json::json!(channel_id));
            out.insert("pts".into(), serde_json::json!(pts));
        }
        None => {}
    }
    serde_json::Value::Object(out)
}

fn streamed_message_row(m: &grammers_client::message::Message) -> TeleResult<serde_json::Value> {
    let mut row = crate::serialize::message_to_json(m)?;
    crate::serialize::enrich_message_row(&mut row, m);
    Ok(row)
}

fn message_event_applies(
    new_message_on: bool,
    album_on: bool,
    service_on: bool,
    service_flavored: bool,
) -> bool {
    new_message_on || album_on || (service_on && service_flavored)
}

fn routes_to_service(service_on: bool, service_flavored: bool) -> bool {
    service_on && service_flavored
}

fn service_row(
    account: &str,
    chat_id: Option<i64>,
    base: serde_json::Value,
    action: &tl::enums::MessageAction,
) -> serde_json::Value {
    let mut row = event_row("Service", account, chat_id, None, Some(base));
    let (kind, label) = message_action_kind_label(action);
    if let serde_json::Value::Object(map) = &mut row {
        map.insert(
            "service_action".into(),
            serde_json::json!({ "kind": kind, "label": label }),
        );
    }
    row
}

fn chat_action_row(
    account: &str,
    raw: &tl::enums::Update,
) -> Option<(Option<PeerId>, Option<PeerId>, serde_json::Value)> {
    let build = |peer: Option<PeerId>,
                 emit_chat_id: Option<i64>,
                 sender: Option<PeerId>,
                 user_id: Option<i64>,
                 action: &tl::enums::SendMessageAction| {
        let (kind, label) = typing_action_kind_label(action);
        let mut row = event_row("ChatAction", account, None, None, None);
        if let serde_json::Value::Object(map) = &mut row {
            if let Some(chat_id) = emit_chat_id {
                map.insert("chat_id".into(), serde_json::json!(chat_id));
            }
            if let Some(user_id) = user_id {
                map.insert("user_id".into(), serde_json::json!(user_id));
            }
            map.insert(
                "action".into(),
                serde_json::json!({ "kind": kind, "label": label }),
            );
        }
        (peer, sender, row)
    };
    let sender_user_id = |from_id: &tl::enums::Peer| match from_id {
        tl::enums::Peer::User(user) => Some(user.user_id),
        _ => None,
    };
    match raw {
        tl::enums::Update::UserTyping(t) => {
            let peer = PeerId::user(t.user_id);
            Some(build(peer, None, peer, Some(t.user_id), &t.action))
        }
        tl::enums::Update::ChatUserTyping(t) => {
            let chat = PeerId::chat(t.chat_id);
            let sender = PeerId::from(&t.from_id);
            let emit = chat.and_then(|p| p.bare_id());
            Some(build(
                chat,
                emit,
                Some(sender),
                sender_user_id(&t.from_id),
                &t.action,
            ))
        }
        tl::enums::Update::ChannelUserTyping(t) => {
            let chat = PeerId::channel(t.channel_id);
            let sender = PeerId::from(&t.from_id);
            let emit = chat.and_then(|p| p.bare_id());
            Some(build(
                chat,
                emit,
                Some(sender),
                sender_user_id(&t.from_id),
                &t.action,
            ))
        }
        _ => None,
    }
}

fn user_update_row(
    account: &str,
    raw: &tl::enums::Update,
) -> Option<(Option<PeerId>, Option<PeerId>, serde_json::Value)> {
    match raw {
        tl::enums::Update::UserStatus(u) => {
            let peer = PeerId::user(u.user_id);
            let mut row = event_row("UserUpdate", account, None, None, None);
            if let serde_json::Value::Object(map) = &mut row {
                map.insert("user_id".into(), serde_json::json!(u.user_id));
                map.insert("status".into(), user_status_json(&u.status));
            }
            Some((peer, peer, row))
        }
        _ => None,
    }
}

fn callback_query_row(
    account: &str,
    raw: &tl::enums::Update,
) -> Option<(Option<PeerId>, Option<PeerId>, serde_json::Value)> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let build = |peer: Option<PeerId>,
                 sender: Option<PeerId>,
                 user_id: Option<i64>,
                 chat_id: Option<i64>,
                 msg_id: Option<i32>,
                 data: &[u8]| {
        let mut row = event_row("CallbackQuery", account, None, None, None);
        if let serde_json::Value::Object(map) = &mut row {
            if let Some(user_id) = user_id {
                map.insert("user_id".into(), serde_json::json!(user_id));
            }
            if let Some(chat_id) = chat_id {
                map.insert("chat_id".into(), serde_json::json!(chat_id));
            }
            if let Some(msg_id) = msg_id {
                map.insert("message_id".into(), serde_json::json!(msg_id));
            }
            map.insert(
                "data".into(),
                serde_json::json!(String::from_utf8_lossy(data)),
            );
            map.insert("data_b64".into(), serde_json::json!(STANDARD.encode(data)));
        }
        (peer, sender, row)
    };
    match raw {
        tl::enums::Update::BotCallbackQuery(q) => {
            let peer = PeerId::from(&q.peer);
            let chat_id = peer.bare_id();
            let sender = PeerId::user(q.user_id);
            Some(build(
                Some(peer),
                sender,
                Some(q.user_id),
                chat_id,
                Some(q.msg_id),
                q.data.as_deref().unwrap_or_default(),
            ))
        }
        tl::enums::Update::InlineBotCallbackQuery(q) => {
            let sender = PeerId::user(q.user_id);
            Some(build(
                sender,
                sender,
                Some(q.user_id),
                None,
                None,
                q.data.as_deref().unwrap_or_default(),
            ))
        }
        _ => None,
    }
}

fn user_status_json(status: &tl::enums::UserStatus) -> serde_json::Value {
    let (kind, label) = user_status_kind_label(status);
    let mut out = serde_json::Map::new();
    out.insert("kind".into(), serde_json::json!(kind));
    out.insert("label".into(), serde_json::json!(label));
    match status {
        tl::enums::UserStatus::Online(t) => {
            out.insert("expires".into(), serde_json::json!(t.expires));
        }
        tl::enums::UserStatus::Offline(t) => {
            out.insert("was_online".into(), serde_json::json!(t.was_online));
        }
        _ => {}
    }
    serde_json::Value::Object(out)
}

fn user_status_kind_label(status: &tl::enums::UserStatus) -> (&'static str, &'static str) {
    match status {
        tl::enums::UserStatus::Empty => ("userStatusEmpty", "empty"),
        tl::enums::UserStatus::Online(_) => ("userStatusOnline", "online"),
        tl::enums::UserStatus::Offline(_) => ("userStatusOffline", "offline"),
        tl::enums::UserStatus::Recently(_) => ("userStatusRecently", "recently"),
        tl::enums::UserStatus::LastWeek(_) => ("userStatusLastWeek", "last-week"),
        tl::enums::UserStatus::LastMonth(_) => ("userStatusLastMonth", "last-month"),
    }
}

fn typing_action_kind_label(action: &tl::enums::SendMessageAction) -> (&'static str, &'static str) {
    let kind = match action {
        tl::enums::SendMessageAction::SendMessageTypingAction => "sendMessageTypingAction",
        tl::enums::SendMessageAction::SendMessageCancelAction => "sendMessageCancelAction",
        tl::enums::SendMessageAction::SendMessageRecordVideoAction => {
            "sendMessageRecordVideoAction"
        }
        tl::enums::SendMessageAction::SendMessageUploadVideoAction(_) => {
            "sendMessageUploadVideoAction"
        }
        tl::enums::SendMessageAction::SendMessageRecordAudioAction => {
            "sendMessageRecordAudioAction"
        }
        tl::enums::SendMessageAction::SendMessageUploadAudioAction(_) => {
            "sendMessageUploadAudioAction"
        }
        tl::enums::SendMessageAction::SendMessageUploadPhotoAction(_) => {
            "sendMessageUploadPhotoAction"
        }
        tl::enums::SendMessageAction::SendMessageUploadDocumentAction(_) => {
            "sendMessageUploadDocumentAction"
        }
        tl::enums::SendMessageAction::SendMessageGeoLocationAction => {
            "sendMessageGeoLocationAction"
        }
        tl::enums::SendMessageAction::SendMessageChooseContactAction => {
            "sendMessageChooseContactAction"
        }
        tl::enums::SendMessageAction::SendMessageGamePlayAction => "sendMessageGamePlayAction",
        tl::enums::SendMessageAction::SendMessageRecordRoundAction => {
            "sendMessageRecordRoundAction"
        }
        tl::enums::SendMessageAction::SendMessageUploadRoundAction(_) => {
            "sendMessageUploadRoundAction"
        }
        tl::enums::SendMessageAction::SpeakingInGroupCallAction => "speakingInGroupCallAction",
        tl::enums::SendMessageAction::SendMessageHistoryImportAction(_) => {
            "sendMessageHistoryImportAction"
        }
        tl::enums::SendMessageAction::SendMessageChooseStickerAction => {
            "sendMessageChooseStickerAction"
        }
        tl::enums::SendMessageAction::SendMessageEmojiInteraction(_) => {
            "sendMessageEmojiInteraction"
        }
        tl::enums::SendMessageAction::SendMessageEmojiInteractionSeen(_) => {
            "sendMessageEmojiInteractionSeen"
        }
        tl::enums::SendMessageAction::SendMessageTextDraftAction(_) => "sendMessageTextDraftAction",
        tl::enums::SendMessageAction::InputSendMessageRichMessageDraftAction(_) => {
            "inputSendMessageRichMessageDraftAction"
        }
        tl::enums::SendMessageAction::SendMessageRichMessageDraftAction(_) => {
            "sendMessageRichMessageDraftAction"
        }
    };
    let label = match kind {
        "sendMessageTypingAction" => "typing",
        _ => kind,
    };
    (kind, label)
}

fn message_action_kind_label(action: &tl::enums::MessageAction) -> (&'static str, &'static str) {
    let kind = message_action_kind(action);
    let label = match kind {
        "messageActionChatAddUser" | "messageActionInviteToGroupCall" => "join-invite",
        "messageActionChatJoinedByLink" | "messageActionChatJoinedByRequest" => "join",
        "messageActionChatDeleteUser" => "leave",
        "messageActionPinMessage" => "pin",
        _ => kind,
    };
    (kind, label)
}

fn message_action_kind(action: &tl::enums::MessageAction) -> &'static str {
    match action {
        tl::enums::MessageAction::Empty => "messageActionEmpty",
        tl::enums::MessageAction::ChatCreate(_) => "messageActionChatCreate",
        tl::enums::MessageAction::ChatEditTitle(_) => "messageActionChatEditTitle",
        tl::enums::MessageAction::ChatEditPhoto(_) => "messageActionChatEditPhoto",
        tl::enums::MessageAction::ChatDeletePhoto => "messageActionChatDeletePhoto",
        tl::enums::MessageAction::ChatAddUser(_) => "messageActionChatAddUser",
        tl::enums::MessageAction::ChatDeleteUser(_) => "messageActionChatDeleteUser",
        tl::enums::MessageAction::ChatJoinedByLink(_) => "messageActionChatJoinedByLink",
        tl::enums::MessageAction::ChannelCreate(_) => "messageActionChannelCreate",
        tl::enums::MessageAction::ChatMigrateTo(_) => "messageActionChatMigrateTo",
        tl::enums::MessageAction::ChannelMigrateFrom(_) => "messageActionChannelMigrateFrom",
        tl::enums::MessageAction::PinMessage => "messageActionPinMessage",
        tl::enums::MessageAction::HistoryClear => "messageActionHistoryClear",
        tl::enums::MessageAction::GameScore(_) => "messageActionGameScore",
        tl::enums::MessageAction::PaymentSentMe(_) => "messageActionPaymentSentMe",
        tl::enums::MessageAction::PaymentSent(_) => "messageActionPaymentSent",
        tl::enums::MessageAction::PhoneCall(_) => "messageActionPhoneCall",
        tl::enums::MessageAction::ScreenshotTaken => "messageActionScreenshotTaken",
        tl::enums::MessageAction::CustomAction(_) => "messageActionCustomAction",
        tl::enums::MessageAction::BotAllowed(_) => "messageActionBotAllowed",
        tl::enums::MessageAction::SecureValuesSentMe(_) => "messageActionSecureValuesSentMe",
        tl::enums::MessageAction::SecureValuesSent(_) => "messageActionSecureValuesSent",
        tl::enums::MessageAction::ContactSignUp => "messageActionContactSignUp",
        tl::enums::MessageAction::GeoProximityReached(_) => "messageActionGeoProximityReached",
        tl::enums::MessageAction::GroupCall(_) => "messageActionGroupCall",
        tl::enums::MessageAction::InviteToGroupCall(_) => "messageActionInviteToGroupCall",
        tl::enums::MessageAction::SetMessagesTtl(_) => "messageActionSetMessagesTTL",
        tl::enums::MessageAction::GroupCallScheduled(_) => "messageActionGroupCallScheduled",
        tl::enums::MessageAction::SetChatTheme(_) => "messageActionSetChatTheme",
        tl::enums::MessageAction::ChatJoinedByRequest => "messageActionChatJoinedByRequest",
        tl::enums::MessageAction::WebViewDataSentMe(_) => "messageActionWebViewDataSentMe",
        tl::enums::MessageAction::WebViewDataSent(_) => "messageActionWebViewDataSent",
        tl::enums::MessageAction::GiftPremium(_) => "messageActionGiftPremium",
        tl::enums::MessageAction::TopicCreate(_) => "messageActionTopicCreate",
        tl::enums::MessageAction::TopicEdit(_) => "messageActionTopicEdit",
        tl::enums::MessageAction::SuggestProfilePhoto(_) => "messageActionSuggestProfilePhoto",
        tl::enums::MessageAction::RequestedPeer(_) => "messageActionRequestedPeer",
        tl::enums::MessageAction::SetChatWallPaper(_) => "messageActionSetChatWallPaper",
        tl::enums::MessageAction::GiftCode(_) => "messageActionGiftCode",
        tl::enums::MessageAction::GiveawayLaunch(_) => "messageActionGiveawayLaunch",
        tl::enums::MessageAction::GiveawayResults(_) => "messageActionGiveawayResults",
        tl::enums::MessageAction::BoostApply(_) => "messageActionBoostApply",
        tl::enums::MessageAction::RequestedPeerSentMe(_) => "messageActionRequestedPeerSentMe",
        tl::enums::MessageAction::PaymentRefunded(_) => "messageActionPaymentRefunded",
        tl::enums::MessageAction::GiftStars(_) => "messageActionGiftStars",
        tl::enums::MessageAction::PrizeStars(_) => "messageActionPrizeStars",
        tl::enums::MessageAction::StarGift(_) => "messageActionStarGift",
        tl::enums::MessageAction::StarGiftUnique(_) => "messageActionStarGiftUnique",
        tl::enums::MessageAction::PaidMessagesRefunded(_) => "messageActionPaidMessagesRefunded",
        tl::enums::MessageAction::PaidMessagesPrice(_) => "messageActionPaidMessagesPrice",
        tl::enums::MessageAction::ConferenceCall(_) => "messageActionConferenceCall",
        tl::enums::MessageAction::TodoCompletions(_) => "messageActionTodoCompletions",
        tl::enums::MessageAction::TodoAppendTasks(_) => "messageActionTodoAppendTasks",
        tl::enums::MessageAction::SuggestedPostApproval(_) => "messageActionSuggestedPostApproval",
        tl::enums::MessageAction::SuggestedPostSuccess(_) => "messageActionSuggestedPostSuccess",
        tl::enums::MessageAction::SuggestedPostRefund(_) => "messageActionSuggestedPostRefund",
        tl::enums::MessageAction::GiftTon(_) => "messageActionGiftTon",
        tl::enums::MessageAction::SuggestBirthday(_) => "messageActionSuggestBirthday",
        tl::enums::MessageAction::StarGiftPurchaseOffer(_) => "messageActionStarGiftPurchaseOffer",
        tl::enums::MessageAction::StarGiftPurchaseOfferDeclined(_) => {
            "messageActionStarGiftPurchaseOfferDeclined"
        }
        tl::enums::MessageAction::NewCreatorPending(_) => "messageActionNewCreatorPending",
        tl::enums::MessageAction::ChangeCreator(_) => "messageActionChangeCreator",
        tl::enums::MessageAction::NoForwardsToggle(_) => "messageActionNoForwardsToggle",
        tl::enums::MessageAction::NoForwardsRequest(_) => "messageActionNoForwardsRequest",
        tl::enums::MessageAction::PollAppendAnswer(_) => "messageActionPollAppendAnswer",
        tl::enums::MessageAction::PollDeleteAnswer(_) => "messageActionPollDeleteAnswer",
        tl::enums::MessageAction::ManagedBotCreated(_) => "messageActionManagedBotCreated",
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
mod stopper_tests {
    use super::{
        album_ingest, emit_due_albums, emit_raw_update, emit_row_or_stop, handle_stream_failure,
        on_emit_failure, AlbumBuffer, CountSlot, EmitFailure, EventFilter, ListenWriter,
        RawEmitCtx, StreamFailureCtx, StreamStopper,
    };
    use crate::error::TeleError;
    use grammers_client::tl;
    use grammers_session::updates::State;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CapturedLines(Arc<Mutex<Vec<u8>>>);

    impl CapturedLines {
        fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl std::io::Write for CapturedLines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct BrokenPipeSink;

    impl std::io::Write for BrokenPipeSink {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct GatedSink {
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        out: CapturedLines,
    }

    impl std::io::Write for GatedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.gate;
            let mut open = lock.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = cv.wait(open).unwrap_or_else(|e| e.into_inner());
            }
            drop(open);
            self.out.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn test_writer(capacity: usize, wait: std::time::Duration) -> (ListenWriter, CapturedLines) {
        let sink = CapturedLines::default();
        let writer = super::spawn_writer(
            {
                let sink = sink.clone();
                move || sink
            },
            capacity,
            wait,
        );
        (writer, sink)
    }

    fn release_gate(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
        let (lock, cv) = &**gate;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    /// Wait until the writer thread has dequeued the queued row, so the next
    /// send faces a free slot instead of racing a 200ms budget.
    async fn wait_until_writer_took_the_queued_row(writer: &ListenWriter) {
        for _ in 0..400 {
            if writer.tx.capacity() > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the writer thread never dequeued the queued row");
    }

    // These tests hold FIELDS_TEST_LOCK across awaits to keep OUTPUT_FIELDS
    // stable; the writer thread never takes that lock. Each caller carries
    // the matching allow so a future async test is still flagged.
    fn quiet_fields() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::output::lock_fields_for_test();
        crate::output::clear_output_fields();
        guard
    }

    #[test]
    fn count_zero_is_exhausted_before_any_emit() {
        assert!(StreamStopper::new(Some(0), None, None).is_exhausted());
    }

    #[test]
    fn count_none_never_exhausts() {
        let stopper = StreamStopper::new(None, None, None);
        assert!(!stopper.is_exhausted());
        assert!(!stopper.emitted());
        assert!(!stopper.is_exhausted());
    }

    #[test]
    fn count_n_exhausts_after_n_emits() {
        let stopper = StreamStopper::new(Some(2), None, None);
        assert!(!stopper.is_exhausted());
        assert!(!stopper.emitted());
        assert!(!stopper.is_exhausted());
        assert!(stopper.emitted());
        assert!(stopper.is_exhausted());
    }

    #[test]
    fn past_until_date_stops_on_first_row_at_or_after_it() {
        // Regression: --until used to be a wall-clock deadline, so a past
        // timestamp expired before the first event and the stream exited
        // with zero rows instead of replaying backlog dated before the
        // timestamp.
        let past = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stopper = StreamStopper::new(None, None, Some(past));
        let backlog_row = serde_json::json!({"date": "2025-06-01T00:00:00+00:00"});
        let future_row = serde_json::json!({"date": "2026-06-01T00:00:00+00:00"});
        assert!(
            !stopper.past_until(&backlog_row),
            "backlog dated before --until must stream, not be cut off"
        );
        assert!(
            stopper.past_until(&future_row),
            "first event at/after --until must stop the stream"
        );
    }

    #[test]
    fn raw_and_gap_rows_match_on_state_date() {
        let limit = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stopper = StreamStopper::new(None, None, Some(limit));
        let before = serde_json::json!({"event": "Raw", "state": {"date": 1_700_000_000i64}});
        let after = serde_json::json!({"event": "Raw", "state": {"date": 2_000_000_000i64}});
        assert!(!stopper.past_until(&before));
        assert!(stopper.past_until(&after));
    }

    fn raw_new_message_update(id: i32, date: i32) -> tl::enums::Update {
        tl::enums::Update::NewMessage(tl::types::UpdateNewMessage {
            message: tl::enums::Message::Message(tl::types::Message {
                out: false,
                mentioned: false,
                media_unread: false,
                silent: false,
                post: false,
                from_scheduled: false,
                legacy: false,
                edit_hide: false,
                pinned: false,
                noforwards: false,
                invert_media: false,
                offline: false,
                video_processing_pending: false,
                paid_suggested_post_stars: false,
                paid_suggested_post_ton: false,
                id,
                from_id: None,
                from_boosts_applied: None,
                from_rank: None,
                peer_id: tl::enums::Peer::User(tl::types::PeerUser { user_id: 7 }),
                saved_peer_id: None,
                fwd_from: None,
                via_bot_id: None,
                via_business_bot_id: None,
                guestchat_via_from: None,
                reply_to: None,
                date,
                message: "raw hello".to_string(),
                media: None,
                reply_markup: None,
                entities: None,
                views: None,
                forwards: None,
                replies: None,
                edit_date: None,
                post_author: None,
                grouped_id: None,
                reactions: None,
                restriction_reason: None,
                ttl_period: None,
                quick_reply_shortcut_id: None,
                effect: None,
                factcheck: None,
                report_delivery_until_date: None,
                paid_message_stars: None,
                suggested_post: None,
                schedule_repeat_period: None,
                summary_from_language: None,
                rich_message: None,
            }),
            pts: 10,
            pts_count: 1,
        })
    }

    fn raw_state(date: i32) -> State {
        State {
            date,
            seq: 1,
            message_box: None,
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn raw_events_cover_message_updates_without_a_parsed_family() {
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(None, None, None);
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let mut album = AlbumBuffer::new();
        let update = raw_new_message_update(42, 1_700_000_000);
        let state = raw_state(1_700_000_000);
        let stopped = emit_raw_update(
            &writer,
            &stopper,
            "work",
            &mut album,
            RawEmitCtx {
                filter: &EventFilter::default(),
                raw_on: true,
                update: &update,
                state: &state,
            },
        )
        .await
        .unwrap();
        assert!(!stopped);
        writer.finish().await;
        let lines = sink.lines();
        assert_eq!(
            lines.len(),
            1,
            "Raw must be emitted with no parsed event family selected"
        );
        let row: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(row["event"], "Raw");
        assert_eq!(row["state"]["date"], 1_700_000_000);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn raw_events_follow_the_raw_allowlist() {
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(None, None, None);
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let mut album = AlbumBuffer::new();
        let update = raw_new_message_update(43, 1_700_000_000);
        let state = raw_state(1_700_000_000);
        let open = EventFilter::default();
        let filtered = EventFilter {
            patterns: vec![regex::Regex::new("nope").unwrap()],
            ..EventFilter::default()
        };
        for (raw_on, filter) in [(false, &open), (true, &filtered)] {
            assert!(!emit_raw_update(
                &writer,
                &stopper,
                "work",
                &mut album,
                RawEmitCtx {
                    filter,
                    raw_on,
                    update: &update,
                    state: &state,
                }
            )
            .await
            .unwrap());
        }
        let empty = tl::enums::Update::NewMessage(tl::types::UpdateNewMessage {
            message: tl::enums::Message::Empty(tl::types::MessageEmpty {
                id: 44,
                peer_id: None,
            }),
            pts: 11,
            pts_count: 1,
        });
        assert!(!emit_raw_update(
            &writer,
            &stopper,
            "work",
            &mut album,
            RawEmitCtx {
                filter: &open,
                raw_on: true,
                update: &empty,
                state: &state,
            }
        )
        .await
        .unwrap());
        let other_chat = EventFilter {
            chats: vec![super::PeerId::user(8).unwrap()],
            ..EventFilter::default()
        };
        assert!(!emit_raw_update(
            &writer,
            &stopper,
            "work",
            &mut album,
            RawEmitCtx {
                filter: &other_chat,
                raw_on: true,
                update: &update,
                state: &state,
            }
        )
        .await
        .unwrap());
        writer.finish().await;
        assert!(
            sink.lines().is_empty(),
            "suppressed raw updates must not reach stdout"
        );
    }

    #[test]
    fn rows_without_any_date_never_stop_the_stream() {
        // ChatAction/UserUpdate/CallbackQuery rows carry no date at all, so
        // they can never match a date filter. They must not stop the run;
        // --timeout-secs bounds it instead.
        let limit = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stopper = StreamStopper::new(None, None, Some(limit));
        let undated = serde_json::json!({"event": "ChatAction"});
        assert!(!stopper.past_until(&undated));
        assert!(!stopper.time_up());
    }

    #[tokio::test]
    async fn emit_row_or_stop_stops_on_a_dated_row_at_or_after_the_limit() {
        // The wiring, not just the predicate: a past --until must not stop
        // the stream before the row is written.
        let limit = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stopper = StreamStopper::new(None, None, Some(limit));
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let row = serde_json::json!({"event": "NewMessage", "date": "2026-06-01T00:00:00+00:00"});
        assert!(
            super::emit_row_or_stop(&writer, &stopper, "work", row)
                .await
                .unwrap()
                .stop
        );
        assert!(stopper.stop_requested());
        writer.finish().await;
        assert!(
            sink.lines().is_empty(),
            "the row that reaches --until must not be written"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_dated_until_stop_reports_once() {
        let _lock = quiet_fields();
        let limit = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stopper = StreamStopper::new(None, None, Some(limit));
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let row = serde_json::json!({"event": "NewMessage", "date": "2026-06-01T00:00:00+00:00"});
        assert!(
            emit_row_or_stop(&writer, &stopper, "work", row)
                .await
                .unwrap()
                .stop
        );
        assert!(
            !stopper.report_until_reached(),
            "the row path must claim the one-shot notice, so the end-of-run check stays silent"
        );
        writer.finish().await;
        assert!(sink.lines().is_empty());
    }

    #[tokio::test]
    async fn flood_wait_longer_than_deadline_fails_instead_of_reconnecting_early() {
        let flood = crate::error::TeleError::Rpc(
            "FLOOD_WAIT".to_string(),
            420,
            "FLOOD_WAIT".to_string(),
            Some(60),
        );
        let deadline = Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
        let mut failures = 0u32;
        let start = std::time::Instant::now();
        let out = super::handle_stream_failure(
            "probe",
            flood,
            &mut failures,
            StreamFailureCtx::before_connect(deadline, None),
            5,
        )
        .await;
        assert!(
            out.is_err(),
            "must surface FloodWait, not retry while banned"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "must not sleep out the 60s wait"
        );
    }

    #[tokio::test]
    async fn short_flood_wait_still_backs_off_and_retries() {
        let flood = crate::error::TeleError::Rpc(
            "FLOOD_WAIT".to_string(),
            420,
            "FLOOD_WAIT".to_string(),
            Some(1),
        );
        let deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        let mut failures = 0u32;
        let out = super::handle_stream_failure(
            "probe",
            flood,
            &mut failures,
            StreamFailureCtx::before_connect(deadline, None),
            5,
        )
        .await;
        assert!(out.is_ok());
        assert_eq!(failures, 1);
    }

    #[tokio::test]
    async fn count_zero_emit_row_or_stop_emits_nothing() {
        let stopper = StreamStopper::new(Some(0), None, None);
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let row = serde_json::json!({"event": "NewMessage"});
        assert!(
            super::emit_row_or_stop(&writer, &stopper, "work", row)
                .await
                .unwrap()
                .stop
        );
        writer.finish().await;
        assert!(sink.lines().is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn consuming_the_last_count_slot_shares_the_stop() {
        // Regression: the account that emitted the Nth row returned without
        // telling the stopper, so every other account parked on its next
        // poll (up to 1h) instead of ending with the run.
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(Some(1), None, None);
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let row = serde_json::json!({"event": "NewMessage"});
        assert!(
            super::emit_row_or_stop(&writer, &stopper, "work", row)
                .await
                .unwrap()
                .stop
        );
        assert!(
            stopper.stop_requested(),
            "a spent --count budget must request a stop so parked accounts wake"
        );
        writer.finish().await;
        assert_eq!(sink.lines().len(), 1);
    }

    #[tokio::test]
    async fn an_already_spent_count_budget_stops_the_account_without_making_the_stop_permanent() {
        // The account that reserved the last slot can lose its write to a
        // dropped emit, which gives the slot back. An observer that saw
        // Exhausted must therefore stop itself without broadcasting a stop
        // that would end the run with the budget unspent.
        let stopper = StreamStopper::new(Some(0), None, None);
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let row = serde_json::json!({"event": "NewMessage"});
        assert!(
            super::emit_row_or_stop(&writer, &stopper, "work", row)
                .await
                .unwrap()
                .stop
        );
        assert!(
            !stopper.stop_requested(),
            "an observer of an exhausted budget must not make the stop permanent"
        );
        writer.finish().await;
        assert!(sink.lines().is_empty());
    }

    #[test]
    fn a_usage_failure_promotes_partial_runs_but_not_all_failed_precedence() {
        assert_eq!(
            super::listen_exit_code(true, 1, &[crate::error::EXIT_USAGE]),
            crate::error::EXIT_USAGE,
            "a --fields projection failure must exit 1 even after another account succeeded"
        );
        assert_eq!(
            super::listen_exit_code(
                true,
                0,
                &[crate::error::EXIT_USAGE, crate::error::EXIT_AUTH]
            ),
            crate::error::EXIT_AUTH,
            "when every account failed, auth must outrank the projection usage failure"
        );
        assert_eq!(
            super::listen_exit_code(
                true,
                0,
                &[crate::error::EXIT_USAGE, crate::error::EXIT_ALL_FAILED]
            ),
            crate::error::EXIT_ALL_FAILED,
            "when every account failed, a Telegram/IO failure must outrank the projection usage failure"
        );
        assert_eq!(
            super::listen_exit_code(true, 0, &[crate::error::EXIT_USAGE]),
            crate::error::EXIT_USAGE,
            "a projection failure as the only failure still exits 1"
        );
        assert_eq!(
            super::listen_exit_code(false, 1, &[crate::error::EXIT_USAGE]),
            crate::error::EXIT_PARTIAL,
            "a normal usage failure next to a success stays a partial exit"
        );
        assert_eq!(
            super::listen_exit_code(false, 0, &[crate::error::EXIT_USAGE]),
            crate::error::EXIT_USAGE,
            "usage failures alone still exit 1"
        );
        assert_eq!(
            super::listen_exit_code(false, 2, &[]),
            crate::error::EXIT_OK
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_stop_returns_promptly_after_a_request_from_another_task() {
        let stopper = Arc::new(StreamStopper::new(None, None, None));
        let waiter = tokio::spawn({
            let stopper = Arc::clone(&stopper);
            async move { stopper.wait_for_stop().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            !waiter.is_finished(),
            "wait_for_stop must park until a stop is requested"
        );
        stopper.request_stop();
        // Paused time means the 500ms fallback poll cannot fire inside this
        // one-millisecond window: only the notify can finish the waiter.
        tokio::time::timeout(std::time::Duration::from_millis(1), waiter)
            .await
            .expect("request_stop must wake a parked wait_for_stop, not wait for the poll")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn wait_for_stop_returns_when_the_until_deadline_passes() {
        // No store happens when a wall-clock deadline expires, so only the
        // bounded poll inside wait_for_stop can observe it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        let stopper = StreamStopper::new(None, Some(deadline), None);
        let start = std::time::Instant::now();
        tokio::time::timeout(std::time::Duration::from_secs(2), stopper.wait_for_stop())
            .await
            .expect("an expiring --until deadline must end the wait");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "a future deadline must actually be waited for, not returned from at entry: {elapsed:?}"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(1500),
            "the 500ms poll bounds the deadline latency, took {elapsed:?}"
        );
    }

    #[test]
    fn no_deadline_and_no_stop_means_time_is_not_up() {
        let stopper = StreamStopper::new(None, None, None);
        assert!(!stopper.time_up());
        assert!(!stopper.stop_requested());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::await_holding_lock)]
    async fn count_budget_is_reserved_before_the_emit_so_accounts_cannot_overshoot() {
        let _lock = quiet_fields();
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let stopper = Arc::new(StreamStopper::new(Some(5), None, None));
        let mut tasks = tokio::task::JoinSet::new();
        for account in 0..16 {
            let writer = writer.clone();
            let stopper = Arc::clone(&stopper);
            tasks.spawn(async move {
                let mut album = super::AlbumBuffer::new();
                let row = serde_json::json!({"event": "NewMessage", "account": account});
                super::emit_or_stop(&writer, &stopper, "acct", &mut album, row).await
            });
        }
        let mut stopped = 0;
        while let Some(joined) = tasks.join_next().await {
            if joined.expect("task").expect("emit") {
                stopped += 1;
            }
        }
        writer.finish().await;
        assert_eq!(
            sink.lines().len(),
            5,
            "--count must cap emitted rows even when every account races for the budget"
        );
        assert_eq!(
            stopped, 12,
            "one stop for the last slot, one per starved account"
        );
        assert!(matches!(stopper.reserve(), CountSlot::Exhausted));
    }

    #[test]
    fn connect_success_without_a_delivered_update_keeps_the_failure_streak() {
        let mut failures = 0;
        for _ in 0..=super::MAX_RECONNECT_ATTEMPTS {
            failures = super::streak_before_failure(failures, std::time::Duration::ZERO);
            assert!(
                super::reconnect_allowed(failures),
                "a connect that has not delivered an update must not reset the streak"
            );
            failures = super::on_failure(failures);
        }
        assert!(
            super::give_up(failures),
            "repeated connect-then-error cycles must reach the give-up limit"
        );
        assert_eq!(failures, super::MAX_RECONNECT_ATTEMPTS + 1);
        assert_eq!(
            super::streak_after_update(failures, true),
            failures,
            "an empty update is a tombstone, not delivery"
        );
        assert_eq!(
            super::streak_after_update(failures, false),
            0,
            "a delivered, non-empty update is what resets the streak"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_last_count_slot_wakes_a_parked_waiter() {
        // A flag assertion alone would pass even if the broadcast went
        // missing; the regression this guards is parked accounts waiting out
        // the 500ms poll on every other account.
        let _lock = quiet_fields();
        let stopper = Arc::new(StreamStopper::new(Some(1), None, None));
        let waiter = tokio::spawn({
            let stopper = Arc::clone(&stopper);
            async move { stopper.wait_for_stop().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter must be parked before the last slot is spent"
        );
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let step = super::emit_row_or_stop(
            &writer,
            &stopper,
            "work",
            serde_json::json!({"event": "NewMessage", "text": "last"}),
        )
        .await
        .unwrap();
        assert!(step.stop);
        tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("the last count slot must wake parked accounts without a poll delay")
            .expect("waiter task panicked");
        writer.finish().await;
        assert_eq!(sink.lines().len(), 1);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn writer_preserves_order_and_writes_each_row_once() {
        let _lock = quiet_fields();
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        for id in 0i64..200 {
            writer
                .emit("acct", serde_json::json!({"event": "NewMessage", "id": id}))
                .await
                .unwrap();
        }
        writer.finish().await;
        let lines = sink.lines();
        assert_eq!(lines.len(), 200, "every row must be written exactly once");
        for (expected, line) in lines.iter().enumerate() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["id"], serde_json::json!(expected as i64));
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn writer_surfaces_broken_pipe_instead_of_hanging_or_swallowing_it() {
        let _lock = quiet_fields();
        let writer =
            super::spawn_writer(|| BrokenPipeSink, 4, std::time::Duration::from_millis(200));
        let mut failure = None;
        for _ in 0..64 {
            match writer
                .emit("acct", serde_json::json!({"event": "NewMessage"}))
                .await
            {
                Ok(_) => continue,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        let err = failure.expect("the writer failure must reach the emitters");
        assert!(err.is_broken_pipe(), "err: {err}");
        assert!(
            super::emit_stops_stream(&err),
            "a broken pipe must still stop the stream"
        );
        assert_eq!(err.exit_code(), crate::error::EXIT_OK);
        writer.finish().await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_stalled_stdout_drops_the_row_counts_it_and_stamps_the_next_one() {
        let _lock = quiet_fields();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let sink = CapturedLines::default();
        let writer = super::spawn_writer(
            {
                let gate = Arc::clone(&gate);
                let sink = sink.clone();
                move || GatedSink { gate, out: sink }
            },
            1,
            std::time::Duration::from_millis(200),
        );
        writer
            .emit("acct", serde_json::json!({"event": "NewMessage", "id": 1}))
            .await
            .unwrap();
        wait_until_writer_took_the_queued_row(&writer).await;
        writer
            .emit("acct", serde_json::json!({"event": "NewMessage", "id": 2}))
            .await
            .unwrap();
        let dropped = writer
            .emit("acct", serde_json::json!({"event": "NewMessage", "id": 3}))
            .await
            .unwrap();
        assert!(
            matches!(dropped, super::EmitOutcome::Dropped),
            "a row that cannot be handed off must be dropped, not parked"
        );

        release_gate(&gate);
        writer
            .emit("acct", serde_json::json!({"event": "NewMessage", "id": 4}))
            .await
            .unwrap();
        writer.finish().await;

        let lines = sink.lines();
        assert_eq!(lines.len(), 3, "the dropped row must never reach stdout");
        let rows: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let ids: Vec<i64> = rows.iter().map(|row| row["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![1, 2, 4], "order must survive the drop");
        assert_eq!(
            rows.last().unwrap()["dropped"],
            1,
            "the loss must ride along with the next written row"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_broken_stdout_releases_the_slot_and_wakes_parked_accounts() {
        // Regression: the emit-error arms returned before the last-slot
        // broadcast, so a broken pipe spent the count budget with no wake-up
        // and quiet accounts waited out the next poll.
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(Some(3), None, None);
        let writer =
            super::spawn_writer(|| BrokenPipeSink, 1, std::time::Duration::from_millis(200));
        let first = emit_row_or_stop(
            &writer,
            &stopper,
            "acct",
            serde_json::json!({"event": "NewMessage"}),
        )
        .await
        .expect("the first row is queued before the writer sees the pipe");
        assert!(!first.stop);
        for _ in 0..400 {
            if writer.tx.is_closed() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            writer.tx.is_closed(),
            "the writer thread must exit on a broken pipe"
        );
        let second = emit_row_or_stop(
            &writer,
            &stopper,
            "acct",
            serde_json::json!({"event": "NewMessage"}),
        )
        .await
        .expect("a broken pipe is not a usage error");
        assert!(second.stop, "the emit failure must stop the stream");
        assert!(
            stopper.stop_requested(),
            "a dead stdout must broadcast a stop so parked accounts wake"
        );
        assert!(
            !stopper.usage_failure(),
            "a broken pipe must not force the usage exit code"
        );
        assert!(
            matches!(stopper.reserve(), CountSlot::Reserved { last: false }),
            "the failed row's slot must be returned, not silently spent"
        );
        assert!(
            matches!(stopper.reserve(), CountSlot::Reserved { last: true }),
            "one queued row and one returned slot leave two of three slots"
        );
        writer.finish().await;
    }

    #[test]
    fn a_usage_emit_failure_broadcasts_the_stop() {
        // --fields is one value for the whole run; a row shape without the
        // requested path makes the projection unusable, so stopping only this
        // task would leave the rest streaming shapes that cannot be projected.
        let stopper = StreamStopper::new(Some(1), None, None);
        let failure = on_emit_failure(
            &stopper,
            "acct",
            &TeleError::Usage("unknown --fields path".to_string()),
        );
        assert!(matches!(failure, EmitFailure::Usage));
        assert!(
            stopper.stop_requested(),
            "a projection failure must wake and end every account"
        );
        assert!(
            stopper.usage_failure(),
            "the run exit code must report the projection failure as usage"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn emit_direct_broadcasts_a_stop_on_a_dead_stdout() {
        // The stop-flush path writes without the count check, so it must not
        // silently spin on a dead writer either.
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(None, None, None);
        let writer =
            super::spawn_writer(|| BrokenPipeSink, 1, std::time::Duration::from_millis(200));
        let stopped = {
            let mut stopped = false;
            for _ in 0..64 {
                if super::emit_direct(
                    &writer,
                    &stopper,
                    "acct",
                    serde_json::json!({"event": "NewMessage"}),
                )
                .await
                .expect("a broken pipe is not a usage error")
                {
                    stopped = true;
                    break;
                }
            }
            stopped
        };
        assert!(stopped, "a dead stdout must end the flush");
        assert!(
            stopper.stop_requested(),
            "a dead stdout must wake parked accounts"
        );
        writer.finish().await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn emit_direct_projection_failure_marks_the_run_as_usage() {
        // A stop-flush row can be the first row missing the projected path.
        // It must take the same branch as a normal emit: broadcast the stop
        // and make the run exit 1, not fall through to a partial exit.
        let _lock = quiet_fields();
        let _fields = crate::output::FieldsTestGuard::set("text");
        let stopper = StreamStopper::new(None, None, None);
        let (writer, sink) = test_writer(4, std::time::Duration::from_millis(200));
        let err = super::emit_direct(
            &writer,
            &stopper,
            "acct",
            serde_json::json!({"event": "Album", "id": 1}),
        )
        .await
        .expect_err("a row without the projected path must fail");
        assert!(matches!(err, TeleError::Usage(_)));
        assert!(
            stopper.usage_failure(),
            "the run must exit as a usage failure"
        );
        assert!(
            stopper.stop_requested(),
            "the projection stop must wake parked accounts"
        );
        writer.finish().await;
        assert!(sink.lines().is_empty());
    }

    #[tokio::test]
    async fn a_failure_after_the_stop_was_requested_is_not_counted_or_slept_on() {
        let stopper = StreamStopper::new(None, None, None);
        stopper.request_stop();
        let mut failures = 2u32;
        handle_stream_failure(
            "acct",
            TeleError::Other("boom".into()),
            &mut failures,
            StreamFailureCtx::before_connect(None, Some(&stopper)),
            5,
        )
        .await
        .expect("a stopped run does not treat the failure as fatal");
        assert_eq!(failures, 2, "the stopped run must not grow the streak");
    }

    #[tokio::test]
    async fn a_stop_interrupts_the_reconnect_backoff_sleep() {
        // Regression: a finite run waited out the whole backoff (or FloodWait,
        // up to an hour) after another account had already ended the stream.
        // A one-hour FloodWait makes the stop the only way this returns inside
        // the timeout, so the test does not race the backoff clock.
        let stopper = Arc::new(StreamStopper::new(None, None, None));
        let requester = Arc::clone(&stopper);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            requester.request_stop();
        });
        let mut failures = 0u32;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handle_stream_failure(
                "acct",
                TeleError::Invocation("FLOOD_WAIT_3600".to_string(), Some(3600)),
                &mut failures,
                StreamFailureCtx::before_connect(None, Some(&stopper)),
                5,
            ),
        )
        .await
        .expect("a stop must end the backoff sleep")
        .expect("a stop is not a failure");
    }

    #[tokio::test]
    async fn a_healthy_connection_restarts_the_failure_streak() {
        // Regression: after the connect-time reset was removed, a quiet
        // account that reconnected healthily five times over days still gave
        // up because only a consumed update reset the streak.
        let mut failures = 4u32;
        handle_stream_failure(
            "acct",
            TeleError::Other("boom".into()),
            &mut failures,
            StreamFailureCtx::live(
                std::time::Duration::from_secs(61),
                Some(std::time::Instant::now()),
                None,
            ),
            5,
        )
        .await
        .expect("a failure after a healthy connection is not the fifth consecutive one");
        assert_eq!(
            failures, 1,
            "the streak must restart after a connection that stayed up"
        );
    }

    #[tokio::test]
    async fn a_short_lived_connection_still_counts_as_a_consecutive_failure() {
        let mut failures = 4u32;
        handle_stream_failure(
            "acct",
            TeleError::Other("boom".into()),
            &mut failures,
            StreamFailureCtx::live(
                std::time::Duration::from_secs(1),
                Some(std::time::Instant::now()),
                None,
            ),
            5,
        )
        .await
        .expect("the fifth consecutive failure still reconnects");
        assert_eq!(
            failures, 5,
            "a connection that dies immediately must not reset the streak"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_stop_mid_sweep_writes_due_rows_before_pending_albums() {
        // The sweep removed the due rows in deadline order; flushing the
        // buffer's later-deadline albums first inverted the output order.
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(Some(1), None, None);
        let (writer, sink) = test_writer(8, std::time::Duration::from_millis(200));
        let mut album = AlbumBuffer::new();
        album_ingest(
            "work",
            &mut album,
            serde_json::json!({"id": 99}),
            7,
            70,
            tokio::time::Instant::now(),
        );
        let swept = vec![
            serde_json::json!({"event": "NewMessage", "n": 1}),
            serde_json::json!({"event": "NewMessage", "n": 2}),
        ];
        let stopped = emit_due_albums(&writer, &stopper, "work", &mut album, swept)
            .await
            .unwrap();
        assert!(stopped);
        writer.finish().await;
        let rows: Vec<serde_json::Value> = sink
            .lines()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3, "no swept or buffered row may vanish");
        assert_eq!(rows[0]["n"], 1, "first due row");
        assert_eq!(rows[1]["n"], 2, "second due row before any pending album");
        assert_eq!(rows[2]["event"], "Album", "pending album last");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_stop_before_the_first_swept_row_still_writes_that_row() {
        // The stop can arrive between the sweep and the first emit (another
        // account spending the last --count slot). That row left the buffer
        // already, so a stop outcome that wrote nothing used to drop it while
        // still emitting its successors.
        let _lock = quiet_fields();
        let stopper = StreamStopper::new(Some(1), None, None);
        stopper.request_stop();
        let (writer, sink) = test_writer(8, std::time::Duration::from_millis(200));
        let mut album = AlbumBuffer::new();
        let swept = vec![
            serde_json::json!({"event": "NewMessage", "n": 1}),
            serde_json::json!({"event": "NewMessage", "n": 2}),
        ];
        let stopped = emit_due_albums(&writer, &stopper, "work", &mut album, swept)
            .await
            .unwrap();
        assert!(stopped);
        writer.finish().await;
        let rows: Vec<serde_json::Value> = sink
            .lines()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2, "the row that met the stop must not vanish");
        assert_eq!(rows[0]["n"], 1);
        assert_eq!(rows[1]["n"], 2);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_stop_flush_does_not_write_rows_past_until() {
        // Bypassing the count budget must not bypass the --until window.
        let _lock = quiet_fields();
        let until = chrono::Utc::now();
        let stopper = StreamStopper::new(None, None, Some(until));
        stopper.request_stop();
        let (writer, sink) = test_writer(8, std::time::Duration::from_millis(200));
        let mut album = AlbumBuffer::new();
        let before = (until - chrono::Duration::seconds(60)).to_rfc3339();
        let after = (until + chrono::Duration::seconds(60)).to_rfc3339();
        let swept = vec![
            serde_json::json!({"event": "NewMessage", "n": 1, "date": before}),
            serde_json::json!({"event": "NewMessage", "n": 2, "date": after}),
        ];
        let stopped = emit_due_albums(&writer, &stopper, "work", &mut album, swept)
            .await
            .unwrap();
        assert!(stopped);
        writer.finish().await;
        let rows: Vec<serde_json::Value> = sink
            .lines()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "a row past --until must not be flushed");
        assert_eq!(rows[0]["n"], 1);
    }
}
