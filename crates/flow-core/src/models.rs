//! Model download + on-disk layout management.
//!
//! Layout: `~/.config/vzt-flow/models/parakeet-v3/` holds the extracted
//! ONNX files (encoder/decoder/preprocessor + vocab.txt) that
//! `ParakeetModel::load` expects.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};

/// Same archive Handy (github.com/cjpais/Handy) downloads for its
/// "Parakeet V3" model — a pre-packaged Parakeet TDT 0.6B v3 (int8 ONNX).
const PARAKEET_V3_URL: &str = "https://blob.handy.computer/parakeet-v3-int8.tar.gz";

pub fn model_root_dir() -> Result<PathBuf> {
    // The brief specifies the literal `~/.config/vzt-flow/models` path.
    // `dirs::config_dir()` would resolve to `~/Library/Application
    // Support` on macOS per platform convention, so we build the Unix-style
    // path explicitly off the home directory instead.
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".config").join("vzt-flow").join("models"))
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

/// Download and extract the Parakeet TDT v3 (int8) model archive into
/// `parakeet_model_dir()`, showing a progress bar. Idempotent: does
/// nothing if the model already looks present, unless `force` is set.
pub fn download_parakeet_v3(force: bool) -> Result<PathBuf> {
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

    let staging_root = target_dir.parent().unwrap().join(".staging-parakeet-v3");
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root).ok();
    }
    fs::create_dir_all(&staging_root)?;

    let archive_path = staging_root.join("parakeet-v3-int8.tar.gz");
    download_with_progress(PARAKEET_V3_URL, &archive_path)?;

    let sha = sha256_of_file(&archive_path)?;
    println!("Downloaded archive sha256: {sha}");

    let extract_dir = staging_root.join("extracted");
    fs::create_dir_all(&extract_dir)?;
    let status = Command::new("tar")
        .args(["xzf", archive_path.to_str().unwrap(), "-C", extract_dir.to_str().unwrap()])
        .status()
        .context("failed to invoke `tar` to extract model archive (is it on PATH?)")?;
    if !status.success() {
        anyhow::bail!("tar extraction failed with status {status}");
    }

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

fn download_with_progress(url: &str, dest: &Path) -> Result<()> {
    let response = reqwest::blocking::get(url).with_context(|| format!("GET {url} failed"))?;
    if !response.status().is_success() {
        anyhow::bail!("download failed: HTTP {}", response.status());
    }
    let total_size = response.content_length().unwrap_or(0);

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

    let mut file = fs::File::create(dest)?;
    let mut reader = pb.wrap_read(response);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
    }
    pb.finish_with_message("download complete");
    Ok(())
}

/// Official Qwen3-1.7B-Instruct GGUF has no Q4_K_M variant published, so
/// this uses unsloth's re-quantization of the same weights — same model,
/// same chat template, just a Q4_K_M file available.
const CLEANUP_MODEL_URL: &str =
    "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf";
const CLEANUP_MODEL_SHA256: &str = "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897";
const CLEANUP_MODEL_FILENAME: &str = "Qwen3-1.7B-Q4_K_M.gguf";

pub fn cleanup_model_dir() -> Result<PathBuf> {
    Ok(model_root_dir()?.join("cleanup"))
}

pub fn cleanup_model_path() -> Result<PathBuf> {
    Ok(cleanup_model_dir()?.join(CLEANUP_MODEL_FILENAME))
}

pub fn check_cleanup_model() -> Result<bool> {
    Ok(cleanup_model_path()?.exists())
}

/// Download the cleanup GGUF into `cleanup_model_dir()`, showing a progress
/// bar and checking its sha256 against the known-good hash (a mismatch is
/// logged but not fatal — upstream repos occasionally re-upload).
pub fn download_cleanup_model(force: bool) -> Result<PathBuf> {
    let target = cleanup_model_path()?;
    if !force && target.exists() {
        println!("Cleanup model already present at {}", target.display());
        return Ok(target);
    }

    fs::create_dir_all(target.parent().unwrap()).context("failed to create cleanup model directory")?;
    download_with_progress(CLEANUP_MODEL_URL, &target)?;

    let sha = sha256_of_file(&target)?;
    if sha == CLEANUP_MODEL_SHA256 {
        println!("Cleanup model sha256 verified: {sha}");
    } else {
        println!(
            "WARNING: cleanup model sha256 mismatch (expected {CLEANUP_MODEL_SHA256}, got {sha}) \
             — proceeding anyway, but the upstream file may have changed"
        );
    }
    println!("Cleanup model ready at {}", target.display());
    Ok(target)
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
