//! Transport for the local API: bind hygiene, accept loop, per-connection
//! framing and limits. See [`crate::local_api`] for the wire shape and the
//! harness philosophy — nothing here knows about agents.
//!
//! Reuse note: per-connection rate limiting is the same sliding-window
//! [`RateLimiter`] the Telegram inbound loop uses (`dotagent-notify`), with
//! the connection itself as the single key.

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch, Notify};
use tracing::{debug, error, info, warn};

use dotagent_notify::telegram_inbound::RateLimiter;

use super::protocol::{
    error_code, ClientMessage, MessageSendParams, ServerError, ServerEvent, ServerResponse,
    MAX_CONNECTIONS, MAX_EVENT_QUEUE_BYTES, RATE_PER_MINUTE,
};

/// Ceiling on one request line. `read_bounded_line` returns as soon as this
/// many bytes are exceeded instead of waiting for an oversized frame's
/// newline.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Event frames allowed to sit in the per-connection channel before the byte
/// budget becomes the binding limit. Backstop, not policy.
const EVENT_QUEUE_SLOTS: usize = 512;

/// How long one frame gets to reach the client before it counts as a zombie.
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the daemon's gateway integration must provide. The server calls it;
/// it never calls back. `EventTx` is how output returns to the client that
/// asked for it — the server stays ignorant of sessions and agents.
#[async_trait]
pub trait LocalApiHandler: Send + Sync {
    /// Accept (or reject) a trigger. `Ok` means queued/dispatched, answered
    /// immediately by `{"accepted": true}` — the real reply arrives later as
    /// events on `events`. An optional hook runs after the response queue
    /// attempt, with whether the bounded writer queue accepted the frame. This
    /// lets a handler order subsequent events behind a successful response and
    /// still release cleanup when the connection is already gone.
    async fn handle_message(
        &self,
        params: MessageSendParams,
        actor: PeerInfo,
        events: EventTx,
    ) -> Result<Option<ResponseHook>, ServerError>;

    /// Answer `commands.list` — the slash-command catalog, shaped by the
    /// handler.
    async fn commands_list(&self) -> Result<Value, ServerError>;

    /// Answer `status.get` — daemon health snapshot, shaped by the handler.
    async fn status_get(&self) -> Result<Value, ServerError>;
}

/// Kernel-provided identity of the peer (threat model V8/V9): the actor an
/// audit line names. `None` fields mean the platform or call could not
/// provide them — `actor()` degrades honestly to `"local"` rather than
/// inventing an identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerInfo {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
}

impl PeerInfo {
    /// Stable rendering for logs and audit entries.
    pub fn actor(&self) -> String {
        match (self.pid, self.uid) {
            (Some(pid), Some(uid)) => format!("pid={pid} uid={uid}"),
            (Some(pid), None) => format!("pid={pid}"),
            (None, Some(uid)) => format!("uid={uid}"),
            (None, None) => "local".to_string(),
        }
    }
}

/// Why an event could not be queued for a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSendError {
    /// The per-connection byte budget was blown. The client has been
    /// disconnected: a consumer that far behind never catches up, and
    /// buffering forever would pin daemon memory.
    Overflow,
    /// The connection is already gone.
    Closed,
}

/// Synchronous work to run after the server attempts to queue a handler
/// response. The argument is true only when the frame entered the bounded
/// writer queue.
pub type ResponseHook = Box<dyn FnOnce(bool) + Send + 'static>;

/// Cloneable handle for pushing [`ServerEvent`]s to one client connection.
///
/// The handler receives one per `message.send` and the gateway keeps it for
/// the run it triggered — that is how output finds its way back without the
/// server holding any per-session state.
#[derive(Clone)]
pub struct EventTx {
    tx: mpsc::Sender<Frame>,
    queued_bytes: Arc<AtomicUsize>,
    max_queued_bytes: usize,
    /// Set when the byte budget or shared frame queue is blown: wakes the
    /// writer, which drops the stream. tokio senders cannot close the channel
    /// themselves (clones may live in the gateway), so the poison pair is the
    /// disconnect signal, and the flag flips later sends to `Closed`.
    poison: Arc<Notify>,
    poisoned: Arc<std::sync::atomic::AtomicBool>,
}

impl EventTx {
    fn poison_connection(&self) {
        self.poisoned.store(true, Ordering::Release);
        self.poison.notify_one();
    }

    fn reserve_bytes(&self, len: usize) -> bool {
        let mut queued = self.queued_bytes.load(Ordering::Relaxed);
        loop {
            if queued > self.max_queued_bytes || len > self.max_queued_bytes.saturating_sub(queued)
            {
                return false;
            }
            let next = queued + len;
            match self.queued_bytes.compare_exchange_weak(
                queued,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => queued = actual,
            }
        }
    }

    /// Queue one event. Synchronous on purpose: event delivery must never
    /// hold a run loop hostage to a slow client — the writer task drains
    /// under its own timeout.
    pub fn send(&self, event: &ServerEvent) -> Result<(), EventSendError> {
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(EventSendError::Closed);
        }
        let mut line = serde_json::to_string(event).map_err(|_| EventSendError::Closed)?;
        line.push('\n');
        let len = line.len();
        if !self.reserve_bytes(len) {
            warn!(
                max = self.max_queued_bytes,
                "local api event queue over byte budget; disconnecting the client"
            );
            self.poison_connection();
            return Err(EventSendError::Overflow);
        }
        if self.poisoned.load(Ordering::Acquire) {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
            return Err(EventSendError::Closed);
        }
        match self.tx.try_send(Frame::Event(line)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                warn!("local api event queue full; disconnecting the slow client");
                self.poison_connection();
                Err(EventSendError::Overflow)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                self.poison_connection();
                Err(EventSendError::Closed)
            }
        }
    }
}

/// One line headed to the client. Event frames count against the connection
/// byte budget; responses stay exempt but share the bounded frame queue.
enum Frame {
    Event(String),
    Response(String),
    ResponseAndClose(String),
}

/// Unix-socket JSON-lines server for local clients.
///
/// Deliberately hollow: routing, sessions and agent semantics live behind
/// [`LocalApiHandler`]. Construct, [`bind`](Self::bind) and
/// [`run_bound`](Self::run_bound), and let the daemon's gateway integration be
/// the brain.
pub struct LocalApiServer {
    socket_path: PathBuf,
    handler: Arc<dyn LocalApiHandler>,
    rate_per_minute: u32,
    max_connections: usize,
    max_event_queue_bytes: usize,
    write_timeout: Duration,
    #[cfg(test)]
    input_closed: Option<Arc<Notify>>,
}

impl LocalApiServer {
    /// Server with the default limits from [`super::protocol`].
    pub fn new(socket_path: impl Into<PathBuf>, handler: Arc<dyn LocalApiHandler>) -> Self {
        Self {
            socket_path: socket_path.into(),
            handler,
            rate_per_minute: RATE_PER_MINUTE,
            max_connections: MAX_CONNECTIONS,
            max_event_queue_bytes: MAX_EVENT_QUEUE_BYTES,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            #[cfg(test)]
            input_closed: None,
        }
    }

    /// Override the per-connection request rate.
    #[cfg(test)]
    pub fn with_rate_per_minute(mut self, per_minute: u32) -> Self {
        self.rate_per_minute = per_minute;
        self
    }

    /// Override the concurrent-connection cap.
    #[cfg(test)]
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Override the per-connection pending-event byte budget.
    #[cfg(test)]
    pub fn with_max_event_queue_bytes(mut self, max_bytes: usize) -> Self {
        self.max_event_queue_bytes = max_bytes;
        self
    }

    /// Override how long one write to a client may stall.
    #[cfg(test)]
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_input_closed(mut self, input_closed: Arc<Notify>) -> Self {
        self.input_closed = Some(input_closed);
        self
    }

    /// Create the listening socket, 0600.
    ///
    /// Hygiene order matters: sweep stale state, bind a std listener, chmod
    /// 0600, and only then hand it to tokio — no client can ever connect
    /// through a socket with looser permissions than intended. Failures at
    /// the socket path are loud (ERROR) because they are exactly the "daemon
    /// silently not serving" class of surprise.
    pub fn bind(&self) -> std::io::Result<UnixListener> {
        self.ensure_bindable()?;
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let std_listener = StdListener::bind(&self.socket_path)?;
        std::fs::set_permissions(&self.socket_path, PermissionsExt::from_mode(0o600))?;
        // tokio refuses to register a blocking fd (tokio-rs/tokio#7172), so
        // flip it only after the chmod: no client is served before the
        // reactor owns it anyway.
        std_listener.set_nonblocking(true)?;
        UnixListener::from_std(std_listener)
    }

    /// Decide whether the socket path is safe to bind on, sweeping what is
    /// ours and refusing what is not.
    fn ensure_bindable(&self) -> std::io::Result<()> {
        // lstat, not stat: a symlink at the path is "not a socket" and is
        // never followed into an unlink.
        let md = match std::fs::symlink_metadata(&self.socket_path) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !md.file_type().is_socket() {
            error!(
                path = %self.socket_path.display(),
                "local api socket path occupied by a non-socket file; refusing to bind or remove it"
            );
            return Err(std::io::Error::other(format!(
                "{} exists and is not a unix socket",
                self.socket_path.display()
            )));
        }
        // Somebody answers: a live server owns it, stale or not.
        if std::os::unix::net::UnixStream::connect(&self.socket_path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "another server is listening on {}",
                    self.socket_path.display()
                ),
            ));
        }
        // Nobody answers: a leftover from a crashed daemon. Sweep it only if
        // we own it — another uid's file at our path is not ours to unlink.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid takes no arguments and cannot fault.
            let euid = unsafe { libc::geteuid() };
            if md.uid() != euid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "stale socket {} belongs to uid {}; refusing to remove it",
                        self.socket_path.display(),
                        md.uid()
                    ),
                ));
            }
        }
        std::fs::remove_file(&self.socket_path)?;
        warn!(
            path = %self.socket_path.display(),
            "removed stale local api socket from a previous run"
        );
        Ok(())
    }

    /// Serve an already-bound listener until `shutdown` fires.
    ///
    /// The daemon binds before starting its other ingress tasks, so a socket
    /// error cannot leave Telegram or scheduled work running without the
    /// local endpoint promised by the daemon.
    pub(crate) async fn run_bound(
        &self,
        listener: UnixListener,
        mut shutdown: watch::Receiver<()>,
    ) -> std::io::Result<()> {
        info!(path = %self.socket_path.display(), "local api listening");
        let live = Arc::new(AtomicUsize::new(0));

        loop {
            let (stream, _) = tokio::select! {
                _ = shutdown.changed() => break,
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        error!(
                            path = %self.socket_path.display(),
                            error = %error,
                            "local api accept failed"
                        );
                        return Err(error);
                    }
                },
            };

            let live_now = live.fetch_add(1, Ordering::Relaxed);
            if live_now >= self.max_connections {
                // Over cap: still accept, so the client learns why instead of
                // staring at a connection-refused.
                live.fetch_sub(1, Ordering::Relaxed);
                reject_overflow(stream, self.max_connections).await;
                continue;
            }

            let handler = Arc::clone(&self.handler);
            let limits = ConnectionLimits {
                rate_per_minute: self.rate_per_minute,
                max_event_queue_bytes: self.max_event_queue_bytes,
                write_timeout: self.write_timeout,
                #[cfg(test)]
                input_closed: self.input_closed.clone(),
            };
            let conn_shutdown = shutdown.clone();
            let conn_count = Arc::clone(&live);
            tokio::spawn(async move {
                serve_connection(stream, handler, limits, conn_shutdown).await;
                conn_count.fetch_sub(1, Ordering::Relaxed);
            });
        }

        // Best effort: leave no stale socket for the next boot to sweep. A
        // crash mid-run still leaves one, which bind() hygiene handles.
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }
}

/// Per-connection limits, copied out of the server config so the spawned
/// task is self-contained.
struct ConnectionLimits {
    rate_per_minute: u32,
    max_event_queue_bytes: usize,
    write_timeout: Duration,
    #[cfg(test)]
    input_closed: Option<Arc<Notify>>,
}

/// Serve one connection: read lines, enforce limits, route to the handler,
/// keep the writer fed until the client goes away or the daemon shuts down.
async fn serve_connection(
    stream: UnixStream,
    handler: Arc<dyn LocalApiHandler>,
    limits: ConnectionLimits,
    shutdown: watch::Receiver<()>,
) {
    let peer = peer_credentials(stream.as_raw_fd());
    serve_connection_with_peer(stream, handler, limits, shutdown, peer).await;
}

async fn serve_connection_with_peer(
    stream: UnixStream,
    handler: Arc<dyn LocalApiHandler>,
    limits: ConnectionLimits,
    mut shutdown: watch::Receiver<()>,
    peer: PeerInfo,
) {
    debug!(actor = %peer.actor(), "local api client connected");

    if let Some(uid) = peer.uid {
        let euid = effective_uid();
        if uid != euid {
            warn!(
                actor = %peer.actor(),
                expected_uid = euid,
                "local api peer uid does not match daemon uid; rejecting client"
            );
            reject_peer(stream, limits.write_timeout).await;
            return;
        }
    } else {
        debug!(
            actor = %peer.actor(),
            "local api peer uid unavailable; accepting without inventing identity"
        );
    }

    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let (frames_tx, frames_rx) = mpsc::channel::<Frame>(EVENT_QUEUE_SLOTS);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let poison = Arc::new(Notify::new());
    let poisoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let events = EventTx {
        tx: frames_tx,
        queued_bytes: Arc::clone(&queued_bytes),
        max_queued_bytes: limits.max_event_queue_bytes,
        poison: Arc::clone(&poison),
        poisoned: Arc::clone(&poisoned),
    };

    let mut writer = tokio::spawn(write_frames(
        write_half,
        frames_rx,
        queued_bytes,
        poison,
        shutdown.clone(),
        limits.write_timeout,
    ));
    let mut writer_finished = false;

    let mut rate = RateLimiter::new(limits.rate_per_minute);

    loop {
        let frame = tokio::select! {
            biased;
            writer_result = &mut writer => {
                writer_finished = true;
                match writer_result {
                    Ok(()) => debug!(actor = %peer.actor(), "local api writer stopped"),
                    Err(error) => warn!(
                        actor = %peer.actor(),
                        error = %error,
                        "local api writer task failed"
                    ),
                }
                None
            }
            _ = shutdown.changed() => None,
            line = read_bounded_line(&mut reader, MAX_LINE_BYTES) => match line {
                Ok(Some(line)) => Some(line),
                Ok(None) => {
                    #[cfg(test)]
                    if let Some(input_closed) = &limits.input_closed {
                        input_closed.notify_one();
                    }
                    None
                }
                Err(error) => {
                    debug!(actor = %peer.actor(), error = %error, "local api read error");
                    #[cfg(test)]
                    if let Some(input_closed) = &limits.input_closed {
                        input_closed.notify_one();
                    }
                    // A read error means the peer is gone, not a clean
                    // half-close. Stop producers from retaining this slot.
                    events.poison_connection();
                    None
                }
            },
        };
        let Some(frame) = frame else {
            break;
        };

        let (line, oversized) = match frame {
            BoundedLine::Complete(line) => (line, false),
            BoundedLine::Oversized(prefix) => (prefix, true),
        };
        if line.is_empty() && !oversized {
            continue;
        }

        let salvaged_id = salvage_id_from_bytes(&line);
        // Every non-empty frame consumes quota before any parse or size check.
        if !rate.check(0) {
            let response = ServerResponse::err(
                salvaged_id,
                error_code::RATE_LIMITED,
                format!(
                    "more than {} requests per minute; slow down",
                    limits.rate_per_minute
                ),
            );
            let result = if oversized {
                send_response_and_close(&events, response)
            } else {
                send_response(&events, response)
            };
            if result.is_err() || oversized {
                break;
            }
            continue;
        }
        if oversized {
            if send_response_and_close(
                &events,
                ServerResponse::err(
                    salvaged_id,
                    error_code::INVALID_REQUEST,
                    format!("request line exceeds {MAX_LINE_BYTES} bytes"),
                ),
            )
            .is_err()
            {
                break;
            }
            // The response frame owns the close: this remains deterministic
            // even when a previous request left EventTx clones alive.
            break;
        }

        let line = match String::from_utf8(line) {
            Ok(line) => line,
            Err(_) => {
                if send_response(
                    &events,
                    ServerResponse::err(
                        salvaged_id,
                        error_code::INVALID_REQUEST,
                        "request line is not valid UTF-8",
                    ),
                )
                .is_err()
                {
                    break;
                }
                continue;
            }
        };
        let msg = match parse_request(&line) {
            Ok(m) => m,
            Err(resp) => {
                if send_response(&events, resp).is_err() {
                    break;
                }
                continue;
            }
        };
        let (response, after_response) = dispatch(&*handler, &peer, &events, msg).await;
        if send_response_after(&events, response, after_response).is_err() {
            break;
        }
    }

    // A clean EOF is the client's half-close: request-owned EventTx clones
    // must keep the writer alive until their producers finish. The writer's
    // own shutdown, poison, write timeout and write error remain the hard
    // boundaries for a dead daemon/client or a broken connection.
    drop(events);
    if !writer_finished {
        if let Err(error) = writer.await {
            warn!(actor = %peer.actor(), error = %error, "local api writer task failed");
        }
    }
    debug!(actor = %peer.actor(), "local api client disconnected");
}

enum BoundedLine {
    Complete(Vec<u8>),
    Oversized(Vec<u8>),
}

/// Read exactly one frame without letting a missing newline grow the buffer.
/// Once the bound is crossed, retain only the prefix for best-effort id
/// salvage and return immediately so the connection can be closed.
async fn read_bounded_line<R>(reader: &mut R, max: usize) -> io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::with_capacity(max.min(4096));
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Ok(Some(BoundedLine::Complete(line)));
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(buffer.len());
        let remaining = max.saturating_sub(line.len());
        if content_len > remaining {
            line.extend_from_slice(&buffer[..remaining]);
            // Consume one byte past the limit so a caller that inspects the
            // reader cannot mistake this for a complete frame. The caller
            // closes the connection immediately and never drains the peer.
            reader.consume(remaining + 1);
            return Ok(Some(BoundedLine::Oversized(line)));
        }

        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        line.extend_from_slice(&buffer[..content_len]);
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(BoundedLine::Complete(line)));
        }
    }
}

fn salvage_id_from_bytes(line: &[u8]) -> String {
    let Ok(line) = std::str::from_utf8(line) else {
        return String::new();
    };
    if let Ok(raw) = serde_json::from_str::<Value>(line) {
        return super::protocol::salvage_id(&raw);
    }

    // A truncated frame can still contain a complete id near its start.
    // Recover only a top-level object field; arbitrary text is not an id.
    for (start, _) in line.match_indices("\"id\"") {
        let before = line[..start].trim_end().as_bytes().last().copied();
        if !matches!(before, Some(b'{') | Some(b',')) {
            continue;
        }
        let Some(value) = line[start + 4..].trim_start().strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start();
        if value.starts_with('"') {
            let mut escaped = false;
            for (index, byte) in value.as_bytes().iter().enumerate().skip(1) {
                if escaped {
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    if let Ok(id) = serde_json::from_str::<String>(&value[..=index]) {
                        return id;
                    }
                    break;
                }
            }
        } else {
            let end = value
                .find(|byte: char| byte == ',' || byte == '}' || byte.is_whitespace())
                .unwrap_or(value.len());
            if let Ok(raw) = serde_json::from_str::<Value>(&value[..end]) {
                let id = super::protocol::salvage_id(&raw);
                if !id.is_empty() {
                    return id;
                }
            }
        }
    }
    String::new()
}

/// Route one parsed request to the handler and shape its response.
async fn dispatch(
    handler: &dyn LocalApiHandler,
    peer: &PeerInfo,
    events: &EventTx,
    msg: ClientMessage,
) -> (ServerResponse, Option<ResponseHook>) {
    match msg.method.as_str() {
        "message.send" => {
            let params: MessageSendParams = match msg.params {
                Some(v) => match serde_json::from_value(v) {
                    Ok(p) => p,
                    Err(e) => {
                        return (
                            ServerResponse::err(
                                msg.id,
                                error_code::INVALID_REQUEST,
                                format!("invalid params for message.send: {e}"),
                            ),
                            None,
                        )
                    }
                },
                None => {
                    return (
                        ServerResponse::err(
                            msg.id,
                            error_code::INVALID_REQUEST,
                            "message.send requires params",
                        ),
                        None,
                    )
                }
            };
            if let Err(e) = params.validate() {
                return (ServerResponse::err(msg.id, e.code, e.message), None);
            }
            match handler
                .handle_message(params, peer.clone(), events.clone())
                .await
            {
                Ok(after_response) => (
                    ServerResponse::ok(msg.id, serde_json::json!({ "accepted": true })),
                    after_response,
                ),
                Err(e) => (ServerResponse::err(msg.id, e.code, e.message), None),
            }
        }
        "commands.list" => match handler.commands_list().await {
            Ok(v) => (ServerResponse::ok(msg.id, v), None),
            Err(e) => (ServerResponse::err(msg.id, e.code, e.message), None),
        },
        "status.get" => match handler.status_get().await {
            Ok(v) => (ServerResponse::ok(msg.id, v), None),
            Err(e) => (ServerResponse::err(msg.id, e.code, e.message), None),
        },
        other => (
            ServerResponse::err(
                msg.id,
                error_code::INVALID_REQUEST,
                format!("unknown method '{other}'"),
            ),
            None,
        ),
    }
}

/// Lenient two-step parse: the line becomes a `Value` first so a request
/// that fails the typed shape still gets its id echoed back; only then is it
/// forced into [`ClientMessage`].
fn parse_request(line: &str) -> Result<ClientMessage, ServerResponse> {
    let raw: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Err(ServerResponse::err(
                salvage_id_from_bytes(line.as_bytes()),
                error_code::INVALID_REQUEST,
                format!("invalid JSON: {e}"),
            ))
        }
    };
    let id = super::protocol::salvage_id(&raw);
    serde_json::from_value::<ClientMessage>(raw).map_err(|e| {
        ServerResponse::err(
            id,
            error_code::INVALID_REQUEST,
            format!("invalid request: {e}"),
        )
    })
}

/// Queue a response for the writer. Never blocks: the reader must keep
/// draining the client even when the client stops reading. A full or closed
/// queue poisons the connection so the reader cannot keep the slot alive.
fn send_response(events: &EventTx, resp: ServerResponse) -> Result<(), EventSendError> {
    if events.poisoned.load(Ordering::Acquire) {
        return Err(EventSendError::Closed);
    }
    let Ok(mut line) = serde_json::to_string(&resp) else {
        return Err(EventSendError::Closed);
    };
    line.push('\n');
    match events.tx.try_send(Frame::Response(line)) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            debug!("local api connection closed before the response was queued");
            events.poison_connection();
            Err(EventSendError::Closed)
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("local api response queue full; disconnecting the client");
            events.poison_connection();
            Err(EventSendError::Overflow)
        }
    }
}

/// Queue an error that is written before the connection closes. This avoids
/// racing a poison notification against the response and preserves a
/// salvageable request id even when older requests left EventTx clones alive.
fn send_response_and_close(events: &EventTx, resp: ServerResponse) -> Result<(), EventSendError> {
    if events.poisoned.load(Ordering::Acquire) {
        return Err(EventSendError::Closed);
    }
    let Ok(mut line) = serde_json::to_string(&resp) else {
        return Err(EventSendError::Closed);
    };
    line.push('\n');
    match events.tx.try_send(Frame::ResponseAndClose(line)) {
        Ok(()) => {
            events.poisoned.store(true, Ordering::Release);
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            debug!("local api connection closed before the response was queued");
            events.poison_connection();
            Err(EventSendError::Closed)
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("local api response queue full; disconnecting the client");
            events.poison_connection();
            Err(EventSendError::Overflow)
        }
    }
}

/// Queue the response before waking handler work waiting on it. This is
/// synchronous by design: `try_send` establishes the queue order without
/// holding a lock across an await. The hook also runs on failure so an
/// admitted handler can cancel any gate that would otherwise wait forever.
fn send_response_after(
    events: &EventTx,
    resp: ServerResponse,
    after_response: Option<ResponseHook>,
) -> Result<(), EventSendError> {
    let result = send_response(events, resp);
    if let Some(after_response) = after_response {
        after_response(result.is_ok());
    }
    result
}

/// Drain queued frames to the client, each under a write timeout so a
/// zombie cannot pin the daemon: one frame that cannot be delivered in
/// `write_timeout` ends the connection. The poison flag is the
/// event-budget's disconnect order (see [`EventTx::send`]).
async fn write_frames(
    mut tx: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<Frame>,
    queued_bytes: Arc<AtomicUsize>,
    poison: Arc<Notify>,
    mut shutdown: watch::Receiver<()>,
    write_timeout: Duration,
) {
    loop {
        let frame = tokio::select! {
            _ = shutdown.changed() => break,
            _ = poison.notified() => {
                debug!("local api event budget blown; closing connection");
                break;
            }
            frame = rx.recv() => frame,
        };
        let Some(frame) = frame else {
            break;
        };
        let (line, budgeted, close_after) = match frame {
            Frame::Event(line) => (line, true, false),
            Frame::Response(line) => (line, false, false),
            Frame::ResponseAndClose(line) => (line, false, true),
        };
        let write = async {
            tx.write_all(line.as_bytes()).await?;
            tx.flush().await
        };
        match tokio::time::timeout(write_timeout, write).await {
            Ok(Ok(())) => {
                if budgeted {
                    queued_bytes.fetch_sub(line.len(), Ordering::Relaxed);
                }
                if close_after {
                    break;
                }
            }
            Ok(Err(e)) => {
                debug!(error = %e, "local api write failed; closing connection");
                break;
            }
            Err(_) => {
                warn!(
                    timeout = ?write_timeout,
                    "local api client stopped reading; dropping it"
                );
                break;
            }
        }
    }
}

/// Tell an over-cap client why it is being dropped, then drop it.
async fn reject_overflow(mut stream: UnixStream, max_connections: usize) {
    let response = ServerResponse::err(
        "",
        error_code::TOO_MANY_CONNECTIONS,
        format!("connection limit ({max_connections}) reached; try again later"),
    );
    let Ok(mut line) = serde_json::to_string(&response) else {
        return;
    };
    line.push('\n');
    // Best effort: a client that cannot even accept the rejection learns of
    // its fate from the socket closing when `stream` falls out of scope.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(line.as_bytes())).await;
}

async fn reject_peer(mut stream: UnixStream, write_timeout: Duration) {
    let response = ServerResponse::err(
        "",
        error_code::INTERNAL,
        "peer credentials do not match the daemon uid",
    );
    let Ok(mut line) = serde_json::to_string(&response) else {
        return;
    };
    line.push('\n');
    let write = async {
        stream.write_all(line.as_bytes()).await?;
        stream.flush().await
    };
    if let Err(error) = tokio::time::timeout(write_timeout, write).await {
        debug!(error = %error, "local api could not send peer rejection");
    }
}

/// Ask the kernel who is on the other end of the socket — the actor every
/// audit line names. Linux answers one `SO_PEERCRED` call with pid and uid;
/// Darwin needs two `SOL_LOCAL` options; elsewhere we degrade honestly.
#[cfg(target_os = "linux")]
fn peer_credentials(fd: RawFd) -> PeerInfo {
    // SAFETY: getsockopt writes into a zeroed, correctly sized ucred that
    // the kernel never reads from.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return PeerInfo::default();
    }
    PeerInfo {
        pid: Some(cred.pid as u32),
        uid: Some(cred.uid),
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and cannot fault.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "macos")]
fn peer_credentials(fd: RawFd) -> PeerInfo {
    // libc does not export the SOL_LOCAL family; the values below are the
    // stable ABI constants from <sys/un.h> that every Darwin release ships.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERCRED: libc::c_int = 0x001;
    const LOCAL_PEERPID: libc::c_int = 0x002;

    let mut uid = None;
    // SAFETY: getsockopt writes into a zeroed, correctly sized xucred that
    // the kernel never reads from.
    let mut cred: libc::xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERCRED,
            &mut cred as *mut libc::xucred as *mut libc::c_void,
            &mut len,
        )
    };
    // cr_version == XUCRED_VERSION (0) is the kernel's "actually filled in".
    if rc == 0 && cred.cr_version == 0 {
        uid = Some(cred.cr_uid);
    }
    if uid.is_none() {
        // macOS 27 (observed on build 26A5406e) answers the SOL_LOCAL
        // options with ENOTSUP; getpeereid still works, and uid is the
        // half the threat model refuses to lose.
        // SAFETY: getpeereid writes one uid_t and one gid_t.
        let (mut euid, mut egid) = (0 as libc::uid_t, 0 as libc::gid_t);
        if unsafe { libc::getpeereid(fd, &mut euid, &mut egid) } == 0 {
            uid = Some(euid);
        }
    }

    let mut pid = None;
    // SAFETY: getsockopt writes one zeroed pid_t.
    let mut raw_pid: libc::pid_t = 0;
    let mut pid_len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut raw_pid as *mut libc::pid_t as *mut libc::c_void,
            &mut pid_len,
        )
    };
    if rc == 0 {
        pid = Some(raw_pid as u32);
    }

    PeerInfo { pid, uid }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_credentials(_fd: RawFd) -> PeerInfo {
    // TODO(platform): FreeBSD/NetBSD also answer SO_PEERCRED; wire it when
    // dotagent actually builds there. Until then the audit actor degrades
    // to "local" rather than claiming an identity nobody verified.
    PeerInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_api::protocol::DEFAULT_SESSION_ID;
    use serde_json::json;
    use std::path::Path;

    /// Handler that records everything and answers with canned data.
    #[derive(Default)]
    struct FakeHandler {
        messages: std::sync::Mutex<Vec<(MessageSendParams, PeerInfo)>>,
        event_txs: std::sync::Mutex<Vec<EventTx>>,
    }

    #[async_trait]
    impl LocalApiHandler for FakeHandler {
        async fn handle_message(
            &self,
            params: MessageSendParams,
            actor: PeerInfo,
            events: EventTx,
        ) -> Result<Option<ResponseHook>, ServerError> {
            self.messages.lock().unwrap().push((params, actor));
            self.event_txs.lock().unwrap().push(events);
            Ok(None)
        }

        async fn commands_list(&self) -> Result<Value, ServerError> {
            Ok(json!([{ "name": "standup", "description": "post a standup" }]))
        }

        async fn status_get(&self) -> Result<Value, ServerError> {
            Ok(json!({ "daemon": "ok" }))
        }
    }

    struct TestClient {
        reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl TestClient {
        async fn connect(path: &Path) -> Self {
            let mut last_err = None;
            for _ in 0..500 {
                match tokio::net::UnixStream::connect(path).await {
                    Ok(s) => {
                        let (r, w) = s.into_split();
                        return Self {
                            reader: BufReader::new(r).lines(),
                            writer: w,
                        };
                    }
                    Err(e) => {
                        last_err = Some(e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            panic!(
                "socket {} never came up: {}",
                path.display(),
                last_err.unwrap()
            );
        }

        async fn send(&mut self, line: &str) {
            self.writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            self.writer.flush().await.unwrap();
        }

        /// Next line as JSON; `None` on EOF; panics instead of hanging.
        async fn recv(&mut self) -> Option<Value> {
            match tokio::time::timeout(Duration::from_secs(5), self.reader.next_line()).await {
                Ok(Ok(Some(l))) => Some(serde_json::from_str(&l).unwrap()),
                Ok(Ok(None)) => None,
                Ok(Err(e)) => panic!("read error: {e}"),
                Err(_) => panic!("timed out waiting for a line"),
            }
        }

        async fn recv_for(&mut self, timeout: Duration) -> Option<Option<Value>> {
            match tokio::time::timeout(timeout, self.reader.next_line()).await {
                Ok(Ok(Some(l))) => Some(Some(serde_json::from_str(&l).unwrap())),
                Ok(Ok(None)) => Some(None),
                Ok(Err(e)) => panic!("read error: {e}"),
                Err(_) => None,
            }
        }
    }

    async fn connection_can_serve_status(path: &Path) -> bool {
        let Ok(stream) = tokio::net::UnixStream::connect(path).await else {
            return false;
        };
        let (read_half, mut write_half) = stream.into_split();
        if write_half
            .write_all(b"{\"id\":\"probe\",\"method\":\"status.get\"}\n")
            .await
            .is_err()
        {
            return false;
        }
        if write_half.flush().await.is_err() {
            return false;
        }

        let mut reader = BufReader::new(read_half);
        let mut line = Vec::new();
        match tokio::time::timeout(
            Duration::from_millis(50),
            reader.read_until(b'\n', &mut line),
        )
        .await
        {
            Ok(Ok(_)) => serde_json::from_slice::<Value>(&line)
                .map(|response| response["result"]["daemon"] == json!("ok"))
                .unwrap_or(false),
            _ => false,
        }
    }

    fn server(path: impl Into<PathBuf>) -> LocalApiServer {
        LocalApiServer::new(path, Arc::new(FakeHandler::default()))
    }

    fn spawn(
        srv: LocalApiServer,
    ) -> (
        tokio::task::JoinHandle<std::io::Result<()>>,
        watch::Sender<()>,
    ) {
        let listener = srv.bind().unwrap();
        let (tx, rx) = watch::channel(());
        let handle = tokio::spawn(async move { srv.run_bound(listener, rx).await });
        (handle, tx)
    }

    fn socket_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("api.sock")
    }

    // `bind` registers with the tokio reactor, so these need a runtime even
    // though they never await anything themselves.
    #[tokio::test]
    async fn bind_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        // A listener that died without removing its socket.
        drop(StdListener::bind(&path).unwrap());
        assert!(path.exists());
        server(path).bind().expect("stale socket must be swept");
    }

    #[tokio::test]
    async fn bind_refuses_a_regular_file_at_the_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        std::fs::write(&path, "not a socket").unwrap();
        let err = server(path).bind().unwrap_err();
        assert!(err.to_string().contains("not a unix socket"), "{err}");
    }

    #[tokio::test]
    async fn bind_refuses_while_another_server_is_listening() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let _alive = StdListener::bind(&path).unwrap();
        let err = server(path).bind().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn numeric_request_ids_are_normalized_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let (handle, shutdown) = spawn(LocalApiServer::new(path.clone(), fake.clone()));

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":42,"method":"message.send","params":{"text":"status?"}}"#)
            .await;
        let resp = client.recv().await.unwrap();
        assert_eq!(resp["id"], json!("42"));
        assert_eq!(resp["result"]["accepted"], json!(true));

        {
            let messages = fake.messages.lock().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].0.text, "status?");
            assert_eq!(messages[0].0.effective_session_id(), DEFAULT_SESSION_ID);
        }

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_half_closed_client_receives_a_late_terminal_reply() {
        const LATE_RESPONSE_DELAY: Duration = Duration::from_millis(300);

        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let input_closed = Arc::new(Notify::new());
        let (handle, shutdown) = spawn(
            LocalApiServer::new(path.clone(), fake.clone()).with_input_closed(input_closed.clone()),
        );

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":"late","method":"message.send","params":{"text":"hi"}}"#)
            .await;
        assert_eq!(
            client.recv().await.unwrap()["result"]["accepted"],
            json!(true)
        );
        let events = fake.event_txs.lock().unwrap()[0].clone();

        let input_closed = input_closed.notified();
        client.writer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), input_closed)
            .await
            .expect("server must observe the half-close");

        // This deliberately crosses the old 250 ms drain grace. It is not
        // used to establish ordering; the Notify above does that.
        tokio::time::sleep(LATE_RESPONSE_DELAY).await;
        assert_eq!(
            events.send(&ServerEvent::reply("default", "late final")),
            Ok(())
        );
        drop(events);
        fake.event_txs.lock().unwrap().clear();

        let reply = client.recv().await.expect("late reply must reach client");
        assert_eq!(reply["event"], json!("reply"));
        assert_eq!(reply["text"], json!("late final"));
        assert!(
            client.recv().await.is_none(),
            "writer must close after producers finish"
        );

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn peer_credentials_identify_the_test_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let (handle, shutdown) = spawn(LocalApiServer::new(path.clone(), fake.clone()));

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":"u1","method":"message.send","params":{"text":"hi"}}"#)
            .await;
        assert_eq!(
            client.recv().await.unwrap()["result"]["accepted"],
            json!(true)
        );

        let peer = fake.messages.lock().unwrap()[0].1.clone();
        // uid is the half the threat model refuses to lose, and both
        // SO_PEERCRED (Linux) and getpeereid (Darwin) provide it.
        assert_eq!(peer.uid, Some(unsafe { libc::geteuid() }), "peer uid");
        // LOCAL_PEERPID is ENOTSUP on macOS 27 (build 26A5406e): when the
        // kernel does answer, it must name us; when it does not, None is
        // the honest answer.
        if let Some(pid) = peer.pid {
            assert_eq!(pid, std::process::id());
        }

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_peer_uid_mismatch_is_rejected_before_the_handler() {
        let (server_stream, client_stream) = tokio::net::UnixStream::pair().unwrap();
        let fake = Arc::new(FakeHandler::default());
        let (_shutdown, shutdown_rx) = watch::channel(());
        let other_uid = if effective_uid() == 0 { 1 } else { 0 };
        let task = tokio::spawn(serve_connection_with_peer(
            server_stream,
            fake.clone(),
            ConnectionLimits {
                rate_per_minute: RATE_PER_MINUTE,
                max_event_queue_bytes: MAX_EVENT_QUEUE_BYTES,
                write_timeout: DEFAULT_WRITE_TIMEOUT,
                #[cfg(test)]
                input_closed: None,
            },
            shutdown_rx,
            PeerInfo {
                pid: None,
                uid: Some(other_uid),
            },
        ));

        let mut lines = BufReader::new(client_stream).lines();
        let response = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], json!("internal"));
        assert!(
            fake.messages.lock().unwrap().is_empty(),
            "a peer with a different uid must not reach the handler"
        );

        task.await.unwrap();
    }

    #[tokio::test]
    async fn an_invalid_session_id_is_rejected_before_the_handler_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let (handle, shutdown) = spawn(LocalApiServer::new(path.clone(), fake.clone()));

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":"u1","method":"message.send","params":{"session_id":"../../etc/passwd","text":"hi"}}"#)
            .await;
        let resp = client.recv().await.unwrap();
        assert_eq!(resp["id"], json!("u1"));
        assert_eq!(resp["error"]["code"], json!("session_id_invalid"));
        assert!(
            fake.messages.lock().unwrap().is_empty(),
            "the handler must not be called for an invalid session"
        );

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rate_limit_kicks_in_after_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path).with_rate_per_minute(2));

        let mut client = TestClient::connect(&path).await;
        for i in 1..=3 {
            client
                .send(&format!(
                    r#"{{"id":"r{i}","method":"message.send","params":{{"text":"hi"}}}}"#
                ))
                .await;
        }
        let r1 = client.recv().await.unwrap();
        let r2 = client.recv().await.unwrap();
        let r3 = client.recv().await.unwrap();
        assert_eq!(r1["result"]["accepted"], json!(true));
        assert_eq!(r2["result"]["accepted"], json!(true));
        assert_eq!(r3["id"], json!("r3"));
        assert_eq!(r3["error"]["code"], json!("rate_limited"));

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_and_oversized_lines_consume_rate_quota() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let (handle, shutdown) =
            spawn(LocalApiServer::new(path.clone(), fake.clone()).with_rate_per_minute(1));

        let mut client = TestClient::connect(&path).await;
        client.send("this is not json").await;
        client
            .send(&format!(
                r#"{{"id":"big","method":"status.get","params":{{"padding":"{}"}}}}"#,
                "a".repeat(70 * 1024)
            ))
            .await;

        assert_eq!(
            client.recv().await.unwrap()["error"]["code"],
            json!("invalid_request")
        );
        let oversized = client.recv().await.unwrap();
        assert_eq!(oversized["id"], json!("big"));
        assert_eq!(oversized["error"]["code"], json!("rate_limited"));
        assert!(tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .unwrap()
            .is_none());
        assert!(fake.messages.lock().unwrap().is_empty());

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn one_connection_accepts_valid_lines_after_64_kib_total() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let (handle, shutdown) =
            spawn(LocalApiServer::new(path.clone(), fake.clone()).with_rate_per_minute(256));

        let mut client = TestClient::connect(&path).await;
        let text = "a".repeat(512);
        let request_count = 128;
        for index in 0..request_count {
            client
                .send(&format!(
                    r#"{{"id":"r{index}","method":"message.send","params":{{"text":"{text}"}}}}"#
                ))
                .await;
        }

        for index in 0..request_count {
            let response = client.recv().await.unwrap();
            assert_eq!(response["id"], json!(format!("r{index}")));
            assert_eq!(response["result"]["accepted"], json!(true));
        }
        assert_eq!(fake.messages.lock().unwrap().len(), request_count);

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn event_queue_overflow_disconnects_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let (handle, shutdown) = spawn(
            LocalApiServer::new(path.clone(), fake.clone())
                .with_max_event_queue_bytes(64)
                .with_write_timeout(Duration::from_millis(200)),
        );

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":"u1","method":"message.send","params":{"text":"hi"}}"#)
            .await;
        assert_eq!(
            client.recv().await.unwrap()["result"]["accepted"],
            json!(true)
        );

        // The client never reads, so the kernel buffer fills, writes begin
        // to stall and the byte budget stops being drained.
        let events = fake.event_txs.lock().unwrap()[0].clone();
        let mut overflowed = false;
        for _ in 0..8192 {
            if events
                .send(&ServerEvent::reply_delta("default", "filler line"))
                .is_err()
            {
                overflowed = true;
                break;
            }
            // Yield so the writer task gets a turn to (try to) drain.
            tokio::task::yield_now().await;
        }
        assert!(overflowed, "budget must eventually blow");

        // Whatever is queued drains, then the connection closes: EOF, and
        // Closed from any further send.
        let mut saw_eof = false;
        for _ in 0..4096 {
            if client.recv().await.is_none() {
                saw_eof = true;
                break;
            }
        }
        assert!(saw_eof, "client must be disconnected after overflow");
        assert_eq!(
            events.send(&ServerEvent::reply("default", "too late")),
            Err(EventSendError::Closed)
        );

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[test]
    fn a_full_response_queue_poison_disconnects_the_connection() {
        let (tx, mut rx) = mpsc::channel(1);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let poison = Arc::new(Notify::new());
        let poisoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = EventTx {
            tx,
            queued_bytes,
            max_queued_bytes: MAX_EVENT_QUEUE_BYTES,
            poison,
            poisoned: Arc::clone(&poisoned),
        };

        assert!(send_response(&events, ServerResponse::ok("first", json!({}))).is_ok());
        assert_eq!(
            send_response(&events, ServerResponse::ok("second", json!({}))),
            Err(EventSendError::Overflow)
        );
        assert!(poisoned.load(Ordering::Acquire));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn response_hook_runs_only_after_the_response_is_queued() {
        let (tx, mut rx) = mpsc::channel(EVENT_QUEUE_SLOTS);
        let events = EventTx {
            tx,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            max_queued_bytes: MAX_EVENT_QUEUE_BYTES,
            poison: Arc::new(Notify::new()),
            poisoned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let hook: ResponseHook = Box::new(move |enqueued| {
            assert!(enqueued);
            let Frame::Response(line) = rx.try_recv().expect("response must be queued first")
            else {
                panic!("response hook must observe the response frame");
            };
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["result"]["accepted"], json!(true));
        });

        assert!(send_response_after(
            &events,
            ServerResponse::ok("u1", json!({ "accepted": true })),
            Some(hook),
        )
        .is_ok());
    }

    #[test]
    fn response_hook_reports_enqueue_failure_for_cleanup() {
        let (tx, mut rx) = mpsc::channel(1);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let poison = Arc::new(Notify::new());
        let poisoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = EventTx {
            tx,
            queued_bytes,
            max_queued_bytes: MAX_EVENT_QUEUE_BYTES,
            poison,
            poisoned,
        };
        events
            .tx
            .try_send(Frame::Response("already queued\n".into()))
            .expect("the queue must be full for this test");

        let hook_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_called_by_hook = Arc::clone(&hook_called);
        let hook: ResponseHook = Box::new(move |enqueued| {
            assert!(!enqueued);
            hook_called_by_hook.store(true, Ordering::Release);
        });

        assert_eq!(
            send_response_after(
                &events,
                ServerResponse::ok("u1", json!({ "accepted": true })),
                Some(hook),
            ),
            Err(EventSendError::Overflow)
        );
        assert!(hook_called.load(Ordering::Acquire));
        assert!(events.poisoned.load(Ordering::Acquire));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn event_queue_budget_rejects_atomic_overflow_without_wrapping() {
        let (tx, _rx) = mpsc::channel(EVENT_QUEUE_SLOTS);
        let queued_bytes = Arc::new(AtomicUsize::new(usize::MAX - 1));
        let poison = Arc::new(Notify::new());
        let poisoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = EventTx {
            tx,
            queued_bytes: Arc::clone(&queued_bytes),
            max_queued_bytes: usize::MAX,
            poison,
            poisoned,
        };

        assert_eq!(
            events.send(&ServerEvent::reply("default", "event")),
            Err(EventSendError::Overflow)
        );
        assert_eq!(queued_bytes.load(Ordering::Relaxed), usize::MAX - 1);
    }

    #[test]
    fn request_id_salvage_handles_a_truncated_json_prefix() {
        assert_eq!(
            salvage_id_from_bytes(br#"{"id":"partial","method":"status.get""#),
            "partial"
        );
        assert_eq!(salvage_id_from_bytes(b"not json"), "");
    }

    #[tokio::test]
    async fn commands_list_and_status_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path));

        let mut client = TestClient::connect(&path).await;
        client.send(r#"{"id":"c1","method":"commands.list"}"#).await;
        let resp = client.recv().await.unwrap();
        assert_eq!(resp["id"], json!("c1"));
        assert_eq!(resp["result"][0]["name"], json!("standup"));

        client.send(r#"{"id":"s1","method":"status.get"}"#).await;
        let resp = client.recv().await.unwrap();
        assert_eq!(resp["result"]["daemon"], json!("ok"));

        client.writer.shutdown().await.unwrap();
        assert!(
            client.recv().await.is_none(),
            "commands/status must close after their response channel drains"
        );

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_client_stops_a_pending_writer_on_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let input_closed = Arc::new(Notify::new());
        let (handle, shutdown) = spawn(
            LocalApiServer::new(path.clone(), fake.clone())
                .with_max_connections(1)
                .with_input_closed(input_closed.clone()),
        );

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":"gone","method":"message.send","params":{"text":"hi"}}"#)
            .await;
        assert_eq!(
            client.recv().await.unwrap()["result"]["accepted"],
            json!(true)
        );
        let events = fake.event_txs.lock().unwrap()[0].clone();

        let input_closed = input_closed.notified();
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), input_closed)
            .await
            .expect("server must observe the closed client");

        // Wake the writer with a pending terminal event. Its write must fail,
        // close the frame channel, and release the connection slot.
        let _ = events.send(&ServerEvent::reply("default", "discarded"));
        let channel_closed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if events
                    .send(&ServerEvent::reply_delta("default", "probe"))
                    .is_err()
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(channel_closed.is_ok(), "write failure must close EventTx");
        drop(events);
        fake.event_txs.lock().unwrap().clear();

        let slot_released = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if connection_can_serve_status(&path).await {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(slot_released.is_ok(), "failed client must release its slot");

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_closes_a_half_closed_connection_with_pending_producer() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let fake = Arc::new(FakeHandler::default());
        let input_closed = Arc::new(Notify::new());
        let (handle, shutdown) = spawn(
            LocalApiServer::new(path.clone(), fake.clone()).with_input_closed(input_closed.clone()),
        );

        let mut client = TestClient::connect(&path).await;
        client
            .send(r#"{"id":"shutdown","method":"message.send","params":{"text":"hi"}}"#)
            .await;
        assert_eq!(
            client.recv().await.unwrap()["result"]["accepted"],
            json!(true)
        );
        let _pending_events = fake.event_txs.lock().unwrap()[0].clone();

        let input_closed = input_closed.notified();
        client.writer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), input_closed)
            .await
            .expect("server must observe the half-close");

        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("listener shutdown must not hang")
            .unwrap()
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .expect("connection shutdown must be observed")
            .is_none());
    }

    #[tokio::test]
    async fn the_connection_cap_rejects_extra_clients() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path).with_max_connections(1));

        let mut first = TestClient::connect(&path).await;
        first
            .send(r#"{"id":"a","method":"message.send","params":{"text":"hi"}}"#)
            .await;
        assert_eq!(
            first.recv().await.unwrap()["result"]["accepted"],
            json!(true)
        );

        let mut second = TestClient::connect(&path).await;
        let resp = second.recv().await.unwrap();
        assert_eq!(resp["error"]["code"], json!("too_many_connections"));
        assert!(
            second.recv().await.is_none(),
            "rejected client must be closed"
        );

        // The served client is unaffected.
        first.send(r#"{"id":"b","method":"status.get"}"#).await;
        assert_eq!(first.recv().await.unwrap()["result"]["daemon"], json!("ok"));

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_writer_failure_releases_the_connection_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(
            server(&path)
                .with_max_connections(1)
                .with_write_timeout(Duration::from_millis(100)),
        );

        let first = TestClient::connect(&path).await;
        let TestClient { reader, mut writer } = first;
        drop(reader);
        writer
            .write_all(b"{\"id\":\"gone\",\"method\":\"status.get\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();
        drop(writer);

        let mut served = false;
        for _ in 0..50 {
            let mut candidate = TestClient::connect(&path).await;
            match candidate.recv_for(Duration::from_millis(20)).await {
                Some(Some(response)) => {
                    assert_eq!(response["error"]["code"], json!("too_many_connections"));
                }
                Some(None) => {}
                None => {
                    candidate
                        .send(r#"{"id":"reused","method":"status.get"}"#)
                        .await;
                    if candidate.recv().await.unwrap()["result"]["daemon"] == json!("ok") {
                        served = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            served,
            "a failed writer must release the live connection slot"
        );

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_malformed_line_gets_an_error_and_the_connection_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path));

        let mut client = TestClient::connect(&path).await;
        client.send("this is not json").await;
        let resp = client.recv().await.unwrap();
        assert_eq!(resp["error"]["code"], json!("invalid_request"));
        assert_eq!(resp["id"], json!(""));

        // Same connection, next request works.
        client.send(r#"{"id":"ok","method":"commands.list"}"#).await;
        assert_eq!(client.recv().await.unwrap()["id"], json!("ok"));

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_final_line_without_newline_is_processed() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path));

        let mut client = TestClient::connect(&path).await;
        client
            .writer
            .write_all(br#"{"id":"partial","method":"status.get"}"#)
            .await
            .unwrap();
        client.writer.shutdown().await.unwrap();
        let response = client.recv().await.unwrap();
        assert_eq!(response["id"], json!("partial"));
        assert_eq!(response["result"]["daemon"], json!("ok"));

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unterminated_oversized_line_closes_without_waiting_for_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path).with_max_connections(1));

        let TestClient {
            mut reader,
            mut writer,
        } = TestClient::connect(&path).await;
        writer
            .write_all(br#"{"id":"continuous","method":"status.get","params":{"padding":""#)
            .await
            .unwrap();
        let producer = tokio::spawn(async move {
            let chunk = vec![b'a'; 4096];
            loop {
                if writer.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });

        let line = tokio::time::timeout(Duration::from_secs(1), reader.next_line())
            .await
            .expect("server must not wait for a newline after overflow")
            .unwrap()
            .expect("server should salvage an error response");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], json!("continuous"));
        assert_eq!(response["error"]["code"], json!("invalid_request"));

        let eof = tokio::time::timeout(Duration::from_secs(1), reader.next_line())
            .await
            .expect("overflowed connection must terminate")
            .unwrap();
        assert!(eof.is_none());
        tokio::time::timeout(Duration::from_secs(1), producer)
            .await
            .expect("continuous peer must observe the closed connection")
            .unwrap();

        let slot_released = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if connection_can_serve_status(&path).await {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            slot_released.is_ok(),
            "overflowed client must release its slot"
        );

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_oversized_line_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(&dir);
        let (handle, shutdown) = spawn(server(&path));

        let mut client = TestClient::connect(&path).await;
        client
            .send(&format!(
                r#"{{"id":"big","method":"message.send","params":{{"text":"{}"}}}}"#,
                "a".repeat(70 * 1024)
            ))
            .await;
        let resp = client.recv().await.unwrap();
        assert_eq!(resp["id"], json!("big"));
        assert_eq!(resp["error"]["code"], json!("invalid_request"));
        assert!(tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .unwrap()
            .is_none());

        shutdown.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }
}
