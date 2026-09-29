//! Credit units and the GPU markup: pure, database- and network-free, the
//! same reasoning as [`crate::overage`] -- the money math is unit-testable
//! without a live database.
//!
//! **The ledger is denominated in micro-dollars** (1 USD = 1_000_000
//! "micros"; 1 cent = 10_000 micros), not cents. GPU rates are far below a
//! cent per second (a $0.30/hr T4 costs ~0.0083 cents/second) while
//! RunpodManager meters roughly every second, so whole-cent debits would
//! over- or under-charge badly. Stripe still deals in cents, so a top-up is
//! converted at the boundary with [`cents_to_micros`].
//!
//! **Billing owns the markup.** The session manager (RunpodManager) hands us
//! the raw Runpod cost per hour; [`customer_rate_micros_per_hour`] applies
//! the configured markup and rounds the *hourly* price up to a clean step
//! (Price Book v1: cost x 1.35, rounded up to $0.05), and
//! [`debit_micros`] prices a slice of session time at that rate.

/// Micro-dollars in one cent.
pub const MICROS_PER_CENT: i64 = 10_000;

const MS_PER_HOUR: i128 = 3_600_000;

pub fn cents_to_micros(cents: i64) -> i64 {
    cents.saturating_mul(MICROS_PER_CENT)
}

/// Whole cents, rounded toward negative infinity, so a balance of 0.5 cents
/// displays as 0 and a slightly negative balance never displays as 0.
pub fn micros_to_cents_floor(micros: i64) -> i64 {
    micros.div_euclid(MICROS_PER_CENT)
}

/// What a customer pays per hour for a GPU that costs us
/// `cost_micros_per_hour`: `cost x markup_percent / 100`, rounded **up** to
/// a multiple of `step_micros` (a step of 0 disables rounding). `None` for a
/// non-positive cost or a result that doesn't fit an `i64`.
pub fn customer_rate_micros_per_hour(cost_micros_per_hour: i64, markup_percent: u32, step_micros: i64) -> Option<i64> {
    if cost_micros_per_hour <= 0 || step_micros < 0 {
        return None;
    }
    let marked_up = ceil_div(cost_micros_per_hour as i128 * markup_percent as i128, 100);
    let rounded = if step_micros == 0 { marked_up } else { ceil_div(marked_up, step_micros as i128) * step_micros as i128 };
    i64::try_from(rounded).ok()
}

/// The price of `duration_ms` of a session at `rate_micros_per_hour`,
/// rounded to the nearest micro. Per-tick rounding error is at most half a
/// micro (5e-7 dollars), so summed over a long session it is negligible.
/// `None` for a non-positive rate or duration, or an overflow.
pub fn debit_micros(rate_micros_per_hour: i64, duration_ms: i64) -> Option<i64> {
    if rate_micros_per_hour <= 0 || duration_ms <= 0 {
        return None;
    }
    let product = rate_micros_per_hour as i128 * duration_ms as i128;
    i64::try_from((product + MS_PER_HOUR / 2) / MS_PER_HOUR).ok()
}

/// What a session must have on hand to start: `rate x min_hours`, rounded up.
pub fn required_micros(rate_micros_per_hour: i64, min_hours: f64) -> Option<i64> {
    if rate_micros_per_hour <= 0 || !min_hours.is_finite() || min_hours <= 0.0 {
        return None;
    }
    let required = (rate_micros_per_hour as f64 * min_hours).ceil();
    if required >= i64::MAX as f64 { None } else { Some(required as i64) }
}

fn ceil_div(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP_5_CENTS: i64 = 5 * MICROS_PER_CENT;

    #[test]
    fn cents_and_micros_convert_at_the_stripe_boundary() {
        assert_eq!(cents_to_micros(2_500), 25_000_000);
        assert_eq!(micros_to_cents_floor(25_000_000), 2_500);
        assert_eq!(micros_to_cents_floor(9_999), 0);
        assert_eq!(micros_to_cents_floor(-1), -1, "a slightly negative balance must not display as $0.00");
    }

    /// Cross-checks every GPU on the pricing spreadsheet's GPU sheet
    /// (Runpod cost -> list price at 1.35x rounded up to $0.05).
    #[test]
    fn markup_reproduces_the_price_book_gpu_prices() {
        let cases = [
            (0.20, 0.30), // T4
            (0.25, 0.35), // A4000
            (0.35, 0.50), // L4
            (0.38, 0.55), // A5000
            (0.40, 0.55), // RTX 3090
            (0.44, 0.60), // RTX 4090
            (0.75, 1.05), // A6000
            (0.78, 1.10), // A40
            (0.95, 1.30), // L40S
            (0.99, 1.35), // RTX 6000 Ada
            (1.89, 2.60), // A100 80GB
            (2.89, 3.95), // H100 PCIe
            (2.99, 4.05), // H100 SXM
            (3.99, 5.40), // H200
        ];
        for (cost, price) in cases {
            let cost_micros = (cost * 1_000_000.0_f64).round() as i64;
            let price_micros = (price * 1_000_000.0_f64).round() as i64;
            assert_eq!(
                customer_rate_micros_per_hour(cost_micros, 135, STEP_5_CENTS),
                Some(price_micros),
                "runpod ${cost}/hr should list at ${price}/hr"
            );
        }
    }

    #[test]
    fn an_exact_multiple_is_not_rounded_up_a_further_step() {
        // 0.40 * 1.35 = 0.54 -> 0.55, but 1.00 * 1.35 = 1.35 exactly on the step.
        assert_eq!(customer_rate_micros_per_hour(1_000_000, 135, STEP_5_CENTS), Some(1_350_000));
    }

    #[test]
    fn a_zero_step_disables_rounding_and_bad_inputs_are_refused() {
        assert_eq!(customer_rate_micros_per_hour(440_000, 135, 0), Some(594_000));
        assert_eq!(customer_rate_micros_per_hour(0, 135, STEP_5_CENTS), None);
        assert_eq!(customer_rate_micros_per_hour(-5, 135, STEP_5_CENTS), None);
        assert_eq!(customer_rate_micros_per_hour(100, 135, -1), None);
        assert_eq!(customer_rate_micros_per_hour(i64::MAX, 135, STEP_5_CENTS), None, "overflow is refused, not wrapped");
    }

    #[test]
    fn a_full_hour_debits_exactly_the_hourly_rate() {
        assert_eq!(debit_micros(600_000, 3_600_000), Some(600_000));
    }

    #[test]
    fn a_one_second_tick_is_representable_and_sums_to_the_hour() {
        // $0.30/hr T4: 83.33.. micros per second -> 83 per tick.
        let tick = debit_micros(300_000, 1_000).unwrap();
        assert_eq!(tick, 83);
        let hour: i64 = (0..3_600).map(|_| tick).sum();
        // Per-tick rounding drift stays tiny (0.4 micro per second here).
        assert!((300_000 - hour).abs() <= 3_600);
    }

    #[test]
    fn non_positive_durations_and_rates_debit_nothing() {
        assert_eq!(debit_micros(600_000, 0), None);
        assert_eq!(debit_micros(600_000, -1), None);
        assert_eq!(debit_micros(0, 1_000), None);
    }

    #[test]
    fn required_micros_is_rate_times_hours_rounded_up() {
        assert_eq!(required_micros(600_000, 1.0), Some(600_000));
        assert_eq!(required_micros(600_000, 1.5), Some(900_000));
        assert_eq!(required_micros(3, 0.5), Some(2));
        assert_eq!(required_micros(600_000, 0.0), None);
        assert_eq!(required_micros(600_000, f64::NAN), None);
    }
}
