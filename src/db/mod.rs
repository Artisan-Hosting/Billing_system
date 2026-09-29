//! Database access.
//!
//! This service owns its schema and runs its own migrations at startup
//! (`migrations/`), matching `domain_management`'s own convention.

pub mod credits;
pub mod invoices;
pub mod payment_intents;
pub mod plans;
pub mod stripe_events;
pub mod subscriptions;

use sqlx::{MySqlPool, mysql::MySqlPoolOptions};
use std::time::Duration;

use crate::error::{Error, Result};

pub async fn connect(database_url: &str) -> Result<MySqlPool> {
    if database_url.is_empty() {
        return Err(Error::Config("DATABASE_URL is not set (env file or environment)".to_owned()));
    }

    MySqlPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .map_err(Error::Database)
}

