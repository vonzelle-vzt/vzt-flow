use std::time::Duration;

use flow_core::history::HistoryEntry;
use flow_core::ipc::Request;

use super::daemon_client;

pub fn run(n: usize) -> anyhow::Result<()> {
    let entries: Vec<HistoryEntry> = match daemon_client::call(&Request::History { n }, Some(Duration::from_secs(5))) {
        Some(resp) if resp.ok => resp.history.unwrap_or_default(),
        _ => flow_core::history::read_recent(n)?,
    };

    if entries.is_empty() {
        println!("(no history yet)");
        return Ok(());
    }

    for entry in entries {
        let app = entry.app.as_deref().unwrap_or("unknown");
        println!("[{}] {:.1}s | mode={} | app={}", entry.ts, entry.duration_s, entry.mode, app);
        println!("  {}", entry.clean_text.trim());
    }
    Ok(())
}
