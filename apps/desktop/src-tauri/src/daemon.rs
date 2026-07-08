//! The daemon control socket: a Unix domain socket at
//! `~/.config/vzt-flow/daemon.sock` that lets the CLI (and, via it, the MCP
//! server) drive this running app instance — status, toggle, cancel, a
//! record-and-return-text `listen`, file transcription, and history.
//!
//! Framing/transport live in `flow_core::ipc`; this module is just the
//! glue between that generic request/response loop and this app's
//! `AppState`/coordinator.

use std::sync::mpsc;
use std::time::Duration;

use flow_core::ipc::{unix, Request, Response};
use flow_core::model_manager::ModelCommand;
use tauri::{AppHandle, Manager};

use crate::coordinator::CoordinatorMsg;
use crate::state::{AppState, ModelLifecycle};

/// Spawns the socket-accept loop on a dedicated thread. Binding happens
/// synchronously (so a bind failure surfaces immediately, before `setup()`
/// returns) and the loop itself runs for the lifetime of the process.
pub fn spawn(app: AppHandle) -> anyhow::Result<()> {
    let path = flow_core::ipc::socket_path()?;
    let listener = unix::bind(&path)?;
    eprintln!("[vzt-flow] daemon socket listening at {}", path.display());

    std::thread::Builder::new()
        .name("vzt-flow-daemon-socket".into())
        .spawn(move || {
            unix::serve(&listener, |req| handle_request(&app, req));
        })
        .expect("failed to spawn daemon socket thread");
    Ok(())
}

/// Best-effort cleanup on shutdown — removes the socket file so a stale
/// entry doesn't cause the next launch's `bind` to think a daemon is still
/// alive (the `bind`/connect-test dance handles that anyway, but removing
/// it here keeps `flow doctor` from reporting a bogus stale-file warning
/// between a clean quit and the next launch).
pub fn cleanup() {
    if let Ok(path) = flow_core::ipc::socket_path() {
        unix::cleanup(&path);
    }
}

fn handle_request(app: &AppHandle, req: Request) -> Response {
    match req {
        Request::Status => handle_status(app),
        Request::Toggle => handle_toggle(app),
        Request::Cancel => handle_cancel(app),
        Request::Listen { mode, timeout_secs, max_secs } => handle_listen(app, mode, timeout_secs, max_secs),
        Request::Transcribe { path } => handle_transcribe(app, &path),
        Request::History { n } => handle_history(n),
    }
}

fn handle_status(app: &AppHandle) -> Response {
    let state = app.state::<AppState>();
    let ds = *state.dictation_state.lock().unwrap();
    let model_loaded = *state.model_lifecycle.lock().unwrap() == ModelLifecycle::Loaded;
    let cleanup_loaded = *state.cleanup_lifecycle.lock().unwrap() == ModelLifecycle::Loaded;
    Response {
        ok: true,
        state: Some(ds.daemon_label().to_string()),
        model_loaded: Some(model_loaded),
        cleanup_loaded: Some(cleanup_loaded),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        ..Default::default()
    }
}

fn handle_toggle(app: &AppHandle) -> Response {
    let Some(tx) = app.state::<AppState>().coordinator_tx.lock().unwrap().clone() else {
        return Response::err("coordinator not ready");
    };
    if tx.send(CoordinatorMsg::TrayToggleDictation).is_err() {
        return Response::err("coordinator channel closed");
    }
    // Best-effort: the toggle is processed asynchronously on the
    // coordinator thread; give it a moment so the reported state reflects
    // the transition rather than the pre-toggle state.
    std::thread::sleep(Duration::from_millis(80));
    let ds = *app.state::<AppState>().dictation_state.lock().unwrap();
    Response { ok: true, state: Some(ds.daemon_label().to_string()), ..Default::default() }
}

fn handle_cancel(app: &AppHandle) -> Response {
    let Some(tx) = app.state::<AppState>().coordinator_tx.lock().unwrap().clone() else {
        return Response::err("coordinator not ready");
    };
    if tx.send(CoordinatorMsg::Hotkey(flow_core::hotkey::HotkeyEvent::CancelRequested)).is_err() {
        return Response::err("coordinator channel closed");
    }
    std::thread::sleep(Duration::from_millis(80));
    let ds = *app.state::<AppState>().dictation_state.lock().unwrap();
    Response { ok: true, state: Some(ds.daemon_label().to_string()), ..Default::default() }
}

fn handle_listen(app: &AppHandle, mode: Option<String>, timeout_secs: Option<u64>, max_secs: Option<u64>) -> Response {
    let Some(tx) = app.state::<AppState>().coordinator_tx.lock().unwrap().clone() else {
        return Response::err("coordinator not ready");
    };

    // Both fields bound the recording's hard duration cap; when both are
    // given, the tighter one wins.
    let effective_cap = match (max_secs, timeout_secs) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let (reply_tx, reply_rx) = mpsc::channel();
    let sent = tx.send(CoordinatorMsg::DaemonListen { mode, max_secs: effective_cap, reply: reply_tx });
    if sent.is_err() {
        return Response::err("coordinator channel closed");
    }

    // The coordinator only replies once the recording has stopped (via VAD
    // auto-stop or the duration cap) and the full pipeline has finished, so
    // this can legitimately take up to ~`effective_cap` seconds plus
    // transcription/cleanup time. Bound the wait generously rather than
    // block forever if something upstream wedges.
    let wait_budget = Duration::from_secs(effective_cap.unwrap_or(300) + 60);
    match reply_rx.recv_timeout(wait_budget) {
        Ok(Ok(outcome)) => Response {
            ok: true,
            raw: Some(outcome.raw),
            text: Some(outcome.text),
            mode: Some(outcome.mode),
            duration_s: Some(outcome.duration_s),
            ..Default::default()
        },
        Ok(Err(e)) => Response::err(e),
        Err(_) => Response::err("timed out waiting for the recording/pipeline to finish"),
    }
}

fn handle_transcribe(app: &AppHandle, path: &str) -> Response {
    let (samples, duration) = match flow_core::audio::load_audio_file_as_f32(std::path::Path::new(path)) {
        Ok(v) => v,
        Err(e) => return Response::err(format!("failed to load {path}: {e}")),
    };

    let Some(model_cmd_tx) = app.state::<AppState>().model_cmd_tx.lock().unwrap().clone() else {
        return Response::err("transcriber not ready");
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    let sent = model_cmd_tx.send(ModelCommand::Transcribe { samples, audio_duration: duration, reply: reply_tx });
    if sent.is_err() {
        return Response::err("transcriber channel closed");
    }
    let transcript = match reply_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Response::err(e),
        Err(_) => return Response::err("transcription timed out"),
    };

    let dict = app.state::<AppState>().dictionary.lock().unwrap().clone();
    let corrected = flow_core::dictionary::correct(&transcript.text, &dict);
    Response {
        ok: true,
        raw: Some(transcript.text),
        text: Some(corrected),
        duration_s: Some(duration.as_secs_f64()),
        ..Default::default()
    }
}

fn handle_history(n: usize) -> Response {
    match flow_core::history::read_recent(n) {
        Ok(entries) => Response { ok: true, history: Some(entries), ..Default::default() },
        Err(e) => Response::err(format!("failed to read history: {e}")),
    }
}
