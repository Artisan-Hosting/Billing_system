//! Queries behind the `plans` / `plan_allowances` / `plan_overage_rates`
//! catalog (see `migrations/0002_plans.sql`).

use sqlx::{MySqlPool, Row};
use std::collections::HashMap;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct PlanAllowancesAndRates {
    /// unit_code -> included_qty
    pub allowances: HashMap<String, f64>,
    /// unit_code -> rate_cents_per_unit. A unit absent here has no overage
    /// rate at all (see `overage::calculate_pool_overage`'s doc comment).
    pub rates: HashMap<String, f64>,
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub plan_code: String,
    pub storefront: String,
    pub display_name: String,
    pub price_cents: i64,
    pub currency: String,
    pub active: bool,
}

pub async fn find(pool: &MySqlPool, plan_code: &str) -> Result<Option<PlanRow>> {
    let row = sqlx::query(
        "SELECT plan_code, storefront, display_name, price_cents, currency, active FROM plans WHERE plan_code = ?",
    )
    .bind(plan_code)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| PlanRow {
        plan_code: row.get("plan_code"),
        storefront: row.get("storefront"),
        display_name: row.get("display_name"),
        price_cents: row.get("price_cents"),
        currency: row.get("currency"),
        active: row.get("active"),
    }))
}

/// Every plan still on sale, cheapest first within a storefront.
pub async fn list_active(pool: &MySqlPool, storefront: Option<&str>) -> Result<Vec<PlanRow>> {
    let rows = sqlx::query(
        "SELECT plan_code, storefront, display_name, price_cents, currency, active FROM plans \
         WHERE active = TRUE AND (? IS NULL OR storefront = ?) ORDER BY storefront, price_cents, plan_code",
    )
    .bind(storefront)
    .bind(storefront)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| PlanRow {
            plan_code: row.get("plan_code"),
            storefront: row.get("storefront"),
            display_name: row.get("display_name"),
            price_cents: row.get("price_cents"),
            currency: row.get("currency"),
            active: row.get("active"),
        })
        .collect())
}

/// `Err(Error::NotFound)` if `plan_code` isn't in the catalog at all --
/// distinct from "this plan has no allowances/rates rows," which is a
/// legitimate (if unusual) state that returns an empty map instead.
pub async fn allowances_and_rates_for_plan(pool: &MySqlPool, plan_code: &str) -> Result<PlanAllowancesAndRates> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM plans WHERE plan_code = ?")
        .bind(plan_code)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Err(Error::NotFound(format!("plan {plan_code}")));
    }

    // CAST ... AS DOUBLE: sqlx has no built-in DECIMAL -> f64 decode (it
    // expects `rust_decimal`/`bigdecimal`, neither of which this crate
    // depends on -- these columns are DECIMAL for exact storage, but every
    // consumer here (`overage::calculate_pool_overage`) already works in
    // f64, so the conversion happens once, in SQL, rather than pulling in a
    // decimal crate for a value that's about to become a float anyway.
    let allowance_rows = sqlx::query(
        "SELECT unit_code, CAST(included_qty AS DOUBLE) AS included_qty FROM plan_allowances WHERE plan_code = ?",
    )
    .bind(plan_code)
    .fetch_all(pool)
    .await?;
    let allowances = allowance_rows
        .into_iter()
        .map(|row| (row.get::<String, _>("unit_code"), row.get::<f64, _>("included_qty")))
        .collect();

    let rate_rows = sqlx::query(
        "SELECT unit_code, CAST(rate_cents_per_unit AS DOUBLE) AS rate_cents_per_unit \
         FROM plan_overage_rates WHERE plan_code = ?",
    )
    .bind(plan_code)
    .fetch_all(pool)
    .await?;
    let rates = rate_rows
        .into_iter()
        .map(|row| (row.get::<String, _>("unit_code"), row.get::<f64, _>("rate_cents_per_unit")))
        .collect();

    Ok(PlanAllowancesAndRates { allowances, rates })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a real database with migrations applied (`DATABASE_URL`),
    /// matching how `stripe::tests` gates its own live-network tests --
    /// `cargo test -- --ignored` with a real `DATABASE_URL` set, not part of
    /// the default `cargo test` run.
    #[tokio::test]
    #[ignore]
    async fn fetches_the_seeded_pro_plan_allowances_and_rates() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");

        let catalog = allowances_and_rates_for_plan(&pool, "dev_pro").await.expect("dev_pro is seeded");
        assert_eq!(catalog.allowances.get("ram_gb_month"), Some(&2.0));
        assert_eq!(catalog.allowances.get("vcpu_month"), Some(&1.0));
        assert_eq!(catalog.rates.get("ram_gb_month"), Some(&1000.0));
        assert_eq!(catalog.rates.get("vcpu_month"), Some(&1300.0));
    }

    #[tokio::test]
    #[ignore]
    async fn find_returns_the_seeded_pro_plan_row() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");

        let plan = find(&pool, "dev_pro").await.expect("query").expect("dev_pro is seeded");
        assert_eq!(plan.storefront, "developer");
        assert_eq!(plan.price_cents, 3200);
        assert!(plan.active);

        assert!(find(&pool, "no_such_plan").await.expect("query").is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn an_unknown_plan_code_is_not_found() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");

        let err = allowances_and_rates_for_plan(&pool, "no_such_plan").await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
