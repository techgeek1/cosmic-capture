use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cosmic_capture::cli::{Cli, Command};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cosmic_capture=info,warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => {
            tracing::info!("no subcommand → GUI mode");
            cosmic_capture::gui::launch()
        }
        Some(cmd) => {
            tracing::info!(?cmd, "CLI mode");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_cli(cmd))
        }
    }
}

async fn run_cli(cmd: Command) -> Result<()> {
    use cosmic_capture::pipeline;
    match cmd {
        Command::Screenshot(args) => pipeline::screenshot::run(args).await,
        Command::Record(args) => pipeline::record::run(args).await,
        Command::Gif(args) => pipeline::gif::run(args).await,
    }
}
