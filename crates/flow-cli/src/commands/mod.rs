pub mod cancel;
pub mod clean_test;
pub mod code_test;
pub mod daemon_client;
pub mod doctor;
pub mod history;
pub mod listen;
pub mod meeting;
pub mod models;
pub mod paste_test;
pub mod status;
pub mod toggle;
pub mod transcribe;

/// Thin wrapper kept for call-site compatibility: the actual file-loading
/// logic lives in `flow_core::audio::load_audio_file_as_f32` now, shared
/// with the daemon's `transcribe` socket command.
pub(crate) fn load_audio_as_f32(path: &std::path::Path) -> anyhow::Result<(Vec<f32>, std::time::Duration)> {
    flow_core::audio::load_audio_file_as_f32(path)
}
