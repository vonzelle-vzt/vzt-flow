use std::time::Duration;

use anyhow::Result;
use flow_core::audio::default_input_device_info;
use flow_core::config::Config;
use flow_core::hotkey::{
    hotkey_keycode_is_hold_capable, hotkey_keycode_is_supported, supported_hotkey_keycodes,
};
use flow_core::ipc::{transport, Request};
use flow_core::models::{check_cleanup_model, check_parakeet_model, model_root_dir};

pub fn run() -> Result<()> {
    println!("VZT Flow doctor");
    println!("===============");
    println!("flow-cli version: {}", env!("CARGO_PKG_VERSION"));
    println!("rustc: {}", rustc_version());

    match model_root_dir() {
        Ok(dir) => println!("Model root dir: {}", dir.display()),
        Err(e) => println!("Model root dir: error ({e})"),
    }

    match check_parakeet_model() {
        Ok(status) => {
            println!("Parakeet v3 model dir: {}", status.dir.display());
            if status.present {
                println!("Parakeet v3 model: PRESENT");
            } else {
                println!("Parakeet v3 model: MISSING");
                if !status.missing_stems.is_empty() {
                    println!("  Missing components: {}", status.missing_stems.join(", "));
                }
                println!("  Run: flow models download parakeet-v3");
            }
        }
        Err(e) => println!("Parakeet v3 model: error checking status ({e})"),
    }

    match default_input_device_info() {
        Ok(info) => {
            println!(
                "Default input device: {} ({} Hz, {} channel(s))",
                info.name, info.sample_rate, info.channels
            );
        }
        Err(e) => println!("Default input device: error ({e})"),
    }

    let ffmpeg = std::process::Command::new("ffmpeg").arg("-version").output();
    match ffmpeg {
        Ok(out) if out.status.success() => {
            let first_line = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            println!("ffmpeg: {first_line}");
        }
        _ => println!("ffmpeg: NOT FOUND on PATH"),
    }

    match check_cleanup_model() {
        Ok(true) => println!("Cleanup model: PRESENT"),
        Ok(false) => println!("Cleanup model: MISSING (run: flow models download cleanup)"),
        Err(e) => println!("Cleanup model: error checking status ({e})"),
    }

    match Config::load() {
        Ok(cfg) => {
            let kc = cfg.hotkey_keycode;
            if hotkey_keycode_is_hold_capable(kc) {
                println!("Hotkey: {} (keycode {kc}) — supported", cfg.hotkey_label);
            } else if kc == 57 {
                // Caps Lock: modifier_bit_for_keycode(57) reads
                // CGEventFlagAlphaShift, the *latched* state (LED on/off),
                // not physical hold state — so it toggles instead of
                // holding. See hotkey.rs::HOLD_CAPABLE_HOTKEY_KEYCODES.
                println!(
                    "Hotkey: {} (keycode {kc}) — toggle semantics, not hold-to-talk (not recommended)",
                    cfg.hotkey_label
                );
            } else if hotkey_keycode_is_supported(kc) {
                println!(
                    "Hotkey: {} (keycode {kc}) — supported but not hold-capable",
                    cfg.hotkey_label
                );
            } else {
                let valid = supported_hotkey_keycodes()
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "Hotkey: keycode {kc} — UNSUPPORTED (hotkey will never fire; valid: {valid})"
                );
            }
        }
        Err(e) => println!("Hotkey: error loading config ({e})"),
    }

    match flow_core::ipc::socket_path() {
        Ok(path) => {
            if !path.exists() {
                println!("Daemon socket: not present ({})", path.display());
            } else if transport::is_alive(&path) {
                println!("Daemon socket: PRESENT and alive ({})", path.display());
                match transport::call(&path, &Request::Status, Some(Duration::from_secs(5))) {
                    Ok(resp) if resp.ok => {
                        println!("Daemon version: {}", resp.version.as_deref().unwrap_or("unknown"));
                        println!("Daemon state: {}", resp.state.as_deref().unwrap_or("unknown"));
                    }
                    Ok(resp) => println!("Daemon status query failed: {}", resp.error.as_deref().unwrap_or("?")),
                    Err(e) => println!("Daemon status query failed: {e}"),
                }
            } else {
                println!("Daemon socket: STALE file present, nothing listening ({})", path.display());
            }
        }
        Err(e) => println!("Daemon socket: error determining path ({e})"),
    }

    match std::process::Command::new("claude").args(["mcp", "list"]).output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains("vzt-flow") {
                println!("MCP registration: vzt-flow IS registered with `claude mcp`");
            } else {
                println!("MCP registration: `claude` binary found, but vzt-flow is NOT registered");
                println!("  Run: claude mcp add vzt-flow --scope user -- node <path to mcp/dist/index.js>");
            }
        }
        Ok(_) => println!("MCP registration: `claude mcp list` exited non-zero; could not check"),
        Err(_) => println!("MCP registration: `claude` binary not found on PATH; skipping check"),
    }

    Ok(())
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string())
        .trim()
        .to_string()
}
