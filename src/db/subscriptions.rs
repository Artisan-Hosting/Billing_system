//! Queries behind the `subscriptions` table (see
//! `migrations/0004_subscriptions.sql`). One row per (organization_id,
//! storefront).

use sqlx::{MySqlPool, Row};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct SubscriptionRow {
    pub id: u64,
    pub organization_id: String,
    pub storefront: String,
    pub plan_code: String,
    /// `billing::domain::BillingStatus::as_str_name()`.
    pub status: String,
    pub current_period_start: i64,
    pub current_period_end: i64,
    pub pending_plan_code: Option<String>,
    pub cancel_at_period_end: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

const SELECT_COLUMNS: &str = "id, organization_id, storefront, plan_code, status, \
     UNIX_TIMESTAMP(current_period_start) AS current_period_start, \
     UNIX_TIMESTAMP(current_period_end) AS current_period_end, \
     pending_plan_code, cancel_at_period_end, \
     UNIX_TIMESTAMP(created_at) AS created_at, UNIX_TIMESTAMP(updated_at) AS updated_at";

pub async fn find(pool: &MySqlPool, organization_id: &str, storefront: &str) -> Result<Option<SubscriptionRow>> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM subscriptions WHERE organization_id = ? AND storefront = ?"
    ))
    .bind(organization_id)
    .bind(storefront)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_entry))
}

pub async fn find_by_id(pool: &MySqlPool, id: u64) -> Result<Option<SubscriptionRow>> {
    let row = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM subscriptions WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(row_to_entry))
}

/// Every subscription an organization holds, across every storefront -- what
/// `GetOrganizationBillingStatus` reduces to its most-severe-status summary.
pub async fn list_for_org(pool: &MySqlPool, organization_id: &str) -> Result<Vec<SubscriptionRow>> {
    let rows = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM subscriptions WHERE organization_id = ?"))
        .bind(organization_id)
        .fetch_all(pool)
        .await?;

    Ok(rows.into_iter().map(row_to_entry).collect())
}

/// `current_period_start`/`current_period_end` are Unix seconds. `status` is
/// a `BillingStatus::as_str_name()` value -- callers decide what a brand new
/// subscription's starting status is (see `SubscriptionCheckout`'s own doc
/// comment: it starts `PastDue`, not `Active`, until the first invoice is
/// paid).
pub async fn insert(
    pool: &MySqlPool,
    organization_id: &str,
    storefront: &str,
    plan_code: &str,
    status: &str,
    current_period_start: i64,
    current_period_end: i64,
) -> Result<u64> {
    sqlx::query(
        "INSERT INTO subscriptions \
         (organization_id, storefront, plan_code, status, current_period_start, current_period_end) \
         VALUES (?, ?, ?, ?, FROM_UNIXTIME(?), FROM_UNIXTIME(?))",
    )
    .bind(organization_id)
    .bind(storefront)
    .bind(plan_code)
    .bind(status)
    .bind(current_period_start)
    .bind(current_period_end)
    .execute(pool)
    .await?;

    let id: u64 = sqlx::query("SELECT id FROM subscriptions WHERE organization_id = ? AND storefront = ?")
        .bind(organization_id)
        .bind(storefront)
        .fetch_one(pool)
        .await?
        .get("id");

    Ok(id)
}

/// An immediate, in-place upgrade: new plan, status unchanged (an upgrade on
/// an already-`Active` subscription stays `Active`; the caller decides
/// whether to also flip a `PastDue`/`GracePeriod` one, which this function
/// does not do on its own).
pub async fn set_plan(pool: &MySqlPool, id: u64, plan_code: &str) -> Result<()> {
    sqlx::query("UPDATE subscriptions SET plan_code = ? WHERE id = ?").bind(plan_code).bind(id).execute(pool).await?;
    Ok(())
}

/// Buying again after a cancellation ran out: same row, fresh period, no
/// queued downgrade, no pending cancel.
pub async fn restart(
    pool: &MySqlPool,
    id: u64,
    plan_code: &str,
    status: &str,
    period_start: i64,
    period_end: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE subscriptions SET plan_code = ?, status = ?, pending_plan_code = NULL, cancel_at_period_end = FALSE, \
         current_period_start = FROM_UNIXTIME(?), current_period_end = FROM_UNIXTIME(?) WHERE id = ?",
    )
    .bind(plan_code)
    .bind(status)
    .bind(period_start)
    .bind(period_end)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_status(pool: &MySqlPool, id: u64, status: &str) -> Result<()> {
    sqlx::query("UPDATE subscriptions SET status = ? WHERE id = ?").bind(status).bind(id).execute(pool).await?;
    Ok(())
}

/// Sets the status of whichever subscription a given invoice belongs to --
/// used by the Stripe webhook handler, which only knows the invoice/payment
/// intent, not the subscription id directly.
pub async fn set_status_for_invoice(pool: &MySqlPool, invoice_id: u64, status: &str) -> Result<()> {
    sqlx::query(
        "UPDATE subscriptions s \
         JOIN invoices i ON i.subscription_id = s.id \
         SET s.status = ? WHERE i.id = ?",
    )
    .bind(status)
    .bind(invoice_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Queues `plan_code` to take effect at `current_period_end` -- does not
/// touch `plan_code` itself.
pub async fn schedule_downgrade(pool: &MySqlPool, id: u64, plan_code: &str) -> Result<()> {
    sqlx::query("UPDATE subscriptions SET pending_plan_code = ? WHERE id = ?")
        .bind(plan_code)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_cancel_at_period_end(pool: &MySqlPool, id: u64, cancel: bool) -> Result<()> {
    sqlx::query("UPDATE subscriptions SET cancel_at_period_end = ? WHERE id = ?")
        .bind(cancel)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

fn row_to_entry(row: sqlx::mysql::MySqlRow) -> SubscriptionRow {
    SubscriptionRow {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        storefront: row.get("storefront"),
        plan_code: row.get("plan_code"),
        status: row.get("status"),
        current_period_start: row.get("current_period_start"),
        current_period_end: row.get("current_period_end"),
        pending_plan_code: row.get("pending_plan_code"),
        cancel_at_period_end: row.get("cancel_at_period_end"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a real, migrated database (`DATABASE_URL`) -- `cargo test --
    /// --ignored`, matching `db::plans::tests`' own gating.
    #[tokio::test]
    #[ignore]
    async fn a_subscription_round_trips_through_insert_and_find() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = uuid_v4_like();

        let id = insert(&pool, &org, "developer", "dev_pro", "past_due", 1_000, 2_000).await.expect("insert");
        let row = find(&pool, &org, "developer").await.expect("find").expect("row exists");
        assert_eq!(row.id, id);
        assert_eq!(row.plan_code, "dev_pro");
        assert_eq!(row.status, "past_due");
        assert_eq!(row.current_period_start, 1_000);
        assert_eq!(row.current_period_end, 2_000);
        assert_eq!(row.pending_plan_code, None);
        assert!(!row.cancel_at_period_end);

        set_status(&pool, id, "active").await.expect("set_status");
        set_plan(&pool, id, "dev_team").await.expect("set_plan");
        schedule_downgrade(&pool, id, "dev_builder").await.expect("schedule_downgrade");
        set_cancel_at_period_end(&pool, id, true).await.expect("set_cancel_at_period_end");

        let updated = find_by_id(&pool, id).await.expect("find_by_id").expect("row exists");
        assert_eq!(updated.status, "active");
        assert_eq!(updated.plan_code, "dev_team");
        assert_eq!(updated.pending_plan_code.as_deref(), Some("dev_builder"));
        assert!(updated.cancel_at_period_end);
    }

    #[tokio::test]
    #[ignore]
    async fn list_for_org_returns_every_storefront() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = uuid_v4_like();

        insert(&pool, &org, "developer", "dev_pro", "active", 1_000, 2_000).await.expect("insert developer");
        insert(&pool, &org, "business", "biz_care", "active", 1_000, 2_000).await.expect("insert business");

        let subs = list_for_org(&pool, &org).await.expect("list_for_org");
        assert_eq!(subs.len(), 2);
    }

    fn uuid_v4_like() -> String {
        // Not a real UUID, just unique enough to avoid colliding with
        // another test's rows in a shared test database.
        format!("test-org-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }
}
