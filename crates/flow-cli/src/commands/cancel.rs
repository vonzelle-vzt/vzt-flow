use std::time::Duration;

use flow_core::ipc::Request;

use super::daemon_client;

pub fn run() -> anyhow::Result<()> {
    match daemon_client::call_required(&Request::Cancel, Some(Duration::from_secs(5))) {
        Ok(resp) if resp.ok => {
            println!("cancelled (state: {})", resp.state.as_deref().unwrap_or("unknown"));
            Ok(())
        }
        Ok(resp) => {
            anyhow::bail!("daemon error: {}", resp.error.as_deref().unwrap_or("unknown error"))
        }
        Err(e) => anyhow::bail!("daemon not running (or unreachable): {e}"),
    }
}
