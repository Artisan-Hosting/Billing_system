//! Pool-overage calculation: what a subscription owes above its plan's
//! included allowance, for one billing period.
//!
//! Pure and network/database-free, the same reasoning
//! `domain_management::purchasing::price_for` is pure -- the money math is
//! unit-testable without a live database, plan catalog, or aggregator run.
//! Replaces the old `usage::calculate_costs`, which priced CPU/RAM/bandwidth
//! from hardcoded constants with no notion of a plan; this reads a plan's
//! allowance and overage rate instead, both DB rows now (see
//! `migrations/0002_plans.sql`).

use serde::Serialize;
use std::collections::HashMap;

/// A subscription's usage for one billing period, already reduced to
/// per-unit averages/totals by the caller (typically from
/// `artisan_middleware::aggregator::BilledUsageSummary`, summed across every
/// instance in the pool). Billed on the **monthly average**, never peak --
/// a peak RAM spike is enforced as a hard cap elsewhere (`max_ram_usage`,
/// see the Price Book's billing rules) and never appears here.
#[derive(Debug, Clone, Default)]
pub struct PoolUsage {
    /// Average GB of RAM in use over the period.
    pub ram_gb_avg: f64,
    /// Average vCPU in use over the period (core-hours consumed / hours in
    /// the period -- see `artisan_middleware::aggregator`'s corrected
    /// core-seconds accrual for where the core-hours figure comes from).
    pub vcpu_avg: f64,
    /// Total GB of egress over the period (metered by total, not average --
    /// unlike RAM/vCPU, egress has no "peak" to distinguish from).
    pub egress_gb_total: f64,
    /// Total Apostle-sent emails over the period, in thousands (matches the
    /// `email_1k` unit).
    pub email_1k_total: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverageLineItem {
    pub unit_code: &'static str,
    /// Usage above the plan's included allowance for this unit. Always > 0
    /// -- a unit at or under its allowance produces no line item at all.
    pub over_qty: f64,
    pub rate_cents_per_unit: f64,
    /// Rounded to the nearest cent -- an invoice line item is money, and
    /// `invoice_line_items.amount_cents` is a `BIGINT`, not a float.
    pub amount_cents: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OverageCharges {
    pub line_items: Vec<OverageLineItem>,
    pub total_cents: i64,
}

/// `allowances`/`rates` are keyed by unit_code (`ram_gb_month`, `vcpu_month`,
/// `egress_gb`, `email_1k`), matching `plan_allowances`/`plan_overage_rates`
/// exactly -- the caller fetches a subscription's plan's rows and passes
/// them in unchanged. A unit missing from `rates` is never charged for
/// (Care plans have allowance rows for capacity reference but no overage
/// rate at all -- see `migrations/0003_plan_catalog_seed.sql`'s comment on
/// why).
pub fn calculate_pool_overage(
    usage: &PoolUsage,
    allowances: &HashMap<String, f64>,
    rates: &HashMap<String, f64>,
) -> OverageCharges {
    let mut line_items = Vec::new();

    let quantities: [(&'static str, f64); 4] = [
        ("ram_gb_month", usage.ram_gb_avg),
        ("vcpu_month", usage.vcpu_avg),
        ("egress_gb", usage.egress_gb_total),
        ("email_1k", usage.email_1k_total),
    ];

    for (unit_code, used) in quantities {
        let Some(&rate_cents_per_unit) = rates.get(unit_code) else {
            continue;
        };
        let included = allowances.get(unit_code).copied().unwrap_or(0.0);
        let over_qty = used - included;
        if over_qty <= 0.0 {
            continue;
        }

        let amount_cents = (over_qty * rate_cents_per_unit).round() as i64;
        line_items.push(OverageLineItem { unit_code, over_qty, rate_cents_per_unit, amount_cents });
    }

    let total_cents = line_items.iter().map(|item| item.amount_cents).sum();
    OverageCharges { line_items, total_cents }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowances(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn rates(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// The Price Book's Pro plan: 2 GB / 1 vCPU included, $10/GB and
    /// $13/vCPU overage (olympus-host-adjusted rates, confirmed 2026-09-27).
    fn pro_plan() -> (HashMap<String, f64>, HashMap<String, f64>) {
        (
            allowances(&[("ram_gb_month", 2.0), ("vcpu_month", 1.0), ("egress_gb", 50.0), ("email_1k", 5.0)]),
            rates(&[("ram_gb_month", 1000.0), ("vcpu_month", 1300.0), ("egress_gb", 5.0)]),
        )
    }

    #[test]
    fn usage_at_or_under_every_allowance_produces_no_charge() {
        let (allow, rate) = pro_plan();
        let usage = PoolUsage { ram_gb_avg: 2.0, vcpu_avg: 1.0, egress_gb_total: 50.0, email_1k_total: 5.0 };
        let charges = calculate_pool_overage(&usage, &allow, &rate);
        assert!(charges.line_items.is_empty());
        assert_eq!(charges.total_cents, 0);
    }

    #[test]
    fn ram_and_vcpu_overage_are_billed_on_the_average_not_peak() {
        // Callers are expected to pass a period AVERAGE for ram/vcpu -- this
        // test's inputs stand in for "the account averaged 3 GB and 1.5 vCPU
        // over the month," regardless of any peak spike within it.
        let (allow, rate) = pro_plan();
        let usage = PoolUsage { ram_gb_avg: 3.0, vcpu_avg: 1.5, egress_gb_total: 50.0, email_1k_total: 5.0 };
        let charges = calculate_pool_overage(&usage, &allow, &rate);

        assert_eq!(charges.line_items.len(), 2);
        let ram = charges.line_items.iter().find(|i| i.unit_code == "ram_gb_month").unwrap();
        assert_eq!(ram.over_qty, 1.0);
        assert_eq!(ram.amount_cents, 1000); // 1 GB over * $10.00/GB

        let vcpu = charges.line_items.iter().find(|i| i.unit_code == "vcpu_month").unwrap();
        assert_eq!(vcpu.over_qty, 0.5);
        assert_eq!(vcpu.amount_cents, 650); // 0.5 vCPU over * $13.00/vCPU

        assert_eq!(charges.total_cents, 1650);
    }

    #[test]
    fn egress_overage_is_billed_on_the_period_total_not_an_average() {
        let (allow, rate) = pro_plan();
        let usage = PoolUsage { ram_gb_avg: 2.0, vcpu_avg: 1.0, egress_gb_total: 60.0, email_1k_total: 5.0 };
        let charges = calculate_pool_overage(&usage, &allow, &rate);

        assert_eq!(charges.line_items.len(), 1);
        assert_eq!(charges.line_items[0].unit_code, "egress_gb");
        assert_eq!(charges.line_items[0].over_qty, 10.0);
        assert_eq!(charges.line_items[0].amount_cents, 50); // 10 GB over * $0.05/GB
    }

    #[test]
    fn a_unit_with_no_overage_rate_is_never_charged_even_if_over_its_allowance() {
        // Care plans: an allowance row exists (capacity reference), but no
        // overage rate -- see migrations/0003_plan_catalog_seed.sql.
        let allow = allowances(&[("ram_gb_month", 1.0), ("vcpu_month", 1.0)]);
        let rate = rates(&[]);
        let usage = PoolUsage { ram_gb_avg: 5.0, vcpu_avg: 5.0, egress_gb_total: 0.0, email_1k_total: 0.0 };

        let charges = calculate_pool_overage(&usage, &allow, &rate);
        assert!(charges.line_items.is_empty());
        assert_eq!(charges.total_cents, 0);
    }

    #[test]
    fn a_unit_missing_an_allowance_row_is_billed_from_zero() {
        // Defensive: an allowance row should always exist for a metered
        // unit, but if one is ever missing, the safe assumption is "nothing
        // included" rather than silently treating it as unlimited.
        let allow = allowances(&[("ram_gb_month", 2.0)]); // no vcpu_month row
        let rate = rates(&[("vcpu_month", 1300.0)]);
        let usage = PoolUsage { ram_gb_avg: 2.0, vcpu_avg: 0.5, egress_gb_total: 0.0, email_1k_total: 0.0 };

        let charges = calculate_pool_overage(&usage, &allow, &rate);
        assert_eq!(charges.line_items.len(), 1);
        assert_eq!(charges.line_items[0].over_qty, 0.5);
        assert_eq!(charges.line_items[0].amount_cents, 650);
    }

    #[test]
    fn rounds_the_charge_to_the_nearest_cent() {
        let allow = allowances(&[("ram_gb_month", 0.0)]);
        let rate = rates(&[("ram_gb_month", 333.333)]); // $3.33333/GB
        let usage = PoolUsage { ram_gb_avg: 1.0, vcpu_avg: 0.0, egress_gb_total: 0.0, email_1k_total: 0.0 };

        let charges = calculate_pool_overage(&usage, &allow, &rate);
        assert_eq!(charges.line_items[0].amount_cents, 333); // 333.333 rounds down
    }
}
