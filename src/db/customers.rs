//! Queries behind `billing_customers` (`migrations/0010_billing_customers.sql`).

use sqlx::{MySqlPool, Row};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct CustomerRow {
    pub organization_id: String,
    pub stripe_customer_id: String,
    pub default_payment_method_id: Option<String>,
}

pub async fn find(pool: &MySqlPool, organization_id: &str) -> Result<Option<CustomerRow>> {
    let row = sqlx::query(
        "SELECT organization_id, stripe_customer_id, default_payment_method_id FROM billing_customers WHERE organization_id = ?",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| CustomerRow {
        organization_id: r.get("organization_id"),
        stripe_customer_id: r.get("stripe_customer_id"),
        default_payment_method_id: r.get("default_payment_method_id"),
    }))
}

/// Records the Customer created for an org. `ON DUPLICATE KEY UPDATE ... = ...`
/// as a no-op so two concurrent first-touches both succeed; the caller
/// re-reads with [`find`] and uses whichever row won.
pub async fn insert(pool: &MySqlPool, organization_id: &str, stripe_customer_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO billing_customers (organization_id, stripe_customer_id) VALUES (?, ?) \
         ON DUPLICATE KEY UPDATE organization_id = organization_id",
    )
    .bind(organization_id)
    .bind(stripe_customer_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remembers the PaymentMethod a customer saved, unless one is already set --
/// the first saved card becomes the default and a later top-up with a
/// different card doesn't silently swap it. Returns whether it was set.
pub async fn set_default_payment_method_if_none(
    pool: &MySqlPool,
    stripe_customer_id: &str,
    payment_method_id: &str,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE billing_customers SET default_payment_method_id = ? \
         WHERE stripe_customer_id = ? AND default_payment_method_id IS NULL",
    )
    .bind(payment_method_id)
    .bind(stripe_customer_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(prefix: &str) -> String {
        format!("{prefix}_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }

    #[tokio::test]
    #[ignore]
    async fn insert_find_and_default_payment_method_round_trip() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let (org, cus) = (unique("org"), unique("cus"));

        assert!(find(&pool, &org).await.unwrap().is_none());
        insert(&pool, &org, &cus).await.unwrap();
        // A concurrent duplicate insert is a no-op, not an error.
        insert(&pool, &org, &cus).await.unwrap();

        let row = find(&pool, &org).await.unwrap().unwrap();
        assert_eq!(row.stripe_customer_id, cus);
        assert_eq!(row.default_payment_method_id, None);

        assert!(set_default_payment_method_if_none(&pool, &cus, "pm_first").await.unwrap());
        assert!(!set_default_payment_method_if_none(&pool, &cus, "pm_second").await.unwrap(), "first card stays the default");
        assert_eq!(find(&pool, &org).await.unwrap().unwrap().default_payment_method_id.as_deref(), Some("pm_first"));
    }
}
