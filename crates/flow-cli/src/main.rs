mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "flow", about = "VZT Flow — local voice dictation CLI (Phase 1)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Record from the mic until Enter is pressed, then transcribe.
    Listen {
        /// Auto-stop after this many seconds instead of waiting for Enter.
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Transcribe an existing audio file (wav, or anything ffmpeg can read).
    Transcribe {
        file: std::path::PathBuf,
    },
    /// Manage local models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Print environment/model/device diagnostics.
    Doctor,
    /// Hidden: exercise the clipboard save/set/paste/restore pipeline in
    /// isolation without needing the desktop app running.
    #[command(hide = true)]
    PasteTest {
        text: String,
    },
    /// Hidden: run the LLM cleanup pass on arbitrary text, outside the
    /// desktop app, and report which path (llm vs. timeout/raw) won plus
    /// the latency.
    #[command(hide = true)]
    CleanTest {
        text: String,
        /// "clean" or "polish".
        #[arg(long, default_value = "clean")]
        mode: String,
        #[arg(long, default_value_t = 2500)]
        timeout_ms: u64,
    },
    /// Hidden: run the deterministic code-mode transform on arbitrary text.
    #[command(hide = true)]
    CodeTest {
        text: String,
    },
}

#[derive(Subcommand)]
enum ModelsAction {
    /// Download a model. Supports: parakeet-v3 (default), cleanup.
    Download {
        #[arg(default_value = "parakeet-v3")]
        model: String,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Listen { seconds } => commands::listen::run(seconds),
        Commands::Transcribe { file } => commands::transcribe::run(&file),
        Commands::Models { action } => match action {
            ModelsAction::Download { model, force } => commands::models::download(&model, force),
        },
        Commands::Doctor => commands::doctor::run(),
        Commands::PasteTest { text } => commands::paste_test::run(&text),
        Commands::CleanTest { text, mode, timeout_ms } => commands::clean_test::run(&text, &mode, timeout_ms),
        Commands::CodeTest { text } => commands::code_test::run(&text),
    }
}
