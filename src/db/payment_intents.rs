//! Queries behind the `payment_intents` table -- Billing's only table.

use sqlx::{MySqlPool, Row};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct PaymentIntentRow {
    pub id: u64,
    pub stripe_payment_intent_id: String,
    pub consumer: String,
    pub external_reference: String,
    pub amount_cents: i64,
    pub currency: String,
    pub status: String,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const SELECT_COLUMNS: &str = "id, stripe_payment_intent_id, consumer, external_reference, amount_cents, \
     currency, status, last_error, UNIX_TIMESTAMP(created_at) AS created_at, \
     UNIX_TIMESTAMP(updated_at) AS updated_at";

pub async fn find_by_consumer_reference(
    pool: &MySqlPool,
    consumer: &str,
    external_reference: &str,
) -> Result<Option<PaymentIntentRow>> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM payment_intents WHERE consumer = ? AND external_reference = ?"
    ))
    .bind(consumer)
    .bind(external_reference)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_entry))
}

pub async fn find_by_stripe_id(pool: &MySqlPool, stripe_payment_intent_id: &str) -> Result<Option<PaymentIntentRow>> {
    let row = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM payment_intents WHERE stripe_payment_intent_id = ?"))
        .bind(stripe_payment_intent_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(row_to_entry))
}

/// Looks a row up by this service's own id, or by
/// `"<consumer>:<external_reference>"` -- whichever the caller has on
/// hand. Split on the *first* `:` only, so a reference that itself
/// contains a colon is still handled correctly.
pub async fn find(pool: &MySqlPool, id_or_reference: &str) -> Result<Option<PaymentIntentRow>> {
    if let Some((consumer, reference)) = id_or_reference.split_once(':') {
        return find_by_consumer_reference(pool, consumer, reference).await;
    }

    let Ok(id) = id_or_reference.parse::<u64>() else {
        return Ok(None);
    };
    let row = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM payment_intents WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(row_to_entry))
}

/// Records a PaymentIntent this service just created with Stripe.
///
/// `ON DUPLICATE KEY UPDATE id = id` (a no-op update) rather than a bare
/// `INSERT`: two concurrent `CreatePaymentIntent` calls for the same
/// `(consumer, external_reference)` both pass the "does this already
/// exist" check before either writes (a real race, not a hypothetical
/// one), and Stripe's own idempotency key -- deterministic from
/// `(consumer, external_reference)`, see `StripeClient::create_payment_intent`'s
/// caller -- means both already got back the *same* Stripe PaymentIntent
/// by the time they race here. Without the no-op clause, the loser would
/// see a raw unique-constraint error instead of the row it was always
/// going to end up agreeing with.
pub async fn insert(
    pool: &MySqlPool,
    stripe_payment_intent_id: &str,
    consumer: &str,
    external_reference: &str,
    amount_cents: i64,
    currency: &str,
    status: &str,
) -> Result<u64> {
    sqlx::query(
        "INSERT INTO payment_intents \
         (stripe_payment_intent_id, consumer, external_reference, amount_cents, currency, status) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE id = id",
    )
    .bind(stripe_payment_intent_id)
    .bind(consumer)
    .bind(external_reference)
    .bind(amount_cents)
    .bind(currency)
    .bind(status)
    .execute(pool)
    .await?;

    let id: u64 = sqlx::query("SELECT id FROM payment_intents WHERE consumer = ? AND external_reference = ?")
        .bind(consumer)
        .bind(external_reference)
        .fetch_one(pool)
        .await?
        .get("id");

    Ok(id)
}

/// Reflects a status Stripe reported (a `GetPaymentIntent` poll, or a
/// verified webhook event) back onto the local row. Returns whether a row
/// was actually found and updated, so a webhook for a PaymentIntent this
/// service didn't create (should never happen, but a webhook endpoint
/// must never assume its own database) can be logged rather than treated
/// as silently fine.
pub async fn update_status(
    pool: &MySqlPool,
    stripe_payment_intent_id: &str,
    status: &str,
    last_error: Option<&str>,
) -> Result<bool> {
    let result =
        sqlx::query("UPDATE payment_intents SET status = ?, last_error = ? WHERE stripe_payment_intent_id = ?")
            .bind(status)
            .bind(last_error)
            .bind(stripe_payment_intent_id)
            .execute(pool)
            .await?;

    Ok(result.rows_affected() > 0)
}

fn row_to_entry(row: sqlx::mysql::MySqlRow) -> PaymentIntentRow {
    PaymentIntentRow {
        id: row.get("id"),
        stripe_payment_intent_id: row.get("stripe_payment_intent_id"),
        consumer: row.get("consumer"),
        external_reference: row.get("external_reference"),
        amount_cents: row.get("amount_cents"),
        currency: row.get("currency"),
        status: row.get("status"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
