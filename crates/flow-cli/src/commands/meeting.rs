//! `flow meeting` — live meeting transcription, and `flow meeting list`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use flow_core::config::Config;
use flow_core::meeting::{
    self, notes, pdf::PdfOptions, summary::SummaryOptions, InterviewTip, MeetingOptions, SessionState,
};

/// Starts a live meeting session, stopping (and summarizing) on Ctrl+C.
///
/// Prints the transcript file path to stdout on completion (unchanged
/// contract for scripting). Everything else — live state/tip output, the PDF
/// path or error, summary coverage and whether notes were merged — goes to
/// stderr.
#[allow(clippy::too_many_arguments)]
pub fn run(
    title: Option<String>,
    out: Option<PathBuf>,
    interview: bool,
    notes_seed: Option<PathBuf>,
    no_pdf: bool,
    pdf_dir: Option<PathBuf>,
) -> Result<()> {
    let cfg = Config::load().unwrap_or_default();

    let title = title.unwrap_or_else(|| "meeting".to_string());
    let out_dir = match out {
        Some(d) => d,
        None => meeting::default_meetings_dir()?,
    };

    // `--notes` needs the transcript's stem to derive the sidecar path, and
    // that's only known once the transcript file is reserved — so reserve it
    // here, on the CLI thread, and hand the reserved path through
    // `MeetingOptions::transcript_path` (the session then skips its own
    // reservation; see `run_with`'s `reserved_here` check).
    let reserved = meeting::reserve_transcript_path(&out_dir, &chrono::Local::now(), &title)?;
    if let Some(seed_path) = &notes_seed {
        let contents = std::fs::read_to_string(seed_path)
            .with_context(|| format!("failed to read notes seed file {}", seed_path.display()))?;
        let notes_path = notes::notes_path_for(&reserved);
        notes::save_atomic(&notes_path, &contents)
            .with_context(|| format!("failed to seed notes sidecar {}", notes_path.display()))?;
    }

    let interview_on = interview || cfg.meeting_interview;
    let interview_flag = Arc::new(AtomicBool::new(interview_on));

    let pdf_enabled = cfg.meeting_pdf && !no_pdf;
    let pdf_directory = pdf_dir
        .or_else(|| cfg.meeting_pdf_dir_resolved())
        .unwrap_or_else(|| out_dir.clone());

    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = stop.clone();
    // SIGINT stops capture gracefully; the session flushes tails and
    // summarizes before returning. Ignore the (only-once) set_handler error.
    let _ = ctrlc::set_handler(move || {
        eprintln!("\n[vzt-flow] Ctrl+C — stopping capture and summarizing...");
        stop_handler.store(true, Ordering::SeqCst);
    });

    let on_state: meeting::StateObserver = Arc::new(|state: &SessionState| {
        let step = match state {
            SessionState::Recording => "recording".to_string(),
            SessionState::Stopping => "stopping".to_string(),
            SessionState::Finalizing { step } => step.clone(),
            SessionState::Completed => "completed".to_string(),
            SessionState::Failed(e) => format!("failed: {e}"),
        };
        eprintln!("[vzt-flow] {step}");
    });

    // Only spawn the coach thread when interview mode is actually on
    // (`on_tip: None` means flow-core doesn't start it at all).
    let on_tip: Option<meeting::TipObserver> = if interview_on {
        Some(Arc::new(|tip: &InterviewTip| {
            eprintln!("\n  ▸ {}", tip.headline);
            let last_idx = tip.bullets.len().saturating_sub(1);
            for (i, bullet) in tip.bullets.iter().enumerate() {
                if i == last_idx {
                    eprintln!("    - {}   ({}ms)", bullet, tip.latency_ms);
                } else {
                    eprintln!("    - {bullet}");
                }
            }
        }))
    } else {
        None
    };

    let opts = MeetingOptions {
        title: Some(title),
        out_dir: Some(out_dir),
        transcript_path: Some(reserved),
        on_state: Some(on_state),
        on_tip,
        interview: interview_flag,
        pdf: Some(PdfOptions { dir: pdf_directory, enabled: pdf_enabled }),
        summary: SummaryOptions {
            window_chars: cfg.meeting_summary_window_chars,
            partial_timeout_ms: cfg.meeting_summary_partial_timeout_ms,
            ..SummaryOptions::default()
        },
        ..Default::default()
    };

    let outcome = meeting::run_with(opts, stop)?;

    println!("{}", outcome.transcript.display());

    match (&outcome.pdf, &outcome.pdf_error) {
        (Some(pdf), _) => eprintln!("[vzt-flow] PDF: {}", pdf.display()),
        (None, Some(err)) => eprintln!("[vzt-flow] PDF not written: {err}"),
        (None, None) => {}
    }
    eprintln!(
        "[vzt-flow] summary: {} section{}{}",
        outcome.summary_sections,
        if outcome.summary_sections == 1 { "" } else { "s" },
        if outcome.summary_complete { "" } else { " (partial — some of the meeting could not be summarized)" }
    );
    eprintln!(
        "[vzt-flow] notes: {}",
        if outcome.notes_merged { "merged into transcript" } else { "not merged" }
    );

    Ok(())
}

/// Lists recent meeting transcripts (newest first).
pub fn list(n: usize) -> Result<()> {
    let dir = meeting::default_meetings_dir()?;
    let meetings = meeting::list_meetings(&dir, n)?;
    if meetings.is_empty() {
        println!("No meetings found in {}", dir.display());
        return Ok(());
    }
    println!("Recent meetings in {}:\n", dir.display());
    for m in meetings {
        let duration = m.duration.as_deref().unwrap_or("?");
        let size_kb = m.size_bytes as f64 / 1024.0;
        let datetime = if m.datetime.is_empty() { "?" } else { &m.datetime };
        println!(
            "  {datetime}  {title}  (dur {duration}, {size_kb:.1} KB)\n    {path}",
            title = m.title,
            path = m.path.display()
        );
    }
    Ok(())
}
