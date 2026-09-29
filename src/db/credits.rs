//! Queries behind `credit_accounts`/`credit_ledger_entries` (see
//! `migrations/0006_credit_ledger.sql`, `0007_credit_ledger_idempotency.sql`).
//!
//! `credit_accounts.balance_cents` is a materialized column kept in sync
//! with the append-only `credit_ledger_entries` inside the same transaction
//! as every write -- see [`apply_ledger_entry`]'s own doc comment.

use sqlx::{MySqlPool, Row};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct CreditAccountRow {
    pub organization_id: String,
    pub balance_cents: i64,
    pub monthly_spend_cap_cents: Option<i64>,
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
        "SELECT organization_id, balance_cents, monthly_spend_cap_cents, \
         UNIX_TIMESTAMP(updated_at) AS updated_at FROM credit_accounts WHERE organization_id = ?",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    Ok(CreditAccountRow {
        organization_id: row.get("organization_id"),
        balance_cents: row.get("balance_cents"),
        monthly_spend_cap_cents: row.get("monthly_spend_cap_cents"),
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
/// applying the entry twice. Returns `(new_balance_cents, applied)`, where
/// `applied` is `false` on a detected retry -- callers that need to know
/// "did this debit actually happen" (vs. "we already knew about it") can
/// tell the two apart.
pub async fn apply_ledger_entry(
    pool: &MySqlPool,
    organization_id: &str,
    entry_type: &str,
    amount_cents: i64,
    external_reference: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<(i64, bool)> {
    let mut tx = pool.begin().await?;

    sqlx::query("INSERT INTO credit_accounts (organization_id) VALUES (?) ON DUPLICATE KEY UPDATE organization_id = organization_id")
        .bind(organization_id)
        .execute(&mut *tx)
        .await?;

    if let Some(key) = idempotency_key {
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT balance_after_cents FROM credit_ledger_entries WHERE organization_id = ? AND idempotency_key = ?",
        )
        .bind(organization_id)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(balance_after_cents) = existing {
            tx.commit().await?;
            return Ok((balance_after_cents, false));
        }
    }

    sqlx::query("UPDATE credit_accounts SET balance_cents = balance_cents + ? WHERE organization_id = ?")
        .bind(amount_cents)
        .bind(organization_id)
        .execute(&mut *tx)
        .await?;

    let new_balance: i64 = sqlx::query_scalar("SELECT balance_cents FROM credit_accounts WHERE organization_id = ?")
        .bind(organization_id)
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO credit_ledger_entries \
         (organization_id, entry_type, amount_cents, external_reference, idempotency_key, balance_after_cents) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(organization_id)
    .bind(entry_type)
    .bind(amount_cents)
    .bind(external_reference)
    .bind(idempotency_key)
    .bind(new_balance)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((new_balance, true))
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
        assert_eq!(account.balance_cents, 0);
        assert_eq!(account.monthly_spend_cap_cents, None);
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
        assert_eq!(account.balance_cents, 1500, "the materialized column must match the ledger's running total");
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
        assert_eq!(account.balance_cents, 4900);
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
