//! Shared "talk to the running daemon over its control socket" helper used
//! by `status`/`toggle`/`cancel`/`history`/`listen`/`doctor`.

use std::time::Duration;

use anyhow::Result;
use flow_core::ipc::{transport, Request, Response};

/// Best-effort check for "is a daemon currently listening". `false` covers
/// both "never started" and "stale socket file left behind" (Unix) or "no
/// listener on the pipe" (Windows) — callers that need to distinguish the
/// platform-specific cases can call `flow_core::ipc::unix::is_alive` /
/// `flow_core::ipc::windows::is_alive` directly.
pub fn is_daemon_running() -> bool {
    match flow_core::ipc::socket_path() {
        Ok(path) => transport::is_alive(&path),
        Err(_) => false,
    }
}

/// Sends `req` to the daemon and returns its response, or `None` if no
/// daemon is reachable (so callers can fall back to a standalone path).
/// `read_timeout` bounds how long to wait for a reply once connected —
/// callers doing a `listen` should pass something generous since the
/// daemon blocks on the full record+transcribe+cleanup pipeline before
/// replying.
pub fn call(req: &Request, read_timeout: Option<Duration>) -> Option<Response> {
    let path = flow_core::ipc::socket_path().ok()?;
    transport::call(&path, req, read_timeout).ok()
}

/// Same as [`call`] but surfaces the connection error instead of swallowing
/// it, for commands where "daemon not running" should be reported as an
/// explicit error rather than silently doing nothing.
pub fn call_required(req: &Request, read_timeout: Option<Duration>) -> Result<Response> {
    let path = flow_core::ipc::socket_path()?;
    transport::call(&path, req, read_timeout)
}
