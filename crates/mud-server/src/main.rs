//! `mud`: the littleguy MUD server.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "mud",
    version,
    about = "A text MUD where humans and littleguy agents share one world"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the terminal, telnet, the observer and MCP.
    Serve {
        /// The config file; defaults apply when it does not exist.
        #[arg(long, env = "MUD_CONFIG", default_value = "mud.toml")]
        config: PathBuf,
        /// Override `[server] bind`.
        #[arg(long)]
        bind: Option<std::net::SocketAddr>,
        /// Override `[world] db`; `:memory:` keeps the world in memory.
        #[arg(long)]
        db: Option<String>,
    },
    /// Run the headless v0.1 smoke scenario and print its transcript.
    Smoke,
    /// Check a world pack and print a summary.
    Check {
        /// The pack folder.
        #[arg(default_value = "worlds/crypt-v01")]
        pack: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MUD_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Serve { config, bind, db } => {
            let mut cfg = mud_server::load_config(&config)?;
            if let Some(b) = bind {
                cfg.server.bind = b;
            }
            if let Some(db) = db {
                cfg.world.db = (db != ":memory:").then(|| PathBuf::from(db));
            }
            mud_server::serve(cfg).await
        }
        Command::Smoke => {
            let t = mud_server::smoke::run().await?;
            for l in &t.lines {
                println!("{l}");
            }
            println!("\nwakes:");
            for w in &t.wakes {
                println!("  {w}");
            }
            println!("\nsmoke: PASS (bounty posted, board read, human recruited)");
            Ok(())
        }
        Command::Check { pack } => {
            let p = mud_world::WorldPack::load(&pack)?;
            println!(
                "{}: {} rooms, {} items, {} skills, {} cast, {} shops — ok",
                p.world.name,
                p.rooms.len(),
                p.items.len(),
                p.skills.len(),
                p.cast.len(),
                p.economy.shops.len()
            );
            Ok(())
        }
    }
}
