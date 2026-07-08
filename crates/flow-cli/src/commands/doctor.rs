use anyhow::Result;
use flow_core::audio::default_input_device_info;
use flow_core::models::{check_parakeet_model, model_root_dir};

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
