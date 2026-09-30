//! `Billing` -- internal gRPC gateway to Stripe, plus plan/subscription/invoice
//! and prepaid-credit management. See `src/lib.rs`.

use artisan_middleware::dusa_collection_utils::core::logger::{LogLevel, set_log_level};
use artisan_middleware::dusa_collection_utils::log;
use billing::config::{Config, Secrets};
use billing::error::Result;
use billing::{db, grpc};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "billing", version, about = "Artisan Hosting billing: Stripe gateway, subscriptions and invoicing")]
struct Cli {
    /// Defaults to /opt/artisan/etc/billing.json.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Credentials file. Defaults to /opt/artisan/etc/billing.env.
    #[arg(long, global = true, value_name = "PATH")]
    env_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the gRPC service (the default).
    Serve,
    /// Remind that migrations are applied manually, then exit.
    Migrate,
}

#[tokio::main]
async fn main() {
    set_log_level(LogLevel::Info);

    // reqwest (aws-lc-rs) and tonic's mTLS transport (ring) each link their
    // own rustls crypto backend, so rustls can no longer infer a process
    // default on its own -- the gRPC server's ServerTlsConfig panics on the
    // first accept without this. Must run before anything builds a TLS
    // config: the Stripe client, the mTLS gRPC server and client all do.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls CryptoProvider");

    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        log!(LogLevel::Error, "{}", err);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let config = Config::load(cli.config.as_deref())?;
    let secrets = Secrets::load(cli.env_file.as_deref())?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config, secrets).await,
        Command::Migrate => {
            log!(LogLevel::Info, "migrations must be run manually");
            Ok(())
        }
    }
}

async fn serve(config: Config, secrets: Secrets) -> Result<()> {
    secrets.require(&[("DATABASE_URL", &secrets.database_url)])?;

    let pool = db::connect(&secrets.database_url).await?;
    // db::migrate(&pool).await?;
    log!(LogLevel::Info, "database ready");

    grpc::serve(config, secrets, pool).await
}
