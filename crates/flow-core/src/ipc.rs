//! Daemon control-socket protocol: newline-delimited JSON request/response
//! frames, decoupled from the transport that carries them. On Unix the
//! transport is a Unix domain socket at [`socket_path`]; on Windows it's a
//! named pipe at `\\.\pipe\vzt-flow-daemon` (see the [`windows`] module) —
//! both implement a `connect`/`bind`-shaped pair against the same
//! [`Request`]/[`Response`] types and the same `send_*`/`read_*` framing
//! functions below (those only need `std::io::{Read, Write}`, not anything
//! transport-specific).
//!
//! Framing: one JSON object per line (`\n`-terminated). Kept intentionally
//! dumb — no length prefixes, no multiplexing — because the daemon serves
//! one request per connection and clients (the CLI, the MCP server) open a
//! fresh connection per call.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::history::HistoryEntry;

/// `~/.config/vzt-flow/daemon.sock`.
pub fn socket_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.sock"))
}

fn default_history_n() -> usize {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Toggle,
    Cancel,
    Listen {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        max_secs: Option<u64>,
    },
    Transcribe {
        path: String,
    },
    History {
        #[serde(default = "default_history_n")]
        n: usize,
    },
}

/// Single flat response shape shared by every command; each command only
/// populates the fields relevant to it. Kept flat (rather than an enum with
/// per-command payloads) so the newline-JSON wire format stays trivial to
/// hand-parse from the MCP server's TypeScript client too.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `status`: "idle" | "recording" | "transcribing".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_loaded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_loaded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `listen`/`transcribe`: the raw (dictionary-corrected, pre-cleanup) text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// `listen`/`transcribe`: the final text after the full pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<HistoryEntry>>,
}

impl Response {
    pub fn ok() -> Self {
        Self { ok: true, ..Default::default() }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), ..Default::default() }
    }
}

/// Serializes `req` as one JSON line and writes it (with a trailing `\n`,
/// flushed) to `w`.
pub fn write_request<W: Write>(w: &mut W, req: &Request) -> Result<()> {
    let mut line = serde_json::to_string(req).context("failed to serialize request")?;
    line.push('\n');
    w.write_all(line.as_bytes()).context("failed to write request")?;
    w.flush().context("failed to flush request")?;
    Ok(())
}

/// Reads one JSON line from `r` and parses it as a [`Request`]. Returns
/// `Ok(None)` on a clean EOF (peer closed without sending anything).
pub fn read_request<R: BufRead>(r: &mut R) -> Result<Option<Request>> {
    let mut line = String::new();
    let n = r.read_line(&mut line).context("failed to read request")?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(trimmed).context("failed to parse request JSON")?))
}

/// Serializes `resp` as one JSON line and writes it (with a trailing `\n`,
/// flushed) to `w`.
pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    let mut line = serde_json::to_string(resp).context("failed to serialize response")?;
    line.push('\n');
    w.write_all(line.as_bytes()).context("failed to write response")?;
    w.flush().context("failed to flush response")?;
    Ok(())
}

/// Reads one JSON line from `r` and parses it as a [`Response`].
pub fn read_response<R: BufRead>(r: &mut R) -> Result<Response> {
    let mut line = String::new();
    let n = r.read_line(&mut line).context("failed to read response")?;
    if n == 0 {
        anyhow::bail!("connection closed before a response was received");
    }
    let trimmed = line.trim_end();
    serde_json::from_str(trimmed).context("failed to parse response JSON")
}

// --- Unix domain socket transport ---------------------------------------
//
// This is the only transport implemented today. It is intentionally kept
// separate from the framing functions above (which only need
// `Read`/`BufRead`/`Write`) so a future Windows named-pipe transport only
// has to provide `bind`/`connect`/`is_alive`/`remove` equivalents — the
// request/response types and the line-framing code are reused unchanged.
#[cfg(unix)]
pub mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    /// Connects to the daemon socket at `path` for a single request/response
    /// round trip and returns the parsed response. Used by clients (CLI,
    /// tests); the MCP server has its own Node-side client instead.
    pub fn call(path: &Path, req: &Request, read_timeout: Option<Duration>) -> Result<Response> {
        let mut stream = UnixStream::connect(path)
            .with_context(|| format!("failed to connect to daemon socket at {}", path.display()))?;
        stream.set_read_timeout(read_timeout).context("failed to set read timeout")?;
        write_request(&mut stream, req)?;
        let mut reader = std::io::BufReader::new(stream);
        read_response(&mut reader)
    }

    /// Connect-tests `path`: `true` if a live listener accepted the
    /// connection (immediately dropped), `false` if nothing is listening —
    /// including the case where the path doesn't exist at all.
    pub fn is_alive(path: &Path) -> bool {
        UnixStream::connect(path).is_ok()
    }

    /// Removes `path` if it exists but nothing is listening on it (a stale
    /// socket file left behind by a daemon that didn't exit cleanly, e.g.
    /// after a crash or `kill -9`). No-ops if the path doesn't exist or is
    /// still live. Returns whether a stale file was actually removed.
    pub fn remove_if_stale(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        if is_alive(path) {
            return Ok(false);
        }
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket at {}", path.display()))?;
        Ok(true)
    }

    /// Binds the daemon listener at `path`, clearing a stale socket file
    /// first if present, and chmods it `0600` (owner read/write only — this
    /// socket accepts commands that can drive the microphone and read
    /// dictation history, so it must not be world-connectable).
    pub fn bind(path: &Path) -> Result<UnixListener> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if path.exists() {
            if is_alive(path) {
                anyhow::bail!(
                    "a daemon is already listening on {} — is another instance of vzt-flow running?",
                    path.display()
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket at {}", path.display()))?;
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("failed to bind daemon socket at {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {} to 0600", path.display()))?;
        Ok(listener)
    }

    /// Best-effort socket file cleanup, called on daemon exit.
    pub fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Runs `handler` against every connection accepted on `listener`,
    /// sequentially (one connection fully handled before the next is
    /// accepted — the daemon only ever does one thing at a time). Returns
    /// only if `accept` itself errors unrecoverably (e.g. the listener was
    /// closed).
    pub fn serve<F>(listener: &UnixListener, mut handler: F)
    where
        F: FnMut(Request) -> Response,
    {
        loop {
            let stream = match listener.accept() {
                Ok((s, _addr)) => s,
                Err(e) => {
                    eprintln!("[vzt-flow] daemon socket accept error: {e}");
                    continue;
                }
            };
            if let Err(e) = handle_one(stream, &mut handler) {
                eprintln!("[vzt-flow] daemon socket connection error: {e}");
            }
        }
    }

    fn handle_one<F>(stream: UnixStream, handler: &mut F) -> Result<()>
    where
        F: FnMut(Request) -> Response,
    {
        let mut reader = std::io::BufReader::new(stream.try_clone().context("failed to clone stream")?);
        let mut writer = stream;
        match read_request(&mut reader) {
            Ok(Some(req)) => {
                let resp = handler(req);
                write_response(&mut writer, &resp)?;
            }
            Ok(None) => {} // peer connected and disconnected without sending anything
            Err(e) => {
                let resp = Response::err(format!("bad request: {e}"));
                let _ = write_response(&mut writer, &resp);
            }
        }
        Ok(())
    }
}

// --- Windows named-pipe transport ---------------------------------------
//
// Mirrors `unix` above but over a native Windows named pipe rather than a
// Unix domain socket, via the `interprocess` crate's `local_socket`
// abstraction (sync API, no extra feature flags needed — `async`/`tokio`
// are opt-in and we don't enable them). `interprocess` was chosen over
// hand-rolling `CreateNamedPipeW`/`ConnectNamedPipe` via the `windows`
// crate because it already wraps the native named-pipe handle in
// `std::io::{Read, Write}`, which is exactly the shape `write_request`/
// `read_request`/`write_response`/`read_response` above need — hand-rolling
// would just be re-deriving that same wrapper. It's listed on crates.io as
// "maintenance: passive" (feature-complete, not under active development)
// rather than abandoned; the sync `local_socket` API this module uses is
// small and stable enough that this is an acceptable trade for not owning
// ~150 lines of raw Win32 FFI.
//
// Pipe name/path: `\\.\pipe\vzt-flow-daemon`, current-user scoped by
// default Windows named-pipe ACLs (a pipe created by a normal process is
// only connectable by the same user session unless explicitly given a
// custom security descriptor, which this code does not do) — equivalent in
// spirit to the Unix socket's `0600` chmod in `unix::bind`.
#[cfg(windows)]
pub mod windows {
    use super::*;
    use interprocess::local_socket::{
        traits::{ListenerExt, Stream as _},
        GenericNamespaced, ListenerOptions, Name, Stream, ToNsName,
    };
    use std::io::BufReader;
    use std::path::Path;
    use std::time::Duration;

    pub use interprocess::local_socket::Listener;

    /// The pipe's base name; `interprocess` maps this onto the full
    /// `\\.\pipe\vzt-flow-daemon` path via [`GenericNamespaced`]. Matches
    /// the Unix socket's filename for consistency across docs/tooling.
    pub const PIPE_NAME: &str = "vzt-flow-daemon";

    /// Everything below is parameterized over the pipe's base name (rather
    /// than hardcoding [`PIPE_NAME`] throughout) purely so `windows_tests`
    /// can exercise this module with per-test pipe names instead of racing
    /// each other — and the real daemon and its stale-listener guard —
    /// on the one shared production name. The public `bind`/`call`/
    /// `is_alive` wrappers below always pass `PIPE_NAME`.
    fn pipe_name_for(name: &'static str) -> Result<Name<'static>> {
        name.to_ns_name::<GenericNamespaced>().context("failed to build the vzt-flow daemon pipe name")
    }

    pub(crate) fn connect_test(name: &'static str) -> bool {
        match pipe_name_for(name) {
            Ok(n) => Stream::connect(n).is_ok(),
            Err(_) => false,
        }
    }

    /// Connect-tests the daemon pipe: `true` if a live listener accepted
    /// the connection (immediately dropped), `false` otherwise. `_path` is
    /// accepted-but-unused so this has the same signature as
    /// `unix::is_alive` and can be re-exported from [`transport`] unchanged
    /// by callers that pass `socket_path()` generically.
    pub fn is_alive(_path: &Path) -> bool {
        connect_test(PIPE_NAME)
    }

    /// No-op, unlike `unix::remove_if_stale`: a named pipe is a pure kernel
    /// object with no filesystem entry to leak. When the owning process
    /// exits — cleanly or via a hard kill — Windows reclaims the pipe
    /// automatically, so there is no "stale pipe file" to clean up.
    pub fn remove_if_stale(_path: &Path) -> Result<bool> {
        Ok(false)
    }

    pub(crate) fn call_named(name: &'static str, req: &Request, read_timeout: Option<Duration>) -> Result<Response> {
        let pipe = pipe_name_for(name)?;
        let mut stream = Stream::connect(pipe)
            .with_context(|| format!("failed to connect to daemon named pipe \\\\.\\pipe\\{name}"))?;
        stream.set_recv_timeout(read_timeout).context("failed to set read timeout")?;
        write_request(&mut stream, req)?;
        let mut reader = BufReader::new(stream);
        read_response(&mut reader)
    }

    /// Connects to the daemon pipe for a single request/response round
    /// trip and returns the parsed response. `_path` is accepted-but-unused
    /// (see [`is_alive`]).
    pub fn call(_path: &Path, req: &Request, read_timeout: Option<Duration>) -> Result<Response> {
        call_named(PIPE_NAME, req, read_timeout)
    }

    pub(crate) fn bind_named(name: &'static str) -> Result<Listener> {
        // Unlike `UnixListener::bind`, Windows named pipes normally allow
        // multiple listener instances to register under the same name at
        // once (that's the OS's standard multi-instance pipe-server model,
        // meant for concurrent-accept pools) — `ListenerOptions::create_sync`
        // would silently succeed even with another vzt-flow daemon already
        // serving. So the "already running" guard `unix::bind` gets for
        // free from `UnixListener::bind` is reimplemented explicitly here
        // via a connect-test first. Same check-then-act race window as the
        // Unix version (not atomic), which is an accepted trade there too.
        if connect_test(name) {
            anyhow::bail!(
                "a daemon is already listening on \\\\.\\pipe\\{name} — is another instance of vzt-flow running?"
            );
        }
        let pipe = pipe_name_for(name)?;
        ListenerOptions::new()
            .name(pipe)
            .create_sync()
            .with_context(|| format!("failed to bind daemon named pipe \\\\.\\pipe\\{name}"))
    }

    /// Binds the daemon's named-pipe listener at [`PIPE_NAME`].
    pub fn bind() -> Result<Listener> {
        bind_named(PIPE_NAME)
    }

    /// Runs `handler` against every connection accepted on `listener`,
    /// sequentially — same one-request-per-connection, one-thing-at-a-time
    /// contract as `unix::serve`.
    pub fn serve<F>(listener: &Listener, mut handler: F)
    where
        F: FnMut(Request) -> Response,
    {
        for conn in listener.incoming() {
            let stream = match conn {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[vzt-flow] daemon pipe accept error: {e}");
                    continue;
                }
            };
            if let Err(e) = handle_one(stream, &mut handler) {
                eprintln!("[vzt-flow] daemon pipe connection error: {e}");
            }
        }
    }

    fn handle_one<F>(stream: Stream, handler: &mut F) -> Result<()>
    where
        F: FnMut(Request) -> Response,
    {
        // Unlike `unix::handle_one` (which clones the stream fd to hold an
        // independent reader/writer pair), `local_socket::Stream` is used
        // here the same way the crate's own examples do: own it inside a
        // `BufReader` for buffered reads, and write back out through
        // `BufReader::get_mut` (which doesn't itself implement `Write`).
        let mut reader = BufReader::new(stream);
        match read_request(&mut reader) {
            Ok(Some(req)) => {
                let resp = handler(req);
                write_response(reader.get_mut(), &resp)?;
            }
            Ok(None) => {} // peer connected and disconnected without sending anything
            Err(e) => {
                let resp = Response::err(format!("bad request: {e}"));
                let _ = write_response(reader.get_mut(), &resp);
            }
        }
        Ok(())
    }
}

/// Cross-platform "is a daemon reachable, and can we round-trip a request"
/// facade used by CLI commands (`daemon_client`, `doctor`) that don't need
/// the rest of `unix`'s/`windows`'s bind/serve surface. Re-exports the
/// platform-appropriate transport; on a target that is neither Unix nor
/// Windows, every call reports "not running"/"not supported" instead of
/// failing to compile — callers already have a standalone (no-daemon)
/// fallback path for exactly this case.
#[cfg(unix)]
pub mod transport {
    pub use super::unix::{call, is_alive};
}

#[cfg(windows)]
pub mod transport {
    pub use super::windows::{call, is_alive};
}

#[cfg(not(any(unix, windows)))]
pub mod transport {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    pub fn is_alive(_path: &Path) -> bool {
        false
    }

    pub fn call(_path: &Path, _req: &Request, _read_timeout: Option<Duration>) -> Result<Response> {
        anyhow::bail!(
            "the vzt-flow daemon control socket is not supported on this platform yet; \
             use the standalone CLI commands instead (they don't require a running daemon)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_round_trips_status() {
        let mut buf = Vec::new();
        write_request(&mut buf, &Request::Status).unwrap();
        assert!(buf.ends_with(b"\n"));
        let mut reader = std::io::BufReader::new(Cursor::new(buf));
        let parsed = read_request(&mut reader).unwrap().unwrap();
        matches!(parsed, Request::Status);
    }

    #[test]
    fn request_round_trips_listen_with_all_fields() {
        let req = Request::Listen {
            mode: Some("code".to_string()),
            timeout_secs: Some(30),
            max_secs: Some(20),
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut reader = std::io::BufReader::new(Cursor::new(buf));
        let parsed = read_request(&mut reader).unwrap().unwrap();
        match parsed {
            Request::Listen { mode, timeout_secs, max_secs } => {
                assert_eq!(mode.as_deref(), Some("code"));
                assert_eq!(timeout_secs, Some(30));
                assert_eq!(max_secs, Some(20));
            }
            other => panic!("expected Listen, got {other:?}"),
        }
    }

    #[test]
    fn request_listen_defaults_optional_fields_to_none() {
        let json = r#"{"cmd":"listen"}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        match parsed {
            Request::Listen { mode, timeout_secs, max_secs } => {
                assert_eq!(mode, None);
                assert_eq!(timeout_secs, None);
                assert_eq!(max_secs, None);
            }
            other => panic!("expected Listen, got {other:?}"),
        }
    }

    #[test]
    fn request_history_defaults_n_to_20() {
        let json = r#"{"cmd":"history"}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        match parsed {
            Request::History { n } => assert_eq!(n, 20),
            other => panic!("expected History, got {other:?}"),
        }
    }

    #[test]
    fn read_request_returns_none_on_empty_input() {
        let mut reader = std::io::BufReader::new(Cursor::new(Vec::<u8>::new()));
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn read_request_errors_on_malformed_json() {
        let mut reader = std::io::BufReader::new(Cursor::new(b"{not json\n".to_vec()));
        assert!(read_request(&mut reader).is_err());
    }

    #[test]
    fn response_round_trips_and_omits_absent_fields() {
        let resp = Response { ok: true, state: Some("idle".to_string()), ..Default::default() };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        let json = String::from_utf8(buf.clone()).unwrap();
        assert!(json.contains("\"state\":\"idle\""));
        assert!(!json.contains("\"error\""));
        assert!(!json.contains("\"raw\""));

        let mut reader = std::io::BufReader::new(Cursor::new(buf));
        let parsed = read_response(&mut reader).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.state.as_deref(), Some("idle"));
    }

    #[test]
    fn error_response_carries_message() {
        let resp = Response::err("already recording");
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("already recording"));
    }

    #[test]
    fn read_response_errors_on_closed_connection() {
        let mut reader = std::io::BufReader::new(Cursor::new(Vec::<u8>::new()));
        assert!(read_response(&mut reader).is_err());
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::unix::*;
    use super::*;
    use std::time::Duration;

    fn tmp_socket_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "vzt-flow-ipc-test-{name}-{}-{}.sock",
            std::process::id(),
            name.len() // trivial extra uniqueness without adding a rand dep
        ))
    }

    #[test]
    fn is_alive_false_for_nonexistent_path() {
        let path = tmp_socket_path("nonexistent");
        assert!(!is_alive(&path));
    }

    #[test]
    fn remove_if_stale_removes_a_dead_socket_file() {
        let path = tmp_socket_path("stale");
        // Bind and immediately drop the listener: the socket file remains
        // on disk (Unix sockets aren't auto-cleaned on drop) but nothing is
        // listening on it anymore — a realistic stale-socket scenario.
        {
            let _listener = bind(&path).unwrap();
        }
        assert!(path.exists());
        assert!(!is_alive(&path));
        assert!(remove_if_stale(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn remove_if_stale_leaves_a_live_socket_alone() {
        let path = tmp_socket_path("live");
        let listener = bind(&path).unwrap();
        assert!(is_alive(&path));
        assert!(!remove_if_stale(&path).unwrap());
        assert!(path.exists());
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bind_rejects_a_second_listener_on_a_live_socket() {
        let path = tmp_socket_path("double-bind");
        let listener = bind(&path).unwrap();
        assert!(bind(&path).is_err());
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bind_reclaims_a_stale_socket_path() {
        let path = tmp_socket_path("reclaim");
        {
            let _listener = bind(&path).unwrap();
        }
        // Stale file left behind; a fresh bind must succeed by clearing it.
        let listener = bind(&path).unwrap();
        assert!(is_alive(&path));
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn socket_is_chmod_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_socket_path("perms");
        let listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn serve_answers_a_single_status_request() {
        let path = tmp_socket_path("serve");
        let listener = bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            // Handle exactly one connection then return.
            let (conn, _addr) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
            let mut writer = conn;
            let req = read_request(&mut reader).unwrap().unwrap();
            matches!(req, Request::Status);
            let resp = Response { ok: true, state: Some("idle".to_string()), ..Default::default() };
            write_response(&mut writer, &resp).unwrap();
        });

        // Give the acceptor a moment to be ready; connect + call.
        std::thread::sleep(Duration::from_millis(20));
        let resp = call(&path, &Request::Status, Some(Duration::from_secs(2))).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.state.as_deref(), Some("idle"));

        handle.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::windows::{
        bind_named as test_bind_named, call_named as test_call_named, connect_test as test_connect_test, PIPE_NAME,
    };
    use super::*;
    use std::time::Duration;

    /// Leaks a unique per-test pipe base name so parallel `cargo test`
    /// threads (and the tests below, which each bind/connect a listener)
    /// don't collide on the same pipe the way they would if every test
    /// used the production [`PIPE_NAME`] — see `windows::pipe_name_for`'s
    /// doc comment for why this module is parameterized over the name at
    /// all. `Box::leak` is fine here: it's test-only and reclaimed at
    /// process exit.
    fn tmp_pipe_name(name: &str) -> &'static str {
        Box::leak(format!("vzt-flow-ipc-test-{name}-{}-{}", std::process::id(), name.len()).into_boxed_str())
    }

    #[test]
    fn pipe_name_constant_matches_documented_path() {
        // Sanity check the constant this module (and USAGE-Windows.md)
        // build `\\.\pipe\vzt-flow-daemon` from hasn't drifted.
        assert_eq!(PIPE_NAME, "vzt-flow-daemon");
    }

    #[test]
    fn is_alive_false_for_nonexistent_pipe() {
        let name = tmp_pipe_name("nonexistent");
        assert!(!test_connect_test(name));
    }

    #[test]
    fn bind_rejects_a_second_listener_on_a_live_pipe() {
        let name = tmp_pipe_name("double-bind");
        let listener = test_bind_named(name).unwrap();
        assert!(test_connect_test(name));
        assert!(test_bind_named(name).is_err());
        drop(listener);
    }

    #[test]
    fn remove_if_stale_is_a_no_op() {
        // Named pipes have no filesystem entry to leak (see the doc
        // comment on `windows::remove_if_stale`); this just locks in that
        // the function never claims to have removed anything.
        assert!(!windows::remove_if_stale(std::path::Path::new("unused")).unwrap());
    }

    #[test]
    fn serve_answers_a_single_status_request() {
        let name = tmp_pipe_name("serve");
        let listener = test_bind_named(name).unwrap();
        let handle = std::thread::spawn(move || {
            // Handle exactly one connection then return.
            windows::serve(&listener, |req| {
                matches!(req, Request::Status);
                Response { ok: true, state: Some("idle".to_string()), ..Default::default() }
            });
        });

        // Give the acceptor a moment to be ready; connect + call.
        std::thread::sleep(Duration::from_millis(20));
        let resp = test_call_named(name, &Request::Status, Some(Duration::from_secs(2))).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.state.as_deref(), Some("idle"));

        // `windows::serve` loops forever (mirrors `unix::serve`'s
        // contract), so the spawned thread never returns on its own once
        // it's answered our one connection — nothing left to join here,
        // it's cleaned up when the test process exits.
        drop(handle);
    }
}
