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
    /// Check a world pack and its scripts (with a dry run), and print a
    /// summary. Exits 1 and lists every problem if any.
    Check {
        /// The pack folder.
        #[arg(default_value = "worlds/crypt-v01")]
        pack: PathBuf,
    },
    /// Manage player accounts in accounts.db (works while the server runs).
    Account {
        /// The config file; defaults apply when it does not exist.
        #[arg(long, env = "MUD_CONFIG", default_value = "mud.toml", global = true)]
        config: PathBuf,
        #[command(subcommand)]
        op: AccountCmd,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// List every account: origin, dates, browsers, lock.
    List,
    /// Print a temporary password, once; it must be changed at the next login.
    Reset {
        /// The character.
        name: String,
    },
    /// Refuse logins; live sessions end within `revalidate_secs`.
    Lock {
        /// The character.
        name: String,
    },
    /// Allow logins again.
    Unlock {
        /// The character.
        name: String,
    },
    /// Forget every remembered browser and end live sessions.
    Signout {
        /// The character.
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::util::SubscriberInitExt as _;
    // Wire frames carry passwords and tokens: `logging` never lets them
    // through, whatever MUD_LOG says.
    let spec = std::env::var("MUD_LOG").unwrap_or_else(|_| "info".to_owned());
    mud_server::logging::subscriber(&spec, std::io::stdout).init();
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
            println!(
                "\nsmoke: PASS (bounty posted, board read, human recruited, account kept, hidden PC guarded, the alley answered, one bounty, one budget and one answered tell across a restart)"
            );
            Ok(())
        }
        Command::Check { pack } => {
            for line in mud_server::check_pack(&pack)? {
                println!("{line}");
            }
            Ok(())
        }
        Command::Account { config, op } => {
            use mud_server::account_cli::{AccountOp, run};
            let cfg = mud_server::load_config(&config)?;
            let op = match op {
                AccountCmd::List => AccountOp::List,
                AccountCmd::Reset { name } => AccountOp::Reset(name),
                AccountCmd::Lock { name } => AccountOp::Lock(name),
                AccountCmd::Unlock { name } => AccountOp::Unlock(name),
                AccountCmd::Signout { name } => AccountOp::Signout(name),
            };
            let now = mud_world::Clock::now_ms(&mud_world::SystemClock);
            println!("{}", run(&cfg, op, now).await?);
            Ok(())
        }
    }
}
