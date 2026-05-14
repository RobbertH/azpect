use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "azpect", version, about = "Azure API observability TUI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print resolved auth + listed subscriptions, then exit. Useful for diagnosing credential setup.
    DebugAuth,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let auth = azpect::azure::auth::AzureAuth::new().await?;
        let cfg = azpect::config::load()?;

        match cli.command {
            Some(Command::DebugAuth) => {
                let subs = azpect::azure::subscriptions::list(&auth).await?;
                println!("Resolved {} subscription(s):", subs.len());
                for s in &subs {
                    println!("  {}  {}  ({})", s.id, s.display_name, s.state);
                }
                Ok(())
            }
            None => azpect::ui::app::run(auth, cfg).await,
        }
    })
}
