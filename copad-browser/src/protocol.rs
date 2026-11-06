use serde::{Deserialize, Serialize};

/// The `error` of a copad RPC response. Lives here because the browser rules refuse with it;
/// copad-core re-exports it as `protocol::ResponseError`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}
