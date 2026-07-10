//! Model download + on-disk layout management.
//!
//! Layout: `~/.config/vzt-flow/models/parakeet-v3/` holds the extracted
//! ONNX files (encoder/decoder/preprocessor + vocab.txt) that
//! `ParakeetModel::load` expects.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};

/// Same archive Handy (github.com/cjpais/Handy) downloads for its
/// "Parakeet V3" model — a pre-packaged Parakeet TDT 0.6B v3 (int8 ONNX).
const PARAKEET_V3_URL: &str = "https://blob.handy.computer/parakeet-v3-int8.tar.gz";
/// sha256 of the `parakeet-v3-int8.tar.gz` archive (478,517,071 bytes),
/// computed against the live URL. Wired into `download_verified` as a hard
/// gate: a truncated/corrupt archive is deleted before it can be extracted,
/// so a bad download can never leave a broken model dir in place.
const PARAKEET_V3_SHA256: &str = "43d37191602727524a7d8c6da0eef11c4ba24320f5b4730f1a2497befc2efa77";

pub fn model_root_dir() -> Result<PathBuf> {
    // Shares `config::config_dir()`'s platform split: literal
    // `~/.config/vzt-flow` on macOS, `%APPDATA%\vzt-flow` on Windows.
    Ok(crate::config::config_dir()?.join("models"))
}

pub fn parakeet_model_dir() -> Result<PathBuf> {
    Ok(model_root_dir()?.join("parakeet-v3"))
}

/// The set of files `ParakeetModel::load` needs to find in the model dir.
/// Used by `flow doctor` / download verification. Exact quantized filenames
/// vary (transcribe-rs resolves `{name}.int8.onnx` and falls back to
/// `{name}.onnx`), so we just check that *an* onnx variant of each exists.
const REQUIRED_STEMS: &[&str] = &["encoder-model", "decoder_joint-model", "nemo128"];

pub struct ModelStatus {
    pub dir: PathBuf,
    pub present: bool,
    pub missing_stems: Vec<String>,
}

pub fn check_parakeet_model() -> Result<ModelStatus> {
    let dir = parakeet_model_dir()?;
    if !dir.exists() {
        return Ok(ModelStatus {
            dir,
            present: false,
            missing_stems: REQUIRED_STEMS.iter().map(|s| s.to_string()).collect(),
        });
    }

    let entries: Vec<String> = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let missing_stems: Vec<String> = REQUIRED_STEMS
        .iter()
        .filter(|stem| !entries.iter().any(|name| name.starts_with(*stem)))
        .map(|s| s.to_string())
        .collect();
    let vocab_present = entries.iter().any(|n| n == "vocab.txt");

    Ok(ModelStatus {
        dir,
        present: missing_stems.is_empty() && vocab_present,
        missing_stems,
    })
}

/// Progress callback: `(downloaded, total)` in bytes. `total == 0` means the
/// server did not advertise a `content-length` (unknown size). Designed to be
/// driven from either the CLI (indicatif bar) or the desktop app (Tauri event
/// emit) — it is `Sync` so it can be handed to a worker thread later.
pub type ProgressFn<'a> = &'a (dyn Fn(u64, u64) + Sync);

/// A no-op [`ProgressFn`] for callers that don't want progress reporting
/// (e.g. `download_parakeet_v3_with_progress(force, &noop_progress)`).
pub fn noop_progress(_: u64, _: u64) {}

/// Download `url` to `dest`, resumably and with sha256 verification, never
/// touching `dest` until the bytes are proven good.
///
/// Contract (every clause load-bearing — this is the fix for the "corrupt
/// file wedged at the final path forever" data-integrity bug):
/// - **Always stages** to a sibling `<dest>.partial`; `dest` is only ever
///   written by the final atomic rename.
/// - **Resumes**: if `<dest>.partial` already holds N>0 bytes, requests
///   `Range: bytes=N-`. A `206` body is appended; if the server ignores the
///   range and answers `200`, the stale partial is truncated and the download
///   restarts from zero (and a `416` — partial larger than the file — likewise
///   restarts fresh).
/// - **Honors `content-length`**: reports `(downloaded, total)` to `progress`,
///   where `total` accounts for already-present resumed bytes.
/// - **Verifies before promoting**: if `expected_sha` is `Some`, hashes the
///   staged file and, on mismatch, **deletes the staging file** and errors —
///   never "proceeds anyway". A short/truncated body (fewer bytes than the
///   advertised `content-length`) errors *before* the hash check and **keeps**
///   the partial so a later run can resume it.
/// - **Promotes atomically** with `fs::rename` only after verification passes.
///
/// Escape hatch: `VZT_FLOW_ALLOW_SHA_MISMATCH=1` downgrades a *hash* mismatch
/// (not a truncation) to a loud warning and promotes anyway. Both model URLs
/// are third-party (blob.handy.computer, huggingface.co); if either upstream
/// legitimately re-uploads a file, a hard sha failure would otherwise brick
/// every fresh install worldwide until we shipped a new constant. The default
/// is a hard failure — this only exists as a manual override.
pub fn download_verified(
    url: &str,
    expected_sha: Option<&str>,
    dest: &Path,
    progress: ProgressFn<'_>,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create download directory {}", parent.display()))?;
    }
    let partial = partial_path(dest);

    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        // No overall request timeout: these are hundreds of MB / 1.1GB and a
        // total-duration cap would kill a healthy-but-slow connection.
        .build()
        .context("failed to build HTTP client")?;

    let mut existing = partial_len(&partial);
    let mut response = send_range_request(&client, url, existing)
        .with_context(|| format!("GET {url} failed"))?;

    // Oversized/stale partial the server can't satisfy — start clean.
    if existing > 0 && response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        fs::remove_file(&partial).ok();
        existing = 0;
        response = send_range_request(&client, url, 0)
            .with_context(|| format!("GET {url} failed"))?;
    }

    let status = response.status();
    let (mut file, mut downloaded, total) = if existing > 0
        && status == StatusCode::PARTIAL_CONTENT
    {
        // Resume: append the remaining bytes onto the existing partial.
        let remaining = content_range_or_length(&response);
        let total = existing + remaining;
        let file = fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .with_context(|| format!("failed to reopen {} for resume", partial.display()))?;
        (file, existing, total)
    } else if status.is_success() {
        // Fresh download (or server ignored our Range and sent a full 200):
        // truncate anything stale and start from zero.
        let total = response.content_length().unwrap_or(0);
        let file = fs::File::create(&partial)
            .with_context(|| format!("failed to create staging file {}", partial.display()))?;
        (file, 0, total)
    } else {
        anyhow::bail!("download failed: HTTP {status}");
    };

    progress(downloaded, total);
    let mut buf = [0u8; 64 * 1024];
    loop {
        // A network drop mid-stream errors here; we propagate and *leave* the
        // partial in place so the next call resumes rather than restarts.
        let n = response
            .read(&mut buf)
            .with_context(|| format!("read error while downloading {url} (partial kept for resume)"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("failed writing to {}", partial.display()))?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    file.flush()?;
    drop(file);

    // Truncated body (server closed before content-length bytes). Keep the
    // partial for resume; do NOT fall through to the hash check (which would
    // "mismatch" and delete the resumable bytes).
    if total != 0 && downloaded != total {
        anyhow::bail!(
            "download incomplete for {}: got {downloaded} of {total} bytes \
             (staged at {} — rerun to resume)",
            dest.display(),
            partial.display()
        );
    }

    if let Some(expected) = expected_sha {
        let actual = sha256_of_file(&partial)?;
        if actual != expected {
            if allow_sha_mismatch() {
                eprintln!(
                    "WARNING: sha256 mismatch for {} (expected {expected}, got {actual}); \
                     VZT_FLOW_ALLOW_SHA_MISMATCH=1 is set, promoting the file anyway",
                    dest.display()
                );
            } else {
                fs::remove_file(&partial).ok();
                anyhow::bail!(
                    "sha256 verification failed for {} (expected {expected}, got {actual}); \
                     deleted the staged download. If upstream legitimately re-uploaded the \
                     file, set VZT_FLOW_ALLOW_SHA_MISMATCH=1 to override.",
                    dest.display()
                );
            }
        }
    }

    fs::rename(&partial, dest).with_context(|| {
        format!(
            "failed to promote {} to {}",
            partial.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

fn partial_len(partial: &Path) -> u64 {
    fs::metadata(partial).map(|m| m.len()).unwrap_or(0)
}

fn allow_sha_mismatch() -> bool {
    std::env::var("VZT_FLOW_ALLOW_SHA_MISMATCH")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn send_range_request(client: &Client, url: &str, from: u64) -> Result<reqwest::blocking::Response> {
    let mut req = client.get(url);
    if from > 0 {
        req = req.header(RANGE, format!("bytes={from}-"));
    }
    Ok(req.send()?)
}

/// Length of the body a `206` response will deliver: prefer the span implied
/// by `Content-Range: bytes A-B/T` (B-A+1), falling back to `content-length`.
fn content_range_or_length(response: &reqwest::blocking::Response) -> u64 {
    if let Some(cr) = response.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        // Format: "bytes 100-199/200"
        if let Some(range) = cr.strip_prefix("bytes ").and_then(|s| s.split('/').next()) {
            if let Some((a, b)) = range.split_once('-') {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
                    return b.saturating_sub(a) + 1;
                }
            }
        }
    }
    response.content_length().unwrap_or(0)
}

/// Download and extract the Parakeet TDT v3 (int8) model archive into
/// `parakeet_model_dir()`. Idempotent: does nothing if the model already looks
/// present, unless `force` is set. CLI entry point — renders an indicatif
/// progress bar with a real total.
pub fn download_parakeet_v3(force: bool) -> Result<PathBuf> {
    with_cli_progress_bar(|progress| download_parakeet_v3_with_progress(force, progress))
}

/// Progress-callback variant of [`download_parakeet_v3`] for callers that
/// render their own UI (e.g. the desktop app).
pub fn download_parakeet_v3_with_progress(force: bool, progress: ProgressFn<'_>) -> Result<PathBuf> {
    let target_dir = parakeet_model_dir()?;
    if !force {
        if let Ok(status) = check_parakeet_model() {
            if status.present {
                println!("Parakeet v3 model already present at {}", target_dir.display());
                return Ok(target_dir);
            }
        }
    }

    fs::create_dir_all(target_dir.parent().unwrap())
        .context("failed to create models parent directory")?;

    // Keep the staging dir across runs so an interrupted 456MB archive
    // download can *resume* (its `<archive>.partial` lives here). The old code
    // wiped this dir on every run, which defeated resume for exactly the flaky-
    // wifi case it's meant to cover. `download_verified` is safe against a
    // stale/incompatible partial (bad sha ⇒ it deletes the partial and restarts
    // fresh), so preserving it can only help.
    let staging_root = target_dir.parent().unwrap().join(".staging-parakeet-v3");
    fs::create_dir_all(&staging_root)?;

    // `download_verified` stages, resumes, hashes and only then promotes the
    // archive into place — so a truncated/corrupt download is deleted here and
    // can never reach the extractor below.
    let archive_path = staging_root.join("parakeet-v3-int8.tar.gz");
    download_verified(PARAKEET_V3_URL, Some(PARAKEET_V3_SHA256), &archive_path, progress)?;

    // A previous attempt may have left a half-unpacked tree; clear only that,
    // never the (possibly resumable) archive above.
    let extract_dir = staging_root.join("extracted");
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir).ok();
    }
    fs::create_dir_all(&extract_dir)?;
    extract_tar_gz(&archive_path, &extract_dir)
        .with_context(|| format!("failed to extract {}", archive_path.display()))?;

    // The archive may extract flat (onnx files directly in extract_dir) or
    // nested under one subdirectory. Find whichever directory actually
    // contains the onnx files and use that as the source.
    let source_dir = find_model_dir(&extract_dir)
        .context("could not locate extracted Parakeet model files (no *.onnx found)")?;

    if target_dir.exists() {
        fs::remove_dir_all(&target_dir)?;
    }
    fs::rename(&source_dir, &target_dir).with_context(|| {
        format!(
            "failed to move extracted model from {} to {}",
            source_dir.display(),
            target_dir.display()
        )
    })?;

    fs::remove_dir_all(&staging_root).ok();

    println!("Parakeet v3 model ready at {}", target_dir.display());
    Ok(target_dir)
}

/// Extract a `.tar.gz` using the `flate2` + `tar` crates rather than shelling
/// out to `tar` — the external binary isn't guaranteed on PATH (notably on
/// Windows, now a supported install target). A truncated archive fails gzip or
/// tar parsing here and errors; `find_model_dir` still tolerates both flat and
/// single-nested layouts afterwards.
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)
        .with_context(|| format!("failed to open {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(decoder);
    ar.unpack(dest)
        .with_context(|| format!("failed to unpack archive into {}", dest.display()))?;
    Ok(())
}

/// Depth-first search for the directory that directly contains `.onnx`
/// files, so we don't have to assume the archive's internal layout.
fn find_model_dir(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    let mut subdirs = Vec::new();
    let mut has_onnx = false;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.extension().is_some_and(|ext| ext == "onnx") {
            has_onnx = true;
        }
    }
    if has_onnx {
        return Some(root.to_path_buf());
    }
    for subdir in subdirs {
        if let Some(found) = find_model_dir(&subdir) {
            return Some(found);
        }
    }
    None
}

/// Official Qwen3-1.7B-Instruct GGUF has no Q4_K_M variant published, so
/// this uses unsloth's re-quantization of the same weights — same model,
/// same chat template, just a Q4_K_M file available.
const CLEANUP_MODEL_URL: &str =
    "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf";
const CLEANUP_MODEL_SHA256: &str = "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897";
const CLEANUP_MODEL_FILENAME: &str = "Qwen3-1.7B-Q4_K_M.gguf";
/// Sentinel written next to the cleanup model *after* its sha256 has been
/// verified. `check_cleanup_model` requires it, so a corrupt leftover (which
/// has no sentinel) reads as MISSING and gets re-downloaded, instead of a bad
/// file being trusted forever. Kept as a separate file so presence is a cheap
/// `exists()` check — we never re-hash 1.1GB on a normal launch.
const CLEANUP_MODEL_OK_SENTINEL: &str = ".Qwen3-1.7B-Q4_K_M.gguf.ok";

pub fn cleanup_model_dir() -> Result<PathBuf> {
    Ok(model_root_dir()?.join("cleanup"))
}

pub fn cleanup_model_path() -> Result<PathBuf> {
    Ok(cleanup_model_dir()?.join(CLEANUP_MODEL_FILENAME))
}

fn cleanup_model_ok_sentinel() -> Result<PathBuf> {
    Ok(cleanup_model_dir()?.join(CLEANUP_MODEL_OK_SENTINEL))
}

/// True only when the cleanup model is present **and verified**.
///
/// Fast path: model file + `.ok` sentinel both exist → true, no hashing.
///
/// Migration / self-heal path (model present, sentinel absent — either a
/// good model downloaded before sentinels existed, or a corrupt leftover from
/// the old "proceeding anyway" bug): hash the file **once**. If it matches,
/// write the sentinel so future checks are cheap and return true; if it
/// doesn't, it's a corrupt leftover — return false so it gets re-downloaded.
/// This one-time hash is the migration cost, and it silently heals anyone
/// currently holding a truncated model.
pub fn check_cleanup_model() -> Result<bool> {
    let model = cleanup_model_path()?;
    if !model.exists() {
        return Ok(false);
    }
    let sentinel = cleanup_model_ok_sentinel()?;
    if sentinel.exists() {
        return Ok(true);
    }
    let actual = sha256_of_file(&model)?;
    if actual == CLEANUP_MODEL_SHA256 {
        // Best-effort: if we can't write the sentinel (e.g. read-only dir) the
        // model is still good, we'll just re-hash next time.
        write_ok_sentinel(&sentinel).ok();
        Ok(true)
    } else {
        Ok(false)
    }
}

fn write_ok_sentinel(sentinel: &Path) -> Result<()> {
    fs::write(sentinel, format!("sha256={CLEANUP_MODEL_SHA256}\n"))
        .with_context(|| format!("failed to write sentinel {}", sentinel.display()))
}

/// Download the cleanup GGUF into `cleanup_model_dir()`. Idempotent on a
/// *verified* model (see [`check_cleanup_model`]); a corrupt leftover is
/// re-downloaded rather than trusted. CLI entry point — renders a progress bar.
pub fn download_cleanup_model(force: bool) -> Result<PathBuf> {
    with_cli_progress_bar(|progress| download_cleanup_model_with_progress(force, progress))
}

/// Progress-callback variant of [`download_cleanup_model`].
pub fn download_cleanup_model_with_progress(force: bool, progress: ProgressFn<'_>) -> Result<PathBuf> {
    let target = cleanup_model_path()?;
    if !force && check_cleanup_model()? {
        println!("Cleanup model already present at {}", target.display());
        return Ok(target);
    }

    fs::create_dir_all(target.parent().unwrap())
        .context("failed to create cleanup model directory")?;

    // Stage → verify → promote. The old code downloaded straight to `target`
    // and, on sha mismatch, printed "proceeding anyway" and kept the bad file
    // (which then blocked every future re-download). `download_verified` hard-
    // fails on mismatch and never touches `target` until the bytes are good.
    download_verified(CLEANUP_MODEL_URL, Some(CLEANUP_MODEL_SHA256), &target, progress)?;
    write_ok_sentinel(&cleanup_model_ok_sentinel()?)?;

    println!("Cleanup model ready at {}", target.display());
    Ok(target)
}

/// Run `f` with a progress callback that drives a fresh indicatif bar. Used by
/// the CLI entry points; the bar shows a real total because the callback gets
/// `content-length` from `download_verified`. Cleared on completion (nothing is
/// left on screen if the download was skipped and the callback never fired).
fn with_cli_progress_bar<T>(f: impl FnOnce(ProgressFn<'_>) -> Result<T>) -> Result<T> {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    let bar_cb = bar.clone();
    let cb = move |done: u64, total: u64| {
        if total > 0 && bar_cb.length() != Some(total) {
            bar_cb.set_length(total);
        }
        bar_cb.set_position(done);
    };
    let result = f(&cb);
    bar.finish_and_clear();
    result
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Serializes the tests that mutate the process-global
    /// `VZT_FLOW_ALLOW_SHA_MISMATCH` env var, since `cargo test` runs the test
    /// binary's cases on parallel threads sharing one environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// What the tiny test server should do for a single connection. All fields
    /// are computed from the parsed `Range:` start (`None` when absent).
    struct Reply {
        status_line: String,
        /// Extra headers, e.g. a `Content-Range`.
        extra_headers: Vec<String>,
        /// `Content-Length` to *advertise* (may exceed `body.len()` to simulate
        /// a truncated response).
        advertised_len: u64,
        /// Bytes actually written before the socket is closed.
        body: Vec<u8>,
    }

    struct TestServer {
        port: u16,
        /// `u64::MAX` sentinel = no Range header was seen on the last request.
        last_range: Arc<AtomicU64>,
    }

    impl TestServer {
        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/file", self.port)
        }
    }

    /// Spawn a one-shot HTTP/1.1 server: it serves connections with `handler`,
    /// which is given the parsed `Range:` start and returns a [`Reply`].
    fn spawn_server<H>(handler: H) -> TestServer
    where
        H: Fn(Option<u64>) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let last_range = Arc::new(AtomicU64::new(u64::MAX));
        let last_range_srv = last_range.clone();
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let range = handle_request_line(&stream);
                last_range_srv.store(range.unwrap_or(u64::MAX), Ordering::SeqCst);
                let reply = handler(range);
                write_reply(stream, &reply);
            }
        });
        TestServer { port, last_range }
    }

    /// Read request headers, returning the `Range: bytes=N-` start if present.
    fn handle_request_line(stream: &TcpStream) -> Option<u64> {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("range:") {
                if let Some(spec) = rest.trim().strip_prefix("bytes=") {
                    if let Some(start) = spec.split('-').next() {
                        range = start.trim().parse::<u64>().ok();
                    }
                }
            }
        }
        range
    }

    fn write_reply(mut stream: TcpStream, reply: &Reply) {
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\n",
            reply.status_line, reply.advertised_len
        );
        for h in &reply.extra_headers {
            head.push_str(h);
            head.push_str("\r\n");
        }
        head.push_str("Connection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&reply.body);
        let _ = stream.flush();
        // Drop closes the socket, giving the client an EOF (needed so a
        // truncated body is seen as a closed connection).
    }

    fn ok_reply(body: Vec<u8>) -> Reply {
        Reply {
            status_line: "200 OK".into(),
            extra_headers: vec![],
            advertised_len: body.len() as u64,
            body,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "vzt-flow-dltest-{}-{}-{}",
            name,
            std::process::id(),
            // unique-ish per test
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn valid_body_promotes_and_clears_partial() {
        let body = b"hello parakeet world".to_vec();
        let sha = {
            let mut h = Sha256::new();
            h.update(&body);
            format!("{:x}", h.finalize())
        };
        let b = body.clone();
        let srv = spawn_server(move |_| ok_reply(b.clone()));

        let dest = tmp("valid");
        download_verified(&srv.url(), Some(&sha), &dest, &noop_progress).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!partial_path(&dest).exists(), "partial must be gone");
        fs::remove_file(&dest).ok();
    }

    #[test]
    fn short_body_errors_dest_absent_partial_kept() {
        // Advertise 1000 bytes but only send 100, then close.
        let srv = spawn_server(|_| Reply {
            status_line: "200 OK".into(),
            extra_headers: vec![],
            advertised_len: 1000,
            body: vec![7u8; 100],
        });

        let dest = tmp("short");
        let err = download_verified(&srv.url(), None, &dest, &noop_progress).unwrap_err();
        assert!(
            format!("{err:#}").contains("incomplete") || format!("{err:#}").contains("read error"),
            "unexpected error: {err:#}"
        );
        assert!(!dest.exists(), "dest must not exist on truncation");
        assert!(
            partial_path(&dest).exists(),
            "partial must be kept for resume on truncation"
        );
        fs::remove_file(partial_path(&dest)).ok();
    }

    #[test]
    fn sha_mismatch_errors_and_deletes_partial() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("VZT_FLOW_ALLOW_SHA_MISMATCH");
        let srv = spawn_server(|_| ok_reply(b"the wrong bytes".to_vec()));
        let dest = tmp("mismatch");
        let err = download_verified(
            &srv.url(),
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
            &dest,
            &noop_progress,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "unexpected: {err:#}");
        assert!(!dest.exists(), "dest must be absent");
        assert!(
            !partial_path(&dest).exists(),
            "partial must be deleted on sha mismatch"
        );
    }

    #[test]
    fn resume_appends_and_sends_range_header() {
        let full: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        let sha = {
            let mut h = Sha256::new();
            h.update(&full);
            format!("{:x}", h.finalize())
        };
        let dest = tmp("resume");
        let partial = partial_path(&dest);
        // Pre-seed the partial with the first 50 bytes.
        fs::create_dir_all(partial.parent().unwrap()).ok();
        fs::write(&partial, &full[..50]).unwrap();

        let full_srv = full.clone();
        let srv = spawn_server(move |range| {
            let start = range.expect("server must receive a Range header") as usize;
            let remainder = full_srv[start..].to_vec();
            Reply {
                status_line: "206 Partial Content".into(),
                extra_headers: vec![format!(
                    "Content-Range: bytes {}-{}/{}",
                    start,
                    full_srv.len() - 1,
                    full_srv.len()
                )],
                advertised_len: remainder.len() as u64,
                body: remainder,
            }
        });

        download_verified(&srv.url(), Some(&sha), &dest, &noop_progress).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), full, "promoted file must be whole");
        assert_eq!(
            srv.last_range.load(Ordering::SeqCst),
            50,
            "server should have seen Range starting at 50"
        );
        assert!(!partial.exists());
        fs::remove_file(&dest).ok();
    }

    #[test]
    fn allow_sha_mismatch_env_promotes_with_warning() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let srv = spawn_server(|_| ok_reply(b"content upstream re-uploaded".to_vec()));
        let dest = tmp("allow");
        // Scope the env var tightly and restore, to avoid polluting other tests.
        std::env::set_var("VZT_FLOW_ALLOW_SHA_MISMATCH", "1");
        let res = download_verified(
            &srv.url(),
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
            &dest,
            &noop_progress,
        );
        std::env::remove_var("VZT_FLOW_ALLOW_SHA_MISMATCH");

        res.expect("override should promote despite mismatch");
        assert_eq!(fs::read(&dest).unwrap(), b"content upstream re-uploaded");
        assert!(!partial_path(&dest).exists());
        fs::remove_file(&dest).ok();
    }

    #[test]
    fn cleanup_present_requires_sentinel_and_correct_hash() {
        // Drive the real check_cleanup_model() against an isolated config dir.
        // VZT_FLOW_CONFIG_DIR is unique to this test, so it doesn't race the
        // download_verified tests (which use explicit paths, never config_dir).
        let cfg = tmp("cleanupcfg");
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &cfg);

        // Corrupt model, no sentinel → NOT present (would be re-downloaded).
        let model = cleanup_model_path().unwrap();
        fs::create_dir_all(model.parent().unwrap()).unwrap();
        fs::write(&model, b"not the real model bytes").unwrap();
        assert!(!cleanup_model_ok_sentinel().unwrap().exists());
        assert!(
            !check_cleanup_model().unwrap(),
            "corrupt model without sentinel must read as NOT present"
        );

        // Adding the sentinel is what marks it verified/present.
        write_ok_sentinel(&cleanup_model_ok_sentinel().unwrap()).unwrap();
        assert!(
            check_cleanup_model().unwrap(),
            "model + sentinel must read as present"
        );

        std::env::remove_var(crate::config::CONFIG_DIR_ENV);
        fs::remove_dir_all(&cfg).ok();
    }
}
