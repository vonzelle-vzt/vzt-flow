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
    /// Record from the mic and transcribe. Daemon-first (records through
    /// the running desktop app, driving its overlay) with a standalone
    /// fallback when no daemon is running.
    Listen {
        /// Pipeline mode: raw|clean|polish|code. Defaults to the daemon's
        /// per-app profile when a daemon is running, else "clean".
        #[arg(long)]
        mode: Option<String>,
        /// Hard cap on recording duration. Standalone mode also accepts no
        /// value and waits for Enter instead.
        #[arg(long)]
        max_secs: Option<u64>,
    },
    /// Transcribe an existing audio file (wav, or anything ffmpeg can read).
    Transcribe {
        file: std::path::PathBuf,
        /// Run the transcript through cleanup/code-mode: raw|clean|polish|code.
        #[arg(long, alias = "clean")]
        mode: Option<String>,
    },
    /// Live-transcribe a meeting (Zoom/Meet/Teams) fully locally: captures
    /// system/app audio (the other participants) and your microphone at once,
    /// writing a timestamped transcript. Ctrl+C stops and appends a summary.
    /// `flow meeting list` shows recent transcripts. macOS only.
    Meeting {
        /// Meeting title (used in the header and filename). Defaults to "meeting".
        #[arg(long)]
        title: Option<String>,
        /// Output directory. Defaults to ~/Documents/vzt-flow/meetings/.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        #[command(subcommand)]
        action: Option<MeetingAction>,
    },
    /// Manage local models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Print environment/model/device diagnostics.
    Doctor,
    /// Query the running daemon's status (idle/recording/transcribing,
    /// models loaded, version). Reports "daemon not running" if unreachable.
    Status,
    /// Start/stop a hands-free recording on the running daemon (same as
    /// the tray's Start/Stop dictation item).
    Toggle,
    /// Cancel the daemon's in-progress recording, if any.
    Cancel,
    /// Show recent dictation history.
    History {
        #[arg(short = 'n', long, default_value_t = 20)]
        n: usize,
    },
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
        /// Deadline override, in ms. Defaults to the same length-scaled
        /// formula the desktop app uses (`cleanup_manager::cleanup_deadline_ms`
        /// against the local config) rather than a flat value, so this tool
        /// exercises the real deadline a long dictation would get.
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Hidden: run the deterministic code-mode transform on arbitrary text.
    #[command(hide = true)]
    CodeTest {
        text: String,
    },
}

#[derive(Subcommand)]
enum MeetingAction {
    /// List recent meeting transcripts (newest first).
    List {
        #[arg(short = 'n', long, default_value_t = 10)]
        n: usize,
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
        Commands::Listen { mode, max_secs } => commands::listen::run(mode, max_secs),
        Commands::Transcribe { file, mode } => commands::transcribe::run(&file, mode.as_deref()),
        Commands::Meeting { title, out, action } => match action {
            Some(MeetingAction::List { n }) => commands::meeting::list(n),
            None => commands::meeting::run(title, out),
        },
        Commands::Models { action } => match action {
            ModelsAction::Download { model, force } => commands::models::download(&model, force),
        },
        Commands::Doctor => commands::doctor::run(),
        Commands::Status => commands::status::run(),
        Commands::Toggle => commands::toggle::run(),
        Commands::Cancel => commands::cancel::run(),
        Commands::History { n } => commands::history::run(n),
        Commands::PasteTest { text } => commands::paste_test::run(&text),
        Commands::CleanTest { text, mode, timeout_ms } => commands::clean_test::run(&text, &mode, timeout_ms),
        Commands::CodeTest { text } => commands::code_test::run(&text),
    }
}
