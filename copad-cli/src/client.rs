use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use copad_core::protocol::{Request, Response};

pub fn send_command(
    socket_path: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<Response, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(socket_path)?;
    // Read timeout sits just above the supervisor's action timeout
    // (`DEFAULT_ACTION_TIMEOUT` in copad-linux/src/service_supervisor.rs,
    // currently 120s for LLM completions) so a slow service-plugin
    // call can return a structured `action_timeout` response over
    // the socket instead of failing at the transport layer. Keep
    // these in lockstep when the supervisor constant changes.
    stream.set_read_timeout(Some(Duration::from_secs(130)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let request = Request {
        id: uuid::Uuid::new_v4().to_string(),
        method: method.to_string(),
        params,
        target_client_id: None,
    };

    let mut writer = stream.try_clone()?;
    let line = serde_json::to_string(&request)?;
    writeln!(writer, "{line}")?;
    writer.flush()?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let response: Response = serde_json::from_str(&line)?;
        if response.id == request.id {
            return Ok(response);
        }
    }

    Err("No response received".into())
}

pub fn subscribe(socket_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let request = Request {
        id: uuid::Uuid::new_v4().to_string(),
        method: "event.subscribe".to_string(),
        params: serde_json::json!({}),
        target_client_id: None,
    };

    let mut writer = stream.try_clone()?;
    let line = serde_json::to_string(&request)?;
    writeln!(writer, "{line}")?;
    writer.flush()?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        println!("{line}");
    }

    Ok(())
}
