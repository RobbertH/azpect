use std::sync::Mutex;

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

/// Install the global tracing subscriber. `tui` selects the writer: the TUI
/// must keep its terminal pristine, so it logs to a file (best-effort — if the
/// file can't be opened we discard logs rather than fall back to stderr, which
/// would corrupt the rendered frame). The one-shot CLI keeps stderr.
fn init_tracing(tui: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    if !tui {
        builder.with_writer(std::io::stderr).init();
        return;
    }

    // TUI: route to a log file under the cache dir. On any failure, fall back to
    // a sink that discards output — never stderr, which shares the alt screen.
    let file = azpect::config::log_path().ok().and_then(|path| {
        path.parent().map(std::fs::create_dir_all);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
    });
    match file {
        Some(f) => builder.with_ansi(false).with_writer(Mutex::new(f)).init(),
        None => builder.with_writer(std::io::sink).init(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Logging destination depends on the command. The TUI owns the terminal via
    // an alternate screen, and stderr writes land on that same surface — a
    // `tracing` line would paint raw text *outside* ratatui's cell buffer, where
    // it never gets cleared (it survives resize/scroll/refresh, looking like
    // "random characters" on the rendered frame). So the TUI logs to a file; the
    // one-shot `debug-auth` subcommand has no TUI and keeps stderr.
    init_tracing(cli.command.is_none());

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
