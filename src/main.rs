use anyhow::Result;
use clap::Parser;
use pcstui::cli::{Cli, run};

#[tokio::main]
async fn main() {
    if let Err(err) = real_main().await {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let cli = Cli::parse();
    run(cli).await
}
