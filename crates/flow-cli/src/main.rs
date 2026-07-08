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
}

#[derive(Subcommand)]
enum ModelsAction {
    /// Download a model. Currently supports: parakeet-v3 (default).
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
    }
}
