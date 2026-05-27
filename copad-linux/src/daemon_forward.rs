//! Proxies unknown GUI-socket Requests to the daemon. Runs on worker
//! threads so the GTK timer that drives `socket::dispatch` is never
//! blocked on a slow daemon/plugin reply.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use std::time::Duration;

use copad_core::protocol::{Request, Response};
use copad_core::thread_pool::{Cancelable, ThreadPool};

const POOL_WORKERS: usize = 4;
const POOL_QUEUE: usize = 16;
/// > daemon's 120s outer timeout — wedged-pump safety net only.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(125);

static POOL: OnceLock<std::sync::Arc<ThreadPool>> = OnceLock::new();

fn pool() -> &'static std::sync::Arc<ThreadPool> {
    POOL.get_or_init(|| ThreadPool::new(POOL_WORKERS, POOL_QUEUE))
}

struct ForwardJob {
    request: Request,
    reply: Sender<Response>,
}

impl ForwardJob {
    fn send_response(reply: &Sender<Response>, resp: Response) {
        let _ = reply.send(resp);
    }
}

impl Cancelable for ForwardJob {
    fn run(self: Box<Self>) {
        let this = *self;
        let socket_path = match copad_core::paths::daemon_socket_path() {
            Some(p) => p,
            None => {
                Self::send_response(
                    &this.reply,
                    Response::error(
                        this.request.id.clone(),
                        "no_daemon",
                        "copadd not reachable (untrusted runtime dir or no daemon listening)",
                    ),
                );
                return;
            }
        };
        let resp = forward_once(&socket_path.to_string_lossy(), &this.request);
        Self::send_response(&this.reply, resp);
    }

    fn cancel(self: Box<Self>) {
        let this = *self;
        Self::send_response(
            &this.reply,
            Response::error(
                this.request.id.clone(),
                "overloaded",
                "GUI→daemon forward pool saturated; retry shortly",
            ),
        );
    }
}

fn forward_once(socket_path: &str, request: &Request) -> Response {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                request.id.clone(),
                "no_daemon",
                &format!("connect to copadd at {socket_path}: {e}"),
            );
        }
    };
    if stream.set_read_timeout(Some(FORWARD_TIMEOUT)).is_err() {
        return Response::error(
            request.id.clone(),
            "internal_error",
            "set_read_timeout failed",
        );
    }
    let line = match serde_json::to_string(request) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(
                request.id.clone(),
                "internal_error",
                &format!("serialize forwarded request: {e}"),
            );
        }
    };
    if writeln!(stream, "{line}").is_err() {
        return Response::error(
            request.id.clone(),
            "no_daemon",
            "daemon closed connection mid-write",
        );
    }
    let mut reader = BufReader::new(stream);
    let mut reply_line = String::new();
    match reader.read_line(&mut reply_line) {
        Ok(0) => Response::error(
            request.id.clone(),
            "no_daemon",
            "daemon closed connection before replying",
        ),
        Ok(_) => match serde_json::from_str::<Response>(reply_line.trim()) {
            Ok(resp) => resp,
            Err(e) => Response::error(
                request.id.clone(),
                "internal_error",
                &format!("daemon reply parse: {e}"),
            ),
        },
        Err(e) => Response::error(
            request.id.clone(),
            "no_daemon",
            &format!("read from daemon: {e}"),
        ),
    }
}

/// Returns immediately (caller never blocks). Reply on `reply` may be
/// success, daemon error, `no_daemon`, or `overloaded`; always echoes
/// the original request id.
pub fn forward(request: Request, reply: Sender<Response>) {
    let job = Box::new(ForwardJob { request, reply });
    if let Err(rejected) = pool().try_execute(job) {
        rejected.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::channel;
    use std::thread;

    fn fake_daemon<F>(handler: F) -> std::path::PathBuf
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        let dir = std::env::temp_dir().join(format!(
            "copad-forward-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let sp = socket_path.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    continue;
                }
                let reply = handler(line.trim());
                let mut s = stream;
                writeln!(s, "{reply}").unwrap();
            }
            drop(sp);
        });
        socket_path
    }

    #[test]
    fn forward_once_returns_daemon_response() {
        let sock = fake_daemon(|_line| {
            let resp = Response::success("r-1".to_string(), json!({ "ok": true }));
            serde_json::to_string(&resp).unwrap()
        });
        let req = Request::new("r-1", "echo.ping", json!({}));
        let resp = forward_once(&sock.to_string_lossy(), &req);
        assert!(resp.ok);
        assert_eq!(resp.id, "r-1");
    }

    #[test]
    fn forward_once_returns_no_daemon_when_socket_missing() {
        let req = Request::new("r-2", "echo.ping", json!({}));
        let resp = forward_once("/tmp/copad-no-such-socket-12345.sock", &req);
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "no_daemon");
    }

    #[test]
    fn cancel_writes_overloaded_response() {
        let (tx, rx) = channel::<Response>();
        let job = Box::new(ForwardJob {
            request: Request::new("r-3", "echo.ping", json!({})),
            reply: tx,
        });
        Cancelable::cancel(job);
        let resp = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(resp.error.unwrap().code, "overloaded");
    }
}
