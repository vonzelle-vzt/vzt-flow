//! Offline meeting-accuracy replay harness (not part of the CLI): feeds a
//! corpus of pre-recorded wav files through the *real* meeting pipeline
//! (`StreamingChunker` -> `meeting::pipeline::ChunkPipeline` -> a real
//! `ParakeetTranscriber`) and reports WER against a reference transcript per
//! case, plus a corpus-wide WER.
//!
//! Mirrors `cleanup_replay.rs`'s framing: the engine is loaded once (model
//! load + Metal/ONNX pipeline warm-up costs several seconds and would dominate
//! a per-file invocation), then reused across every case in the corpus.
//!
//! Per CLAUDE.md gotcha (b) (transcribe-rs Parakeet memory is quadratic in
//! audio length; never call `.transcribe()` on >60s of audio directly), audio
//! is never handed to the engine whole — it is fed through a real
//! `StreamingChunker` in 4096-sample blocks, exactly as the live meeting
//! worker does, so each engine call sees at most one ~30s chunk.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example meeting_replay -- --corpus <dir> [--json]
//! ```
//!
//! `<dir>` contains, per case, either:
//!   - `<name>.wav` + `<name>.ref.txt` (single-source case), or
//!   - `<name>.them.wav` + `<name>.me.wav` + `<name>.ref.txt` (dual-source
//!     case: `them.wav` and `me.wav` are each run through their own
//!     `StreamingChunker`, and the resulting chunks are interleaved by
//!     `start_offset` before being fed to one `ChunkPipeline` — this is what
//!     the live worker's FIFO approximates, and it is where the echo filter
//!     is exercised).
//!
//! For each case, prints:
//!   `case=<name> wer=<f> ref_words=<n> sub=<n> del=<n> ins=<n> lines=<n> dropped_echo=<n> skipped=<n>`
//! and finally, over all cases' hypotheses/references concatenated:
//!   `corpus_wer=<f>`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use flow_core::dictionary;
use flow_core::engine::{ParakeetTranscriber, Transcriber};
use flow_core::eval;
use flow_core::meeting::pipeline::{ChunkPipeline, Decision, SkipReason};
use flow_core::meeting::transcriber::{Source, StreamingChunker};
use flow_core::models;

/// One emitted transcript line, ordered by when the pipeline decided it
/// (which for a dual-source case is the merged `start_offset` order fed in,
/// not necessarily wall-clock emission order of a live worker — see the
/// module docs).
struct Line {
    #[allow(dead_code)]
    start: f32,
    #[allow(dead_code)]
    source: Source,
    text: String,
}

/// A single native-rate, mono f32 wav read directly off disk (no resampling
/// to 16kHz here — that happens inside `ChunkPipeline::process`, exactly as
/// the live worker does it, so the chunker sees the file's real sample rate).
struct WavFile {
    samples: Vec<f32>,
    sample_rate: u32,
}

fn load_wav_native(path: &Path) -> Result<WavFile> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("failed to open wav {}", path.display()))?;
    let spec = reader.spec();

    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read f32 wav samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed to read int wav samples")?
        }
    };

    let mono = if spec.channels > 1 {
        raw.chunks(spec.channels as usize)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    } else {
        raw
    };

    Ok(WavFile { samples: mono, sample_rate: spec.sample_rate })
}

/// Feeds `wav` through a fresh `StreamingChunker` for `source`, in
/// 4096-sample blocks (matching the live capture block size), flushing
/// whatever is left at the end. Returns every emitted `Chunk`.
fn chunk_wav(wav: &WavFile, source: Source) -> Vec<flow_core::meeting::transcriber::Chunk> {
    const BLOCK: usize = 4096;
    let mut chunker = StreamingChunker::new(source, wav.sample_rate);
    let mut chunks = Vec::new();
    for block in wav.samples.chunks(BLOCK) {
        chunks.extend(chunker.push(block));
    }
    if let Some(last) = chunker.flush() {
        chunks.push(last);
    }
    chunks
}

/// One test case: either a single `.wav` or a `.them.wav`/`.me.wav` pair,
/// plus its `.ref.txt` reference transcript.
struct Case {
    name: String,
    reference: String,
    them_wav: Option<PathBuf>,
    me_wav: Option<PathBuf>,
    single_wav: Option<PathBuf>,
}

fn discover_cases(corpus_dir: &Path) -> Result<Vec<Case>> {
    let mut names: Vec<String> = std::fs::read_dir(corpus_dir)
        .with_context(|| format!("failed to read corpus dir {}", corpus_dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file_name = e.file_name().into_string().ok()?;
            file_name.strip_suffix(".ref.txt").map(|n| n.to_string())
        })
        .collect();
    names.sort();

    let mut cases = Vec::new();
    for name in names {
        let reference = std::fs::read_to_string(corpus_dir.join(format!("{name}.ref.txt")))
            .with_context(|| format!("failed to read {name}.ref.txt"))?
            .trim()
            .to_string();

        let single = corpus_dir.join(format!("{name}.wav"));
        let them = corpus_dir.join(format!("{name}.them.wav"));
        let me = corpus_dir.join(format!("{name}.me.wav"));

        if them.exists() && me.exists() {
            cases.push(Case {
                name,
                reference,
                them_wav: Some(them),
                me_wav: Some(me),
                single_wav: None,
            });
        } else if single.exists() {
            cases.push(Case { name, reference, them_wav: None, me_wav: None, single_wav: Some(single) });
        } else {
            anyhow::bail!(
                "case {name}: found {name}.ref.txt but neither {name}.wav nor \
                 {name}.them.wav+{name}.me.wav"
            );
        }
    }
    Ok(cases)
}

/// Outcome of replaying one case through the pipeline.
struct CaseResult {
    report: eval::WerReport,
    lines: usize,
    dropped_echo: usize,
    skipped: usize,
    hypothesis: String,
}

fn run_case(
    case: &Case,
    transcriber: &mut ParakeetTranscriber,
    dict: &Arc<Vec<dictionary::DictionaryTerm>>,
) -> Result<CaseResult> {
    // Gather every chunk this case will feed the pipeline, in the order the
    // pipeline should see them.
    let mut chunks: Vec<flow_core::meeting::transcriber::Chunk> = Vec::new();
    if let Some(single) = &case.single_wav {
        let wav = load_wav_native(single)?;
        chunks.extend(chunk_wav(&wav, Source::Them));
    } else {
        let them_wav = load_wav_native(case.them_wav.as_ref().unwrap())?;
        let me_wav = load_wav_native(case.me_wav.as_ref().unwrap())?;
        let mut them_chunks = chunk_wav(&them_wav, Source::Them);
        let mut me_chunks = chunk_wav(&me_wav, Source::Me);
        // Interleave by start_offset: this is what the live worker's FIFO
        // approximates (each source chunks independently; the pipeline only
        // ever sees one chunk at a time, in roughly wall-clock order), and
        // it is what puts an overlapping `Me` chunk in front of the `Them`
        // line it should be flagged as echoing.
        them_chunks.sort_by(|a, b| a.start_offset.partial_cmp(&b.start_offset).unwrap());
        me_chunks.sort_by(|a, b| a.start_offset.partial_cmp(&b.start_offset).unwrap());
        let mut ti = them_chunks.into_iter().peekable();
        let mut mi = me_chunks.into_iter().peekable();
        loop {
            match (ti.peek(), mi.peek()) {
                (Some(t), Some(m)) => {
                    if t.start_offset <= m.start_offset {
                        chunks.push(ti.next().unwrap());
                    } else {
                        chunks.push(mi.next().unwrap());
                    }
                }
                (Some(_), None) => chunks.push(ti.next().unwrap()),
                (None, Some(_)) => chunks.push(mi.next().unwrap()),
                (None, None) => break,
            }
        }
    }

    let mut pipeline = ChunkPipeline::new(Arc::clone(dict));
    let mut lines: Vec<Line> = Vec::new();
    let mut dropped_echo = 0usize;
    let mut skipped = 0usize;

    for chunk in &chunks {
        let decision = pipeline.process(chunk, |samples| {
            transcriber.transcribe(samples).map(|t| t.text)
        });
        match decision {
            Decision::Emit { start, source, text } => {
                println!("  [{start:>7.2}s {}] {text}", source.label());
                lines.push(Line { start, source, text });
            }
            Decision::DroppedEcho(text) => {
                println!("  [dropped echo] {text}");
                dropped_echo += 1;
            }
            Decision::Skipped(reason) => {
                let why = match reason {
                    SkipReason::NoSpeech => "no-speech",
                    SkipReason::EmptySamples => "empty-samples",
                    SkipReason::EmptyText => "empty-text",
                };
                println!("  [skipped: {why}]");
                skipped += 1;
            }
            Decision::Failed(err) => {
                eprintln!("  [engine error] {err}");
                skipped += 1;
            }
        }
    }

    let hypothesis = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
    let report = eval::wer(&case.reference, &hypothesis);

    Ok(CaseResult { report, lines: lines.len(), dropped_echo, skipped, hypothesis })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut corpus_dir: Option<PathBuf> = None;
    let mut json = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--corpus" => {
                i += 1;
                corpus_dir = args.get(i).map(PathBuf::from);
            }
            "--json" => json = true,
            other => anyhow::bail!("unrecognized argument: {other}"),
        }
        i += 1;
    }
    let corpus_dir = corpus_dir
        .context("usage: cargo run --release --example meeting_replay -- --corpus <dir> [--json]")?;

    let dict = Arc::new(dictionary::load_or_seed().unwrap_or_default());
    eprintln!("dictionary  : {} terms", dict.len());

    let model_dir = models::parakeet_model_dir()?;
    let mut transcriber = ParakeetTranscriber::load(&model_dir)?;
    eprintln!("model load  : {:.2}s", transcriber.load_time.as_secs_f64());

    let cases = discover_cases(&corpus_dir)?;
    if cases.is_empty() {
        anyhow::bail!("no cases found in {} (expected <name>.ref.txt files)", corpus_dir.display());
    }

    let mut total_ref_words = 0usize;
    let mut total_sub = 0usize;
    let mut total_del = 0usize;
    let mut total_ins = 0usize;
    let mut json_cases = Vec::new();

    for case in &cases {
        println!("=== {} ===", case.name);
        let result = run_case(case, &mut transcriber, &dict)?;
        let wer = result.report.wer();

        println!(
            "case={} wer={:.4} ref_words={} sub={} del={} ins={} lines={} dropped_echo={} skipped={}",
            case.name,
            wer,
            result.report.reference_words,
            result.report.substitutions,
            result.report.deletions,
            result.report.insertions,
            result.lines,
            result.dropped_echo,
            result.skipped,
        );

        total_ref_words += result.report.reference_words;
        total_sub += result.report.substitutions;
        total_del += result.report.deletions;
        total_ins += result.report.insertions;

        if json {
            json_cases.push(serde_json::json!({
                "case": case.name,
                "wer": wer,
                "ref_words": result.report.reference_words,
                "sub": result.report.substitutions,
                "del": result.report.deletions,
                "ins": result.report.insertions,
                "lines": result.lines,
                "dropped_echo": result.dropped_echo,
                "skipped": result.skipped,
                "hypothesis": result.hypothesis,
                "reference": case.reference,
            }));
        }
    }

    let corpus_wer = if total_ref_words == 0 {
        0.0
    } else {
        (total_sub + total_del + total_ins) as f64 / total_ref_words as f64
    };
    println!("corpus_wer={corpus_wer:.4}");

    if json {
        let summary = serde_json::json!({
            "corpus_wer": corpus_wer,
            "total_ref_words": total_ref_words,
            "total_sub": total_sub,
            "total_del": total_del,
            "total_ins": total_ins,
            "cases": json_cases,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

    // Drop the transcriber before exiting so its ONNX sessions tear down
    // normally rather than via process exit.
    drop(transcriber);
    Ok(())
}
