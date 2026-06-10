use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "azpect", version, about = "Azure API observability TUI")]
struct Cli {
    /// Browse a built-in mock tenant (fictional data, zero Azure calls).
    /// No login required; nothing is read from or written to your account.
    /// Useful for screenshots and demos.
    #[arg(long, global = true)]
    demo: bool,

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
        // Demo mode never builds a credential chain — the mock tenant needs no
        // login and AzureAuth::demo() refuses every token scope, so no request
        // can reach a live tenant.
        let auth = if cli.demo {
            azpect::azure::auth::AzureAuth::demo()
        } else {
            azpect::azure::auth::AzureAuth::new().await?
        };
        let cfg = azpect::config::load()?;

        match cli.command {
            Some(Command::DebugAuth) => {
                if cli.demo {
                    println!("demo mode: no real credential; mock subscriptions follow.");
                }
                let subs = if cli.demo {
                    azpect::azure::demo::subscriptions()
                } else {
                    azpect::azure::subscriptions::list(&auth).await?
                };
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
