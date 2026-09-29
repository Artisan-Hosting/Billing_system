//! Billing's own domain types -- kept separate from `proto::billing`
//! (generated wire types) and from `db::*` (raw row types), the same split
//! `domain_management` draws between its proto, its db rows, and its own
//! business-logic types.

use serde::{Deserialize, Serialize};

/// An organization's billing lifecycle state for one subscription
/// (`subscriptions.status`). A domain-specific superset of the platform's
/// canonical `Status` (see `artisan_middleware::aggregator::Status`'s own
/// wire-format contract, which this mirrors) -- owned by Billing because
/// billing/suspension state is deliberately not modeled on the shared
/// `Organization` type (`RESOURCE_TAXONOMY.md` §3.1), the same way
/// `DomainStatus` is owned by `domain_management` rather than folded into
/// the canonical enum.
///
/// # Wire format
///
/// MUST cross the `subscriptions.status` column and any gRPC boundary via
/// [`BillingStatus::as_str_name`] / [`BillingStatus::from_str_name`], never
/// via an ad hoc lowercase/uppercase match -- an unrecognized value is
/// always an `Err`, never silently coerced to a default variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BillingStatus {
    /// Paid and current.
    Active,
    /// The most recent invoice failed to collect and Stripe is still retrying
    /// automatically. Resources keep running -- this is not yet the 7-day
    /// grace period the Price Book's billing rules describe.
    PastDue,
    /// Stripe's retries were exhausted without collecting; the 7-day grace
    /// period is now counting down. Resources still keep running -- this is
    /// "about to be suspended," not "already suspended" (see [`Suspended`]).
    ///
    /// [`Suspended`]: BillingStatus::Suspended
    GracePeriod,
    /// The grace period elapsed without payment: resources are suspended
    /// (instances stopped) but not yet deleted.
    Suspended,
    /// Past the 30-day delete window (see the Price Book's billing rules).
    /// Terminal -- a deleted subscription is never reactivated, a new one is
    /// created instead.
    Deleted,
}

/// Returned by [`BillingStatus::from_str_name`] for an unrecognized wire
/// value, carrying the raw string so the caller can log it before deciding
/// on a *visible* fallback -- never a silent downgrade to any variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingStatusParseError(pub String);

impl std::fmt::Display for BillingStatusParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unrecognized BillingStatus wire value: {:?}", self.0)
    }
}

impl std::error::Error for BillingStatusParseError {}

impl BillingStatus {
    pub fn as_str_name(&self) -> &'static str {
        match self {
            BillingStatus::Active => "active",
            BillingStatus::PastDue => "past_due",
            BillingStatus::GracePeriod => "grace_period",
            BillingStatus::Suspended => "suspended",
            BillingStatus::Deleted => "deleted",
        }
    }

    pub fn from_str_name(s: &str) -> Result<Self, BillingStatusParseError> {
        match s {
            "active" => Ok(BillingStatus::Active),
            "past_due" => Ok(BillingStatus::PastDue),
            "grace_period" => Ok(BillingStatus::GracePeriod),
            "suspended" => Ok(BillingStatus::Suspended),
            "deleted" => Ok(BillingStatus::Deleted),
            _ => Err(BillingStatusParseError(s.to_owned())),
        }
    }

    /// Whether a *new*-purchase-shaped action (starting a VM/GPU session,
    /// placing a domain order) should be allowed for a subscription in this
    /// status. `PastDue` still allows new purchases -- only `GracePeriod`
    /// onward actually blocks -- matching the Price Book's stated rule that
    /// resources are stopped at grace-period end, not at the first missed
    /// payment.
    pub fn permits_new_purchases(&self) -> bool {
        matches!(self, BillingStatus::Active | BillingStatus::PastDue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_billing_status_round_trips_through_as_str_and_from_str() {
        for status in [
            BillingStatus::Active,
            BillingStatus::PastDue,
            BillingStatus::GracePeriod,
            BillingStatus::Suspended,
            BillingStatus::Deleted,
        ] {
            assert_eq!(BillingStatus::from_str_name(status.as_str_name()), Ok(status));
        }
    }

    #[test]
    fn from_str_name_rejects_unrecognized_values_instead_of_silently_falling_back() {
        let err = BillingStatus::from_str_name("not-a-status")
            .expect_err("unrecognized value must be Err, not a silent default");
        assert_eq!(err.0, "not-a-status");
    }

    #[test]
    fn only_active_and_past_due_permit_new_purchases() {
        assert!(BillingStatus::Active.permits_new_purchases());
        assert!(BillingStatus::PastDue.permits_new_purchases());
        assert!(!BillingStatus::GracePeriod.permits_new_purchases());
        assert!(!BillingStatus::Suspended.permits_new_purchases());
        assert!(!BillingStatus::Deleted.permits_new_purchases());
    }
}
