//! `Billing` -- internal gRPC gateway to Stripe, and the platform's
//! usage-cost calculator. See `src/lib.rs` for how the two relate.

use artisan_middleware::dusa_collection_utils::core::logger::{LogLevel, set_log_level};
use artisan_middleware::dusa_collection_utils::log;
use billing::config::{Config, Secrets};
use billing::error::Result;
use billing::{db, grpc, usage};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "billing", version, about = "Artisan Hosting billing: usage cost calculation and Stripe gateway")]
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
    /// Run the gRPC service and the usage-cost HTTP server (the default).
    Serve,
    /// Apply pending database migrations and exit.
    Migrate,
}

#[tokio::main]
async fn main() {
    set_log_level(LogLevel::Info);

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
            let pool = db::connect(&secrets.database_url).await?;
            db::migrate(&pool).await?;
            log!(LogLevel::Info, "migrations applied");
            Ok(())
        }
    }
}

async fn serve(config: Config, secrets: Secrets) -> Result<()> {
    secrets.require(&[("DATABASE_URL", &secrets.database_url)])?;

    let pool = db::connect(&secrets.database_url).await?;
    db::migrate(&pool).await?;
    log!(LogLevel::Info, "database ready");

    // The usage-cost route keeps running alongside the gRPC server, just as
    // one task among two now, instead of being the entire process. It shares
    // the same pool as everything else -- it now reads the plan catalog to
    // price overage, not just hardcoded constants.
    let http_bind = config.http.bind.clone();
    let http_pool = pool.clone();
    let http_task = tokio::spawn(async move { usage::serve_http(&http_bind, http_pool).await });

    let grpc_result = grpc::serve(config, secrets, pool).await;

    // `usage::serve_http` never returns on its own either; once the gRPC
    // server has stopped there is nothing left for the HTTP task to serve
    // a result to.
    http_task.abort();

    grpc_result
}
