mod app;
mod config;
mod context;
mod model;
mod openrouter;
mod shell;

use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "ash - a blazing-fast AI shell",
    long_about = None
)]
struct Cli {
    #[arg(value_name = "PROMPT")]
    prompt: Vec<String>,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    refresh_models: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = config::Config::load(cli.config)?;

    let launch = app::LaunchOptions {
        initial_query: if cli.prompt.is_empty() {
            None
        } else {
            Some(cli.prompt.join(" "))
        },
        initial_model: cli.model,
        force_model_refresh: cli.refresh_models,
    };

    app::run(config, launch).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}
