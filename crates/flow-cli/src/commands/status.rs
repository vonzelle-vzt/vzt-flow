use std::time::Duration;

use flow_core::ipc::Request;

use super::daemon_client;

pub fn run() -> anyhow::Result<()> {
    match daemon_client::call(&Request::Status, Some(Duration::from_secs(5))) {
        Some(resp) if resp.ok => {
            println!("daemon: running");
            println!("state: {}", resp.state.as_deref().unwrap_or("unknown"));
            println!("model loaded: {}", resp.model_loaded.unwrap_or(false));
            println!("cleanup model loaded: {}", resp.cleanup_loaded.unwrap_or(false));
            println!("version: {}", resp.version.as_deref().unwrap_or("unknown"));
        }
        Some(resp) => {
            println!("daemon: running");
            println!("error: {}", resp.error.as_deref().unwrap_or("unknown error"));
        }
        None => {
            println!("daemon: not running");
        }
    }
    Ok(())
}
