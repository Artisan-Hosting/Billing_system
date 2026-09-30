//! Period rollover: what happens when a subscription's paid period runs out.
//!
//! Until this job existed nothing ever looked at `current_period_end`, so a
//! queued downgrade or cancellation was recorded and then ignored forever.
//! One pass ([`run_once`]) does three things:
//!
//! 1. **Cancellations end.** A due subscription with `cancel_at_period_end`
//!    becomes `canceled`, and any invoice it still owes is voided (along with
//!    its PaymentIntent, best effort -- a stale checkout tab must not be able
//!    to pay a void invoice and revive the subscription through the webhook).
//! 2. **Active subscriptions renew.** A due `active` subscription takes its
//!    queued plan (if any), advances one period, and gets an `open` invoice
//!    for the new period's base price; it goes `past_due` until that invoice
//!    is paid (the webhook flips it back). A free plan's invoice is born
//!    `paid` and the subscription stays `active`.
//! 3. **Unpaid invoices get something to pay.** Invoices left `open` without
//!    a PaymentIntent (the job creates them, or a purchase crashed mid-way)
//!    get one, via the same idempotent helper `RetryInvoicePayment` uses.
//!
//! A subscription that is already `past_due`/`grace_period`/`suspended` is
//! **not** renewed: its last invoice is still unpaid, and stacking another
//! one on top would only grow a debt. The suspend/grace/delete lifecycle
//! (not built yet) is what acts on those.
//!
//! # Idempotency
//!
//! Each subscription is handled in one transaction that re-reads the row
//! under `FOR UPDATE` and re-checks that it is still due, so two instances,
//! or a run repeated after a crash, cannot advance a period twice or write
//! two invoices for it. The PaymentIntent work happens *after* commit, so a
//! failure in Stripe can never roll back a renewal; the next pass retries it.
//!
//! # Not covered
//!
//! Overage for the ended period. `RecordOverageUsage` prices base + overage
//! for a period it is handed, so whatever calls it must now send overage
//! only for periods this job already billed the base price for.

use sqlx::{MySqlPool, Row};

use crate::db::{invoices as inv_db, plans as plans_db};
use crate::domain::BillingStatus;
use crate::error::{Error, Result};
use crate::grpc::admin_service::ensure_invoice_payment_intent;
use crate::grpc::service::Billing;
use artisan_middleware::dusa_collection_utils::core::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::log;

/// Same flat period `CreateOrUpgradeSubscription` starts a subscription with.
pub const PERIOD_SECONDS: i64 = 30 * 24 * 60 * 60;

/// How often the job wakes up.
pub const TICK_SECONDS: u64 = 300;

/// A safety stop for a subscription that somehow stays due (a free plan left
/// unvisited for years): at most this many periods are advanced per pass.
const MAX_PERIODS_PER_PASS: u32 = 36;

/// An invoice this young may still be mid-creation by a purchase request, so
/// the sweep leaves it alone rather than racing that request for the Stripe
/// call.
const SWEEP_MIN_AGE_SECONDS: i64 = 120;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub renewed: u32,
    pub canceled: u32,
    pub payment_intents_created: u32,
}

/// What one subscription's pass did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    NotDue,
    Renewed { periods: u32 },
    Canceled { voided_payment_intents: Vec<String> },
}

pub async fn run_once(billing: &Billing, now: i64) -> Result<Report> {
    let mut report = Report::default();

    let due: Vec<u64> = sqlx::query_scalar(
        "SELECT id FROM subscriptions WHERE current_period_end <= FROM_UNIXTIME(?) \
         AND status NOT IN ('deleted', 'canceled') ORDER BY id LIMIT 500",
    )
    .bind(now)
    .fetch_all(&billing.pool)
    .await?;

    for id in due {
        match roll_subscription(&billing.pool, id, now).await {
            Ok(Outcome::NotDue) => {}
            Ok(Outcome::Renewed { periods }) => {
                report.renewed += periods;
                log!(LogLevel::Info, "subscription {id} renewed ({periods} period(s))");
            }
            Ok(Outcome::Canceled { voided_payment_intents }) => {
                report.canceled += 1;
                log!(LogLevel::Info, "subscription {id} ended its cancellation");
                for pi in voided_payment_intents {
                    // Best effort: the invoice is already void, this only
                    // closes the checkout. An already-final PaymentIntent
                    // refusing to cancel is expected.
                    if let Err(e) = billing.stripe.cancel_payment_intent(&pi).await {
                        log!(LogLevel::Warn, "could not cancel {pi} for the void invoice: {e}");
                    }
                }
            }
            // One bad row must not stop the others from renewing.
            Err(e) => log!(LogLevel::Error, "rollover of subscription {id} failed: {e}"),
        }
    }

    let waiting: Vec<u64> = sqlx::query_scalar(
        "SELECT id FROM invoices WHERE status = 'open' AND total_cents > 0 AND stripe_payment_intent_id IS NULL \
         AND created_at <= FROM_UNIXTIME(?) ORDER BY id LIMIT 500",
    )
    .bind(now - SWEEP_MIN_AGE_SECONDS)
    .fetch_all(&billing.pool)
    .await?;

    for invoice_id in waiting {
        let Some(invoice) = inv_db::find(&billing.pool, invoice_id).await? else { continue };
        match ensure_invoice_payment_intent(billing, &invoice).await {
            Ok(_) => report.payment_intents_created += 1,
            Err(e) => log!(LogLevel::Error, "could not create a PaymentIntent for invoice {invoice_id}: {e}"),
        }
    }

    Ok(report)
}

/// Advances or ends one subscription, atomically. Pure database: no Stripe.
async fn roll_subscription(pool: &MySqlPool, id: u64, now: i64) -> Result<Outcome> {
    let mut tx = pool.begin().await?;
    let mut periods = 0u32;

    loop {
        let row = sqlx::query(
            "SELECT organization_id, plan_code, status, pending_plan_code, cancel_at_period_end, \
             UNIX_TIMESTAMP(current_period_end) AS period_end \
             FROM subscriptions WHERE id = ? FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("subscription {id}")))?;

        let organization_id: String = row.get("organization_id");
        let plan_code: String = row.get("plan_code");
        let status: String = row.get("status");
        let pending: Option<String> = row.get("pending_plan_code");
        let cancel: bool = row.get("cancel_at_period_end");
        let period_end: i64 = row.get("period_end");

        let finished = status == BillingStatus::Deleted.as_str_name() || status == BillingStatus::Canceled.as_str_name();
        if period_end > now || finished {
            tx.commit().await?;
            return Ok(if periods > 0 { Outcome::Renewed { periods } } else { Outcome::NotDue });
        }

        if cancel {
            let voided: Vec<Option<String>> = sqlx::query_scalar(
                "SELECT stripe_payment_intent_id FROM invoices WHERE subscription_id = ? AND status = 'open'",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            sqlx::query("UPDATE invoices SET status = 'void' WHERE subscription_id = ? AND status = 'open'")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE subscriptions SET status = ?, pending_plan_code = NULL WHERE id = ?")
                .bind(BillingStatus::Canceled.as_str_name())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Outcome::Canceled { voided_payment_intents: voided.into_iter().flatten().collect() });
        }

        if status != BillingStatus::Active.as_str_name() || periods >= MAX_PERIODS_PER_PASS {
            // Unpaid (or, past the safety stop, caught up enough): leave it.
            tx.commit().await?;
            return Ok(if periods > 0 { Outcome::Renewed { periods } } else { Outcome::NotDue });
        }

        let next_plan_code = pending.unwrap_or(plan_code);
        let plan = plans_db::find(pool, &next_plan_code)
            .await?
            .ok_or_else(|| Error::NotFound(format!("plan {next_plan_code}")))?;
        let new_start = period_end;
        let new_end = period_end + PERIOD_SECONDS;
        let owes = plan.price_cents > 0;

        sqlx::query(
            "UPDATE subscriptions SET plan_code = ?, pending_plan_code = NULL, status = ?, \
             current_period_start = FROM_UNIXTIME(?), current_period_end = FROM_UNIXTIME(?) WHERE id = ?",
        )
        .bind(&next_plan_code)
        .bind(if owes { BillingStatus::PastDue.as_str_name() } else { BillingStatus::Active.as_str_name() })
        .bind(new_start)
        .bind(new_end)
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let invoice_id = sqlx::query(
            "INSERT INTO invoices (organization_id, subscription_id, period_start, period_end, status, total_cents, currency) \
             VALUES (?, ?, FROM_UNIXTIME(?), FROM_UNIXTIME(?), ?, ?, ?)",
        )
        .bind(&organization_id)
        .bind(id)
        .bind(new_start)
        .bind(new_end)
        .bind(if owes { "open" } else { "paid" })
        .bind(plan.price_cents)
        .bind(&plan.currency)
        .execute(&mut *tx)
        .await?
        .last_insert_id();

        sqlx::query(
            "INSERT INTO invoice_line_items (invoice_id, unit_code, description, quantity, unit_price_cents, amount_cents) \
             VALUES (?, NULL, ?, 1, ?, ?)",
        )
        .bind(invoice_id)
        .bind(&plan.display_name)
        .bind(plan.price_cents as f64)
        .bind(plan.price_cents)
        .execute(&mut *tx)
        .await?;

        periods += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::subscriptions as sub_db;

    async fn pool() -> MySqlPool {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        crate::db::connect(&database_url).await.expect("connect")
    }

    fn unique_org() -> String {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!("rollover-{nonce}")
    }

    const NOW: i64 = 2_000_000_000;

    /// A subscription whose period ended a day before `NOW`.
    async fn due_subscription(pool: &MySqlPool, org: &str, plan: &str, status: &str) -> u64 {
        sub_db::insert(pool, org, "developer", plan, status, NOW - PERIOD_SECONDS - 86_400, NOW - 86_400)
            .await
            .expect("insert subscription")
    }

    async fn invoices_of(pool: &MySqlPool, sub: u64) -> Vec<(String, i64)> {
        sqlx::query("SELECT status, total_cents FROM invoices WHERE subscription_id = ? ORDER BY id")
            .bind(sub)
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.get("status"), r.get("total_cents")))
            .collect()
    }

    #[tokio::test]
    #[ignore]
    async fn a_due_subscription_applies_its_queued_downgrade_and_renews_exactly_once() {
        let pool = pool().await;
        let id = due_subscription(&pool, &unique_org(), "dev_pro", "active").await;
        sub_db::schedule_downgrade(&pool, id, "dev_builder").await.unwrap();

        assert_eq!(roll_subscription(&pool, id, NOW).await.unwrap(), Outcome::Renewed { periods: 1 });

        let sub = sub_db::find_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(sub.plan_code, "dev_builder");
        assert_eq!(sub.pending_plan_code, None);
        assert_eq!(sub.status, "past_due", "owes the new period until it is paid");
        assert_eq!(sub.current_period_start, NOW - 86_400);
        assert_eq!(sub.current_period_end, NOW - 86_400 + PERIOD_SECONDS);
        assert_eq!(invoices_of(&pool, id).await, vec![("open".to_owned(), 800)]);

        // A second pass -- a crash and restart, or another instance -- finds
        // nothing due and writes nothing.
        assert_eq!(roll_subscription(&pool, id, NOW).await.unwrap(), Outcome::NotDue);
        assert_eq!(invoices_of(&pool, id).await.len(), 1);
    }

    #[tokio::test]
    #[ignore]
    async fn a_free_plan_renews_straight_to_paid_and_stays_active() {
        let pool = pool().await;
        let id = due_subscription(&pool, &unique_org(), "dev_beta", "active").await;

        assert_eq!(roll_subscription(&pool, id, NOW).await.unwrap(), Outcome::Renewed { periods: 1 });
        let sub = sub_db::find_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(sub.status, "active");
        assert_eq!(invoices_of(&pool, id).await, vec![("paid".to_owned(), 0)]);
    }

    #[tokio::test]
    #[ignore]
    async fn a_cancellation_ends_the_subscription_and_voids_what_it_still_owed() {
        let pool = pool().await;
        let id = due_subscription(&pool, &unique_org(), "dev_pro", "past_due").await;
        sub_db::set_cancel_at_period_end(&pool, id, true).await.unwrap();
        let invoice = inv_db::insert_draft(&pool, &unique_org(), Some(id), NOW - 100_000, NOW - 86_400, "usd").await.unwrap();
        inv_db::finalize(&pool, invoice, 3200).await.unwrap();

        let outcome = roll_subscription(&pool, id, NOW).await.unwrap();
        assert_eq!(outcome, Outcome::Canceled { voided_payment_intents: vec![] });

        assert_eq!(sub_db::find_by_id(&pool, id).await.unwrap().unwrap().status, "canceled");
        assert_eq!(invoices_of(&pool, id).await, vec![("void".to_owned(), 3200)]);
        assert_eq!(roll_subscription(&pool, id, NOW).await.unwrap(), Outcome::NotDue, "canceled is terminal here");
    }

    #[tokio::test]
    #[ignore]
    async fn an_unpaid_subscription_is_not_renewed_and_a_current_one_is_left_alone() {
        let pool = pool().await;
        let unpaid = due_subscription(&pool, &unique_org(), "dev_pro", "past_due").await;
        assert_eq!(roll_subscription(&pool, unpaid, NOW).await.unwrap(), Outcome::NotDue);
        assert!(invoices_of(&pool, unpaid).await.is_empty(), "no second invoice on top of an unpaid one");

        let current = sub_db::insert(&pool, &unique_org(), "developer", "dev_pro", "active", NOW - 10, NOW + 10_000).await.unwrap();
        assert_eq!(roll_subscription(&pool, current, NOW).await.unwrap(), Outcome::NotDue);
        assert!(invoices_of(&pool, current).await.is_empty());
    }
}
