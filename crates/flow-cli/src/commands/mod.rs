pub mod doctor;
pub mod listen;
pub mod models;
pub mod paste_test;
pub mod transcribe;

/// Shared helper: convert an arbitrary audio file to a 16kHz mono f32
/// sample buffer, using `hound` directly for wav and shelling out to the
/// system `ffmpeg` for anything else.
pub(crate) fn load_audio_as_f32(path: &std::path::Path) -> anyhow::Result<(Vec<f32>, std::time::Duration)> {
    use anyhow::Context;

    let is_wav = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false);

    let wav_path: std::path::PathBuf = if is_wav {
        path.to_path_buf()
    } else {
        let tmp = std::env::temp_dir().join(format!(
            "flow-convert-{}.wav",
            std::process::id()
        ));
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(path)
            .args(["-ar", "16000", "-ac", "1", "-f", "wav"])
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to invoke ffmpeg (is it installed and on PATH?)")?;
        if !status.success() {
            anyhow::bail!("ffmpeg conversion failed for {}", path.display());
        }
        tmp
    };

    let mut reader = hound::WavReader::open(&wav_path)
        .with_context(|| format!("failed to open wav {}", wav_path.display()))?;
    let spec = reader.spec();
    let duration = std::time::Duration::from_secs_f64(reader.duration() as f64 / spec.sample_rate as f64);

    let raw_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .context("failed to read f32 wav samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<Vec<_>, _>>()
                .context("failed to read int wav samples")?
        }
    };

    let mono = if spec.channels > 1 {
        raw_samples
            .chunks(spec.channels as usize)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    } else {
        raw_samples
    };

    let resampled = flow_core::audio::resample_linear(&mono, spec.sample_rate, flow_core::audio::TARGET_SAMPLE_RATE);

    if !is_wav {
        let _ = std::fs::remove_file(&wav_path);
    }

    Ok((resampled, duration))
}
