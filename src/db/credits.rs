//! Queries behind `credit_accounts`/`credit_ledger_entries` (see
//! `migrations/0006_credit_ledger.sql`, `0007_credit_ledger_idempotency.sql`).
//!
//! `credit_accounts.balance_micros` (micro-dollars, see [`crate::credit`]) is a materialized column kept in sync
//! with the append-only `credit_ledger_entries` inside the same transaction
//! as every write -- see [`apply_ledger_entry`]'s own doc comment.

use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct CreditAccountRow {
    pub organization_id: String,
    pub balance_micros: i64,
    pub monthly_spend_cap_micros: Option<i64>,
    pub updated_at: i64,
}

/// Every organization has an implicit $0 balance whether or not a
/// `credit_accounts` row exists yet for it -- this creates one on first
/// touch (`INSERT ... ON DUPLICATE KEY UPDATE id = id`, the same no-op-update
/// idiom `db::payment_intents::insert` uses to make a concurrent first-touch
/// race safe) rather than requiring every caller to handle "no row yet."
pub async fn get_or_create(pool: &MySqlPool, organization_id: &str) -> Result<CreditAccountRow> {
    sqlx::query("INSERT INTO credit_accounts (organization_id) VALUES (?) ON DUPLICATE KEY UPDATE organization_id = organization_id")
        .bind(organization_id)
        .execute(pool)
        .await?;

    let row = sqlx::query(
        "SELECT organization_id, balance_micros, monthly_spend_cap_micros, \
         UNIX_TIMESTAMP(updated_at) AS updated_at FROM credit_accounts WHERE organization_id = ?",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    Ok(CreditAccountRow {
        organization_id: row.get("organization_id"),
        balance_micros: row.get("balance_micros"),
        monthly_spend_cap_micros: row.get("monthly_spend_cap_micros"),
        updated_at: row.get("updated_at"),
    })
}

/// Applies one ledger entry and updates the materialized balance, in one
/// transaction -- the two must never observably disagree, since the balance
/// column exists purely as a fast-path cache of "sum the ledger."
///
/// `idempotency_key`, when given, makes a retried call land once: a second
/// call with the same `(organization_id, idempotency_key)` is detected via
/// the table's unique key and returns the balance *unchanged* rather than
/// applying the entry twice. Returns `(new_balance_micros, applied)`, where
/// `applied` is `false` on a detected retry -- callers that need to know
/// "did this debit actually happen" (vs. "we already knew about it") can
/// tell the two apart.
pub async fn apply_ledger_entry(
    pool: &MySqlPool,
    organization_id: &str,
    entry_type: &str,
    amount_micros: i64,
    external_reference: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<(i64, bool)> {
    let mut tx = pool.begin().await?;
    let result =
        apply_ledger_entry_in(&mut tx, organization_id, entry_type, amount_micros, external_reference, idempotency_key).await?;
    tx.commit().await?;
    Ok(result)
}

/// [`apply_ledger_entry`]'s body, for a caller that already holds the
/// transaction (so it can do other reads/writes atomically with the entry).
pub async fn apply_ledger_entry_in(
    tx: &mut Transaction<'_, MySql>,
    organization_id: &str,
    entry_type: &str,
    amount_micros: i64,
    external_reference: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<(i64, bool)> {
    sqlx::query("INSERT INTO credit_accounts (organization_id) VALUES (?) ON DUPLICATE KEY UPDATE organization_id = organization_id")
        .bind(organization_id)
        .execute(&mut **tx)
        .await?;

    if let Some(key) = idempotency_key {
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT balance_after_micros FROM credit_ledger_entries WHERE organization_id = ? AND idempotency_key = ?",
        )
        .bind(organization_id)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(balance_after_micros) = existing {
            return Ok((balance_after_micros, false));
        }
    }

    sqlx::query("UPDATE credit_accounts SET balance_micros = balance_micros + ? WHERE organization_id = ?")
        .bind(amount_micros)
        .bind(organization_id)
        .execute(&mut **tx)
        .await?;

    let new_balance: i64 = sqlx::query_scalar("SELECT balance_micros FROM credit_accounts WHERE organization_id = ?")
        .bind(organization_id)
        .fetch_one(&mut **tx)
        .await?;

    sqlx::query(
        "INSERT INTO credit_ledger_entries \
         (organization_id, entry_type, amount_micros, external_reference, idempotency_key, balance_after_micros) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(organization_id)
    .bind(entry_type)
    .bind(amount_micros)
    .bind(external_reference)
    .bind(idempotency_key)
    .bind(new_balance)
    .execute(&mut **tx)
    .await?;

    Ok((new_balance, true))
}

/// The top-up entry a Stripe PaymentIntent funded: `(organization_id,
/// amount_micros)`. `None` means that PaymentIntent never credited anyone
/// (it wasn't a credit top-up, or hasn't succeeded yet) -- refund and dispute
/// events use this to decide whether they concern the credit ledger at all,
/// and whose it is, without trusting metadata on the event.
pub async fn find_topup_for_payment_intent(pool: &MySqlPool, stripe_payment_intent_id: &str) -> Result<Option<(String, i64)>> {
    let row = sqlx::query(
        "SELECT organization_id, amount_micros FROM credit_ledger_entries \
         WHERE entry_type = 'topup' AND external_reference = ? LIMIT 1",
    )
    .bind(stripe_payment_intent_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.get("organization_id"), r.get("amount_micros"))))
}

/// Whether this org already has a ledger entry with this idempotency key.
pub async fn has_entry(pool: &MySqlPool, organization_id: &str, idempotency_key: &str) -> Result<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM credit_ledger_entries WHERE organization_id = ? AND idempotency_key = ?")
            .bind(organization_id)
            .bind(idempotency_key)
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

/// Brings the ledger's total *reversal* for one PaymentIntent up to
/// `cumulative_micros`, writing a single negative `adjustment` for the
/// difference. Stripe reports a charge's refunds as a running total
/// (`amount_refunded`), and may deliver `charge.refunded` more than once or
/// out of order, so this diffs against what earlier `<key_prefix>:` entries
/// already reversed rather than trusting each event as a fresh delta.
/// Returns the reversed amount, or `None` when there was nothing new (a
/// duplicate, or an older total arriving late). The balance may go negative:
/// credit already spent cannot be un-spent.
pub async fn reverse_to_cumulative(
    pool: &MySqlPool,
    organization_id: &str,
    stripe_payment_intent_id: &str,
    key_prefix: &str,
    cumulative_micros: i64,
) -> Result<Option<i64>> {
    let mut tx = pool.begin().await?;

    // Serializes concurrent events for one org so the diff below is exact.
    sqlx::query("INSERT INTO credit_accounts (organization_id) VALUES (?) ON DUPLICATE KEY UPDATE organization_id = organization_id")
        .bind(organization_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT balance_micros FROM credit_accounts WHERE organization_id = ? FOR UPDATE")
        .bind(organization_id)
        .fetch_one(&mut *tx)
        .await?;

    let already: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(-amount_micros), 0) AS SIGNED) FROM credit_ledger_entries \
         WHERE organization_id = ? AND external_reference = ? AND idempotency_key LIKE ?",
    )
    .bind(organization_id)
    .bind(stripe_payment_intent_id)
    .bind(format!("{key_prefix}:%"))
    .fetch_one(&mut *tx)
    .await?;

    let delta = cumulative_micros - already;
    if delta <= 0 {
        tx.commit().await?;
        return Ok(None);
    }

    let key = format!("{key_prefix}:{stripe_payment_intent_id}:{cumulative_micros}");
    let (_, applied) =
        apply_ledger_entry_in(&mut tx, organization_id, "adjustment", -delta, Some(stripe_payment_intent_id), Some(&key)).await?;
    tx.commit().await?;
    Ok(if applied { Some(delta) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_org() -> String {
        format!("test-org-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }

    #[tokio::test]
    #[ignore]
    async fn a_new_org_has_an_implicit_zero_balance() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = unique_org();

        let account = get_or_create(&pool, &org).await.expect("get_or_create");
        assert_eq!(account.balance_micros, 0);
        assert_eq!(account.monthly_spend_cap_micros, None);
    }

    #[tokio::test]
    #[ignore]
    async fn topups_and_debits_accumulate_into_the_materialized_balance() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = unique_org();

        let (balance, applied) = apply_ledger_entry(&pool, &org, "topup", 2500, None, None).await.expect("topup");
        assert_eq!(balance, 2500);
        assert!(applied);

        let (balance, applied) = apply_ledger_entry(&pool, &org, "debit", -1000, Some("session-1"), None).await.expect("debit");
        assert_eq!(balance, 1500);
        assert!(applied);

        let account = get_or_create(&pool, &org).await.expect("get_or_create");
        assert_eq!(account.balance_micros, 1500, "the materialized column must match the ledger's running total");
    }

    #[tokio::test]
    #[ignore]
    async fn a_retried_debit_with_the_same_idempotency_key_applies_once() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = unique_org();

        apply_ledger_entry(&pool, &org, "topup", 5000, None, None).await.expect("topup");

        let (first, applied_first) =
            apply_ledger_entry(&pool, &org, "debit", -100, Some("session-1"), Some("session-1:1")).await.expect("first debit");
        assert_eq!(first, 4900);
        assert!(applied_first);

        // Same idempotency key, simulating a retried gRPC call after a timeout.
        let (retried, applied_retry) =
            apply_ledger_entry(&pool, &org, "debit", -100, Some("session-1"), Some("session-1:1")).await.expect("retried debit");
        assert_eq!(retried, 4900, "must not be debited twice");
        assert!(!applied_retry);

        let account = get_or_create(&pool, &org).await.expect("get_or_create");
        assert_eq!(account.balance_micros, 4900);
    }

    #[tokio::test]
    #[ignore]
    async fn a_debit_may_take_the_balance_negative() {
        // The final partial-second metering tick at shutdown cannot be
        // un-billed -- this function never refuses a debit, it only reports
        // where the balance landed (see the proto's own DebitCredit doc).
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let org = unique_org();

        let (balance, _) = apply_ledger_entry(&pool, &org, "debit", -500, None, None).await.expect("debit");
        assert_eq!(balance, -500);
    }
}
