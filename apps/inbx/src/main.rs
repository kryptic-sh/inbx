use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "inbx", version, about = "modal-vim email client")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print resolved config path and account count.
    Config,
    /// Manage accounts.
    Accounts {
        #[command(subcommand)]
        action: AccountCmd,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Config => {
            let path = inbx_config::config_path()?;
            let cfg = inbx_config::load()?;
            println!("config: {}", path.display());
            println!("accounts: {}", cfg.accounts.len());
        }
        Cmd::Accounts { action } => match action {
            AccountCmd::List => {
                let cfg = inbx_config::load()?;
                if cfg.accounts.is_empty() {
                    println!("(no accounts configured)");
                } else {
                    for a in cfg.accounts {
                        println!("{} <{}>", a.name, a.email);
                    }
                }
            }
        },
    }
    Ok(())
}
