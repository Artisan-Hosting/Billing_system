//! Queries behind `invoices`/`invoice_line_items` (see
//! `migrations/0005_invoices.sql`).

use sqlx::{MySqlPool, Row};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct InvoiceRow {
    pub id: u64,
    pub organization_id: String,
    pub subscription_id: Option<u64>,
    pub period_start: i64,
    pub period_end: i64,
    /// draft | open | paid | void | uncollectible.
    pub status: String,
    pub total_cents: i64,
    pub currency: String,
    pub stripe_payment_intent_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct InvoiceLineItemRow {
    pub id: u64,
    pub invoice_id: u64,
    pub unit_code: Option<String>,
    pub description: String,
    pub quantity: f64,
    pub unit_price_cents: f64,
    pub amount_cents: i64,
}

const SELECT_INVOICE_COLUMNS: &str = "id, organization_id, subscription_id, \
     UNIX_TIMESTAMP(period_start) AS period_start, UNIX_TIMESTAMP(period_end) AS period_end, \
     status, total_cents, currency, stripe_payment_intent_id, \
     UNIX_TIMESTAMP(created_at) AS created_at, UNIX_TIMESTAMP(updated_at) AS updated_at";

/// Starts as `draft`, `total_cents = 0` -- the caller adds line items with
/// [`insert_line_item`] and then calls [`finalize`] once the total is known.
pub async fn insert_draft(
    pool: &MySqlPool,
    organization_id: &str,
    subscription_id: Option<u64>,
    period_start: i64,
    period_end: i64,
    currency: &str,
) -> Result<u64> {
    let result = sqlx::query(
        "INSERT INTO invoices (organization_id, subscription_id, period_start, period_end, status, currency) \
         VALUES (?, ?, FROM_UNIXTIME(?), FROM_UNIXTIME(?), 'draft', ?)",
    )
    .bind(organization_id)
    .bind(subscription_id)
    .bind(period_start)
    .bind(period_end)
    .bind(currency)
    .execute(pool)
    .await?;

    Ok(result.last_insert_id())
}

pub async fn insert_line_item(
    pool: &MySqlPool,
    invoice_id: u64,
    unit_code: Option<&str>,
    description: &str,
    quantity: f64,
    unit_price_cents: f64,
    amount_cents: i64,
) -> Result<u64> {
    let result = sqlx::query(
        "INSERT INTO invoice_line_items (invoice_id, unit_code, description, quantity, unit_price_cents, amount_cents) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(invoice_id)
    .bind(unit_code)
    .bind(description)
    .bind(quantity)
    .bind(unit_price_cents)
    .bind(amount_cents)
    .execute(pool)
    .await?;

    Ok(result.last_insert_id())
}

/// Sets the invoice's total and moves it out of `draft` -- `open` (awaiting
/// payment) unless `total_cents` is 0, in which case there's nothing to
/// collect and it goes straight to `paid`.
pub async fn finalize(pool: &MySqlPool, invoice_id: u64, total_cents: i64) -> Result<()> {
    let status = if total_cents <= 0 { "paid" } else { "open" };
    sqlx::query("UPDATE invoices SET total_cents = ?, status = ? WHERE id = ?")
        .bind(total_cents)
        .bind(status)
        .bind(invoice_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_stripe_payment_intent(pool: &MySqlPool, invoice_id: u64, stripe_payment_intent_id: &str) -> Result<()> {
    sqlx::query("UPDATE invoices SET stripe_payment_intent_id = ? WHERE id = ?")
        .bind(stripe_payment_intent_id)
        .bind(invoice_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Marks the invoice tied to `stripe_payment_intent_id` as `paid`. Returns
/// the updated row, or `None` if no invoice is tied to that PaymentIntent
/// (most webhook events aren't for a subscription invoice at all -- see
/// `grpc::service::handle_stripe_webhook`, which already has the same
/// "not every event is ours" shape for `payment_intents`).
pub async fn mark_paid_by_payment_intent(pool: &MySqlPool, stripe_payment_intent_id: &str) -> Result<Option<InvoiceRow>> {
    let result = sqlx::query("UPDATE invoices SET status = 'paid' WHERE stripe_payment_intent_id = ?")
        .bind(stripe_payment_intent_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }

    find_by_stripe_payment_intent(pool, stripe_payment_intent_id).await
}

pub async fn find(pool: &MySqlPool, id: u64) -> Result<Option<InvoiceRow>> {
    let row = sqlx::query(&format!("SELECT {SELECT_INVOICE_COLUMNS} FROM invoices WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_invoice))
}

pub async fn find_by_stripe_payment_intent(
    pool: &MySqlPool,
    stripe_payment_intent_id: &str,
) -> Result<Option<InvoiceRow>> {
    let row = sqlx::query(&format!("SELECT {SELECT_INVOICE_COLUMNS} FROM invoices WHERE stripe_payment_intent_id = ?"))
        .bind(stripe_payment_intent_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_invoice))
}

/// `storefront`, when set, filters to invoices whose subscription is in that
/// storefront -- a one-off invoice with no subscription behind it is
/// excluded whenever a storefront filter is given, since it has no
/// storefront to match.
pub async fn list_for_org(
    pool: &MySqlPool,
    organization_id: &str,
    storefront: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<InvoiceRow>> {
    const SELECT_INVOICE_COLUMNS_JOINED: &str = "i.id, i.organization_id, i.subscription_id, \
         UNIX_TIMESTAMP(i.period_start) AS period_start, UNIX_TIMESTAMP(i.period_end) AS period_end, \
         i.status, i.total_cents, i.currency, i.stripe_payment_intent_id, \
         UNIX_TIMESTAMP(i.created_at) AS created_at, UNIX_TIMESTAMP(i.updated_at) AS updated_at";

    let rows = match storefront {
        Some(storefront) => {
            sqlx::query(&format!(
                "SELECT {SELECT_INVOICE_COLUMNS_JOINED} FROM invoices i \
                 JOIN subscriptions s ON s.id = i.subscription_id \
                 WHERE i.organization_id = ? AND s.storefront = ? \
                 ORDER BY i.created_at DESC LIMIT ? OFFSET ?"
            ))
            .bind(organization_id)
            .bind(storefront)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(&format!(
                "SELECT {SELECT_INVOICE_COLUMNS} FROM invoices WHERE organization_id = ? \
                 ORDER BY created_at DESC LIMIT ? OFFSET ?"
            ))
            .bind(organization_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows.into_iter().map(row_to_invoice).collect())
}

/// Same filter as [`list_for_org`], for paging.
pub async fn count_for_org(pool: &MySqlPool, organization_id: &str, storefront: Option<&str>) -> Result<i64> {
    let count: i64 = match storefront {
        Some(storefront) => {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM invoices i JOIN subscriptions s ON s.id = i.subscription_id \
                 WHERE i.organization_id = ? AND s.storefront = ?",
            )
            .bind(organization_id)
            .bind(storefront)
            .fetch_one(pool)
            .await?
        }
        None => {
            sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE organization_id = ?")
                .bind(organization_id)
                .fetch_one(pool)
                .await?
        }
    };
    Ok(count)
}

pub async fn line_items(pool: &MySqlPool, invoice_id: u64) -> Result<Vec<InvoiceLineItemRow>> {
    let rows = sqlx::query(
        "SELECT id, invoice_id, unit_code, description, CAST(quantity AS DOUBLE) AS quantity, \
         CAST(unit_price_cents AS DOUBLE) AS unit_price_cents, amount_cents \
         FROM invoice_line_items WHERE invoice_id = ? ORDER BY id",
    )
    .bind(invoice_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| InvoiceLineItemRow {
            id: row.get("id"),
            invoice_id: row.get("invoice_id"),
            unit_code: row.get("unit_code"),
            description: row.get("description"),
            quantity: row.get("quantity"),
            unit_price_cents: row.get("unit_price_cents"),
            amount_cents: row.get("amount_cents"),
        })
        .collect())
}

fn row_to_invoice(row: sqlx::mysql::MySqlRow) -> InvoiceRow {
    InvoiceRow {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        subscription_id: row.get("subscription_id"),
        period_start: row.get("period_start"),
        period_end: row.get("period_end"),
        status: row.get("status"),
        total_cents: row.get("total_cents"),
        currency: row.get("currency"),
        stripe_payment_intent_id: row.get("stripe_payment_intent_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn an_invoice_and_its_line_items_round_trip() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = format!("test-org-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());

        let invoice_id = insert_draft(&pool, &org, None, 1_000, 2_000, "usd").await.expect("insert_draft");
        insert_line_item(&pool, invoice_id, None, "Pro plan", 1.0, 3200.0, 3200).await.expect("base line item");
        insert_line_item(&pool, invoice_id, Some("ram_gb_month"), "RAM overage", 1.0, 1000.0, 1000)
            .await
            .expect("overage line item");
        finalize(&pool, invoice_id, 4200).await.expect("finalize");

        let invoice = find(&pool, invoice_id).await.expect("find").expect("row exists");
        assert_eq!(invoice.total_cents, 4200);
        assert_eq!(invoice.status, "open");

        let items = line_items(&pool, invoice_id).await.expect("line_items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].amount_cents, 3200);
        assert_eq!(items[1].unit_code.as_deref(), Some("ram_gb_month"));

        let pi_id = format!("pi_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        set_stripe_payment_intent(&pool, invoice_id, &pi_id).await.expect("set_stripe_payment_intent");
        let paid = mark_paid_by_payment_intent(&pool, &pi_id).await.expect("mark_paid").expect("row exists");
        assert_eq!(paid.status, "paid");
        assert_eq!(paid.id, invoice_id);
    }

    #[tokio::test]
    #[ignore]
    async fn list_for_org_filters_by_storefront_via_its_subscription() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = format!("test-org-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());

        let dev_sub = crate::db::subscriptions::insert(&pool, &org, "developer", "dev_pro", "active", 1_000, 2_000)
            .await
            .expect("insert developer subscription");
        let biz_sub = crate::db::subscriptions::insert(&pool, &org, "business", "biz_care", "active", 1_000, 2_000)
            .await
            .expect("insert business subscription");

        insert_draft(&pool, &org, Some(dev_sub), 1_000, 2_000, "usd").await.expect("dev invoice");
        insert_draft(&pool, &org, Some(biz_sub), 1_000, 2_000, "usd").await.expect("biz invoice");
        insert_draft(&pool, &org, None, 1_000, 2_000, "usd").await.expect("one-off invoice, no subscription");

        let developer_only = list_for_org(&pool, &org, Some("developer"), 10, 0).await.expect("list developer");
        assert_eq!(developer_only.len(), 1);
        assert_eq!(developer_only[0].subscription_id, Some(dev_sub));

        let everything = list_for_org(&pool, &org, None, 10, 0).await.expect("list all");
        assert_eq!(everything.len(), 3, "no storefront filter must include the one-off invoice too");
    }

    #[tokio::test]
    #[ignore]
    async fn a_zero_total_invoice_finalizes_straight_to_paid() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = format!("test-org-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());

        let invoice_id = insert_draft(&pool, &org, None, 1_000, 2_000, "usd").await.expect("insert_draft");
        finalize(&pool, invoice_id, 0).await.expect("finalize");

        let invoice = find(&pool, invoice_id).await.expect("find").expect("row exists");
        assert_eq!(invoice.status, "paid");
    }
}

use std::collections::HashSet;

pub async fn subscriptions_with_paid_invoice(pool: &MySqlPool, subscription_ids: &[u64]) -> Result<HashSet<u64>> {
    // Returns the set of subscription IDs that have at least one paid invoice.
    todo!()
}
