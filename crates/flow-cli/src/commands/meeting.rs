//! `flow meeting` — live meeting transcription, and `flow meeting list`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use flow_core::meeting;

/// Starts a live meeting session, stopping (and summarizing) on Ctrl+C.
/// Prints the transcript file path to stdout on completion.
pub fn run(title: Option<String>, out: Option<PathBuf>) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = stop.clone();
    // SIGINT stops capture gracefully; the session flushes tails and
    // summarizes before returning. Ignore the (only-once) set_handler error.
    let _ = ctrlc::set_handler(move || {
        eprintln!("\n[vzt-flow] Ctrl+C — stopping capture and summarizing...");
        stop_handler.store(true, Ordering::SeqCst);
    });

    let path = meeting::run(title, out, stop)?;
    println!("{}", path.display());
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
