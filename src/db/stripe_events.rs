//! The webhook inbox (`migrations/0009_stripe_events.sql`).

use sqlx::MySqlPool;

use crate::error::Result;

/// Records that `event_id` arrived and reports whether it was already fully
/// processed. Safe to call any number of times for the same event, including
/// concurrently: the insert is a no-op on a duplicate key.
pub async fn begin(pool: &MySqlPool, event_id: &str, event_type: &str) -> Result<bool> {
    sqlx::query("INSERT INTO stripe_events (event_id, event_type) VALUES (?, ?) ON DUPLICATE KEY UPDATE event_id = event_id")
        .bind(event_id)
        .bind(event_type)
        .execute(pool)
        .await?;

    let processed: bool = sqlx::query_scalar("SELECT processed_at IS NOT NULL FROM stripe_events WHERE event_id = ?")
        .bind(event_id)
        .fetch_one(pool)
        .await?;
    Ok(processed)
}

pub async fn mark_processed(pool: &MySqlPool, event_id: &str) -> Result<()> {
    sqlx::query("UPDATE stripe_events SET processed_at = CURRENT_TIMESTAMP WHERE event_id = ?")
        .bind(event_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_event() -> String {
        format!("evt_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }

    #[tokio::test]
    #[ignore]
    async fn an_event_is_unprocessed_until_marked_and_survives_redelivery() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let id = unique_event();

        assert!(!begin(&pool, &id, "payment_intent.succeeded").await.unwrap(), "first sight is unprocessed");
        assert!(!begin(&pool, &id, "payment_intent.succeeded").await.unwrap(), "redelivery before completion is still unprocessed");

        mark_processed(&pool, &id).await.unwrap();
        assert!(begin(&pool, &id, "payment_intent.succeeded").await.unwrap(), "after marking, redelivery is a known duplicate");
    }
}
