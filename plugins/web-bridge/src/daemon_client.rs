//! Raw daemon-socket client, mirroring `copad-cli/src/client.rs` over `tokio::net::UnixStream`.
//!
//! Two access patterns: a one-shot RPC (open, send a `Request`, read the matching `Response`,
//! close; the daemon dispatch is thread-safe and connect is cheap, so there is no pool), and a
//! long-lived `event.subscribe` whose event lines are forwarded to an `mpsc::Sender<Value>`
//! until the channel closes or the socket dies (`/ws/events` fans one subscription out).
//!
//! Requests use `copad_core::protocol::{Request, Response}` so a daemon protocol change is a
//! compile error here too.

use std::path::PathBuf;
use std::sync::Arc;

use copad_core::protocol::{Request, Response};
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

    /// Round-trip a single Request/Response, reading until the response with our id arrives and
    /// ignoring anything else the daemon interleaves on the connection.
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

    /// Open a long-lived `event.subscribe` connection, forwarding every line that parses as JSON
    /// (events or the subscribe ack) to `sink` until the receiver closes or the socket dies.
    ///
    /// Uses `std::os::unix::net::UnixStream` in `spawn_blocking`, mirroring `copad-cli`. A tokio
    /// `UnixStream + into_split` version connected and wrote but never saw a line on read.
    /// Blocking I/O stays on the blocking pool so axum handlers stay responsive.
    pub async fn subscribe(
        &self,
        patterns: Vec<String>,
        sink: mpsc::Sender<Value>,
    ) -> Result<(), DaemonError> {
        let socket_path = self.socket_path.clone();
        // Shared stop flag: dropping `_guard` on task abort (WS client closed) flips it, and the
        // reader checks it between read timeouts. Without it the `spawn_blocking` thread sits in
        // read_line on an idle daemon, leaking the connection and a pool slot after
        // `subscribe_task.abort()`.
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
            // Short read timeout so the loop polls the stop flag about twice a second.
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

/// RAII handle that flips an `AtomicBool` on drop, telling the `spawn_blocking` reader in
/// `DaemonClient::subscribe` to stop when the outer future is aborted or completes.
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
