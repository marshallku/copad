//! Raw daemon-socket client. Mirrors `nestty-cli/src/client.rs` but
//! over `tokio::net::UnixStream` so it composes with the axum runtime.
//!
//! Two access patterns are exposed:
//!
//! 1. **One-shot RPC** — open connection, send a `Request`, read the
//!    matching `Response`, close. The daemon dispatch path is
//!    thread-safe and connect is cheap, so each axum handler that
//!    needs to call a daemon method just opens a fresh socket. No
//!    connection pool yet; if the dashboard starts hammering the
//!    daemon we can revisit.
//!
//! 2. **Long-lived subscription** — open connection, send
//!    `event.subscribe { patterns }`, then forward every event line
//!    to an `mpsc::Sender<Value>` until the channel closes or the
//!    socket dies. Used by `/ws/events` to fan one daemon
//!    subscription out to multiple WebSocket clients.
//!
//! Requests use the canonical `nestty_core::protocol::{Request,
//! Response}` shape so a daemon protocol change shows up as a
//! compile error in this plugin too — no parallel structs.

use std::path::PathBuf;
use std::sync::Arc;

use nestty_core::protocol::{Request, Response};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: Arc<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("daemon closed connection before responding")]
    Closed,
    #[error("daemon error [{code}]: {message}")]
    Daemon { code: String, message: String },
}

impl DaemonClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: Arc::new(socket_path.into()),
        }
    }

    /// Round-trip a single Request/Response. The daemon may interleave
    /// `<method>.completed` events on the same socket connection
    /// (because action dispatch publishes on the bus and forwarders
    /// pick them up), but those don't get this connection — generic
    /// clients aren't subscribed unless they explicitly call
    /// `event.subscribe`. So we just read until the response with our
    /// id arrives, ignoring anything else.
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, DaemonError> {
        let req = Request {
            id: uuid::Uuid::new_v4().to_string(),
            method: method.to_string(),
            params,
            target_client_id: None,
        };
        let stream = UnixStream::connect(self.socket_path.as_path()).await?;
        let (read_half, mut write_half) = stream.into_split();
        let line = serde_json::to_string(&req)?;
        write_half.write_all(line.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;

        let mut reader = BufReader::new(read_half).lines();
        while let Some(line) = reader.next_line().await? {
            if line.is_empty() {
                continue;
            }
            let resp: Response = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => continue, // event/garbage line; ignore
            };
            if resp.id != req.id {
                continue;
            }
            if resp.ok {
                return Ok(resp.result.unwrap_or(Value::Null));
            } else if let Some(err) = resp.error {
                return Err(DaemonError::Daemon {
                    code: err.code,
                    message: err.message,
                });
            } else {
                return Err(DaemonError::Daemon {
                    code: "unknown".into(),
                    message: "daemon reported failure with no error body".into(),
                });
            }
        }
        Err(DaemonError::Closed)
    }

    /// Open a long-lived `event.subscribe` connection. Reads lines
    /// forever and forwards everything that parses as JSON (Event
    /// frames OR the initial subscribe ack) to `sink`. Returns when
    /// the receiver is closed or the socket dies.
    ///
    /// Implemented over `std::os::unix::net::UnixStream` inside
    /// `tokio::task::spawn_blocking`, mirroring `nestty-cli/src/client.rs`
    /// (which is the validated working path against this same daemon
    /// socket protocol). An earlier tokio `UnixStream + into_split`
    /// version connected and wrote successfully but never observed
    /// any line on read; rather than debug that further inside Slice
    /// 3.0, this matches the known-good model. Blocking I/O lives on
    /// the blocking pool, not on the runtime workers, so axum's HTTP
    /// handlers stay snappy.
    pub async fn subscribe(
        &self,
        patterns: Vec<String>,
        sink: mpsc::Sender<Value>,
    ) -> Result<(), DaemonError> {
        let socket_path = self.socket_path.clone();
        // Shared stop flag — the outer async future drops `_guard` on
        // task abort (WS client closed), which flips the flag; the
        // blocking reader's polling loop checks it between socket
        // read-timeouts and exits cleanly. Without this the
        // `spawn_blocking` thread can sit forever inside read_line
        // when the daemon is idle, leaking the socket connection +
        // blocking-pool slot even after `subscribe_task.abort()`.
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_blocking = stop.clone();
        let _guard = StopOnDrop(stop);

        let join = tokio::task::spawn_blocking(move || -> Result<(), DaemonError> {
            use std::io::{BufRead, Write};
            use std::os::unix::net::UnixStream as StdUnix;
            use std::time::Duration;
            let req = Request {
                id: uuid::Uuid::new_v4().to_string(),
                method: "event.subscribe".to_string(),
                params: serde_json::json!({ "patterns": patterns }),
                target_client_id: None,
            };
            let stream = StdUnix::connect(socket_path.as_path())?;
            // Short read timeout so the loop polls the stop flag
            // ~twice a second. 500ms is the sweet spot — frequent
            // enough to make WS close + reconnect feel snappy, rare
            // enough that idle CPU is negligible.
            stream.set_read_timeout(Some(Duration::from_millis(500)))?;
            let mut writer = stream.try_clone()?;
            let line = serde_json::to_string(&req)?;
            writeln!(writer, "{line}")?;
            writer.flush()?;
            let mut reader = std::io::BufReader::new(stream);
            let mut buf = String::new();
            loop {
                if stop_for_blocking.load(Ordering::Acquire) {
                    return Ok(());
                }
                buf.clear();
                match reader.read_line(&mut buf) {
                    Ok(0) => return Err(DaemonError::Closed),
                    Ok(_) => {
                        let line = buf.trim_end_matches('\n');
                        if line.is_empty() {
                            continue;
                        }
                        let v: Value = match serde_json::from_str(line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if sink.blocking_send(v).is_err() {
                            return Ok(());
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        })
        .await;
        match join {
            Ok(inner) => inner,
            Err(e) => Err(DaemonError::Io(std::io::Error::other(format!(
                "subscribe task join: {e}"
            )))),
        }
    }
}

/// RAII handle that flips an `AtomicBool` on drop. Used to signal
/// the `spawn_blocking` reader inside `DaemonClient::subscribe` to
/// terminate when the outer async future is aborted or completes.
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
