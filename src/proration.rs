//! Subscription plan-change money math.
//!
//! Pure, same reasoning `overage::calculate_pool_overage` and
//! `domain_management::purchasing::price_for` are pure -- unit-testable
//! without a database, a plan catalog, or a live Stripe call.

/// Cents to charge immediately for moving from `old_price_cents` to
/// `new_price_cents`, for the time remaining in `[period_start, period_end)`
/// as of `now`.
///
/// A brand new subscription (no prior plan) is just the `old_price_cents =
/// 0` case: with `now == period_start`, the whole period is "remaining," so
/// this returns the plan's full price -- one function covers both "first
/// subscription" and "mid-period upgrade" rather than two.
///
/// Returns 0 for a downgrade or a same-price change (`new_price_cents <=
/// old_price_cents`) -- `CreateOrUpgradeSubscription` rejects those
/// entirely (see its own AUTHZ doc comment: downgrades go through
/// `ScheduleDowngrade` instead, which never charges anything now), so this
/// is a defensive floor, not a path the handler is expected to hit.
pub fn prorate_upgrade_charge(old_price_cents: i64, new_price_cents: i64, now: i64, period_start: i64, period_end: i64) -> i64 {
    if new_price_cents <= old_price_cents {
        return 0;
    }

    let period_len = (period_end - period_start).max(1);
    let remaining = (period_end - now).clamp(0, period_len);
    let price_delta = new_price_cents - old_price_cents;

    ((price_delta as f64) * (remaining as f64) / (period_len as f64)).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn a_brand_new_subscription_is_charged_its_full_price() {
        let period_start = 0;
        let period_end = 30 * DAY;
        // now == period_start: the whole period is "remaining."
        assert_eq!(prorate_upgrade_charge(0, 3200, period_start, period_start, period_end), 3200);
    }

    #[test]
    fn an_upgrade_halfway_through_the_period_charges_half_the_delta() {
        let period_start = 0;
        let period_end = 30 * DAY;
        let now = 15 * DAY;
        // Pro ($32) -> Team ($95): $63 delta, half the period remaining -> $31.50, rounds to $32 (3150 -> wait, compute exactly).
        let charge = prorate_upgrade_charge(3200, 9500, now, period_start, period_end);
        assert_eq!(charge, 3150); // (9500-3200) * 0.5 = 3150.0 exactly
    }

    #[test]
    fn a_downgrade_charges_nothing() {
        assert_eq!(prorate_upgrade_charge(9500, 3200, 0, 0, 30 * DAY), 0);
    }

    #[test]
    fn a_same_price_change_charges_nothing() {
        assert_eq!(prorate_upgrade_charge(3200, 3200, 0, 0, 30 * DAY), 0);
    }

    #[test]
    fn an_upgrade_at_the_very_end_of_the_period_charges_almost_nothing() {
        let period_start = 0;
        let period_end = 30 * DAY;
        let now = period_end - 1;
        let charge = prorate_upgrade_charge(3200, 9500, now, period_start, period_end);
        assert!(charge >= 0 && charge < 10, "expected a near-zero charge, got {charge}");
    }

    #[test]
    fn now_past_period_end_charges_nothing_rather_than_going_negative() {
        let period_start = 0;
        let period_end = 30 * DAY;
        let now = period_end + DAY; // clock skew / a late call
        assert_eq!(prorate_upgrade_charge(3200, 9500, now, period_start, period_end), 0);
    }

    #[test]
    fn a_zero_length_period_never_divides_by_zero() {
        // period_start == period_end: `period_len` floors to 1 rather than 0,
        // so this returns a value instead of panicking or producing NaN/inf.
        let charge = prorate_upgrade_charge(3200, 9500, 100, 100, 100);
        assert_eq!(charge, 0); // `remaining` clamps to 0 when now >= period_end
    }
}
