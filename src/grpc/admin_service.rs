//! `BillingAdminService` implementation -- plans, subscriptions, invoices,
//! and the GPU/LLM prepaid credit ledger. See `proto/billing.proto`'s own
//! module doc comment for the two authorization shapes this file's handlers
//! split into (end-user-facing vs. internal-only).
//!
//! GPU credit RPCs (`GetCreditBalance`/`TopUpCredit`/`DebitCredit`/
//! `PreflightCreditCheck`) are implemented fully here even though nothing
//! calls them yet -- RunpodManager's metering integration is deferred
//! pending a Runpod rework (see the billing overhaul plan's sequencing,
//! updated 2026-09-28). Implementing Billing's own side of the contract now
//! means it's simply waiting for a caller, not still to be built, when that
//! rework lands.

use tonic::{Request, Response, Status};

use crate::db::{credits as credits_db, invoices as inv_db, plans as plans_db, subscriptions as sub_db};
use crate::domain::BillingStatus;
use crate::grpc::service::Billing;
use crate::proto::billing::*;
use crate::proto::billing::billing_admin_service_server::BillingAdminService;
use std::collections::HashSet;
// `create_payment_intent` is a `BillingService` trait method reused
// in-process (see `finalize_invoice_and_maybe_charge`'s own comment) --
// needs the trait in scope even though this file implements the *other*
// service.
use crate::proto::billing::billing_service_server::BillingService;

use artisan_middleware::api::claims::{Claims, TokenType};
use artisan_middleware::api::roles::Role;
use artisan_middleware::identity::Action;

/// The wire value for `ResourceType::Subscription` -- see
/// `auth::AuthClient::evaluate_access`'s own doc comment for why this is a
/// string literal rather than the typed enum for now.
const RESOURCE_TYPE_SUBSCRIPTION: &str = "subscription";

const VALID_STOREFRONTS: [&str; 3] = ["developer", "business", "email"];

/// A brand new subscription's period length. A flat 30 days rather than a
/// calendar month -- simpler to reason about and test; revisit if billing
/// needs to line up with calendar-month statements later.
const DEFAULT_PERIOD_SECONDS: i64 = 30 * 24 * 60 * 60;

/// The consumer name `BillingAdminService` stamps on every `PaymentIntent`
/// it creates via `BillingService::create_payment_intent` (called in-process
/// -- see `create_or_upgrade_subscription`'s own comment on why this reuses
/// that handler directly rather than duplicating its idempotency logic).
const PAYMENT_CONSUMER: &str = "billing_subscriptions";

impl Billing {
    /// Who is calling. Every token is checked with ais_auth, the same
    /// reasoning `domain_management::Domains::caller` documents.
    async fn caller(&self, access_token: &str) -> Result<Claims, Status> {
        if access_token.is_empty() {
            return Err(Status::unauthenticated("no access token"));
        }
        Ok(self.auth.validate(access_token).await?)
    }

    /// Re-checks a step-up token and confirms it belongs to the same person
    /// as the access token that came with it -- verbatim the same check
    /// `domain_management::Domains::elevated` makes, and for the same
    /// reason: validity alone isn't enough, since an ordinary access token
    /// is also "valid."
    async fn elevated(&self, elevated_token: &str, caller: &Claims) -> Result<(), Status> {
        if elevated_token.is_empty() {
            return Err(Status::permission_denied("this needs an elevated token"));
        }

        let elevated = self.caller(elevated_token).await?;
        if elevated.kind != TokenType::Elevated {
            return Err(Status::permission_denied("that is an ordinary access token, not an elevated one"));
        }
        if elevated.sub != caller.sub {
            return Err(Status::permission_denied("the elevated token belongs to a different user"));
        }

        Ok(())
    }

    /// Org-pinning for a read or a non-money write: a non-Super caller may
    /// only act on their own organization. Returns the organization id to
    /// actually use (the caller's own, unless Super passed one explicitly).
    fn scoped_org(&self, claims: &Claims, requested_organization_id: &str) -> Result<String, Status> {
        if claims.role == Role::Super {
            if requested_organization_id.is_empty() {
                return Err(Status::invalid_argument("organization_id is required"));
            }
            return Ok(requested_organization_id.to_owned());
        }

        if claims.organization_id.is_empty() {
            return Err(Status::permission_denied("caller has no organization"));
        }
        if !requested_organization_id.is_empty() && requested_organization_id != claims.organization_id {
            return Err(Status::permission_denied("cannot act on another organization"));
        }

        Ok(claims.organization_id.clone())
    }

    /// Whether `claims` may do `action` to this organization's billing, decided
    /// by ais_auth's policy engine. The resource id is the **organization id**:
    /// a subscription belongs to an organization, and ais_auth resolves that id
    /// straight to its owning org -- so an Admin (an org-scoped Super) passes
    /// for their own organization only, other roles pass only where the org's
    /// policy says so, and a viewer sees nothing until the org opts them in.
    /// `Super` is the platform operator's support bypass and skips the call,
    /// as the policy engine would allow it anyway.
    ///
    /// A failure to reach ais_auth is an error (`Unavailable`), never a
    /// silent denial or a silent allow.
    async fn require_subscription_access(
        &self,
        claims: &Claims,
        organization_id: &str,
        action: Action,
        denied: &'static str,
    ) -> Result<(), Status> {
        if claims.role == Role::Super {
            return Ok(());
        }
        let allowed = self
            .auth
            .evaluate_access(claims, RESOURCE_TYPE_SUBSCRIPTION, organization_id, action)
            .await
            .map_err(Status::from)?;
        if allowed {
            Ok(())
        } else {
            Err(Status::permission_denied(denied))
        }
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn billing_status_to_i32(status: &str) -> i32 {
    match BillingStatus::from_str_name(status) {
        Ok(BillingStatus::Active) => proto_status::Active as i32,
        Ok(BillingStatus::PastDue) => proto_status::PastDue as i32,
        Ok(BillingStatus::GracePeriod) => proto_status::GracePeriod as i32,
        Ok(BillingStatus::Suspended) => proto_status::Suspended as i32,
        Ok(BillingStatus::Deleted) => proto_status::Deleted as i32,
        Ok(BillingStatus::Canceled) => proto_status::Canceled as i32,
        // Unreachable in practice -- this service is the only writer of
        // `subscriptions.status`, and only ever writes a value produced by
        // `BillingStatus::as_str_name()`. Falling back to UNSPECIFIED rather
        // than panicking keeps a data-integrity bug a visible wire value
        // instead of an outage.
        Err(_) => proto_status::Unspecified as i32,
    }
}

// Shorthand so the match arms above read as the enum's own variant names
// rather than the generated `BillingStatus::BillingStatusActive`-style path.
mod proto_status {
    pub use crate::proto::billing::BillingStatus::*;
}

/// Highest-severity status across a set of subscriptions: Deleted >
/// Suspended > GracePeriod > PastDue > Active. An org with no subscriptions
/// at all is `Active` -- "never subscribed to anything" is not a reason to
/// refuse the purchase that would create the first one (see
/// `OrgBillingStatus`'s own proto doc comment).
/// Whether `subscriptions` entitle the organization to deploy under `storefront`:
/// a subscription there that is Active or Past Due (the same two states
/// `domain_management` lets buy). A canceled, suspended, grace-period or deleted
/// one does not; neither does having none. Pure so the rule is tested directly.
fn entitlement_for(subscriptions: &[sub_db::SubscriptionRow], storefront: &str, paid_subscriptions: &std::collections::HashSet<u64>) -> OrgEntitlements {
    let here: Vec<&sub_db::SubscriptionRow> = subscriptions.iter().filter(|s| s.storefront == storefront).collect();
    if here.is_empty() {
        return OrgEntitlements {
            entitled: false,
            reason: format!("no {storefront} plan: choose a plan to deploy"),
            plan_code: String::new(),
            status: proto_status::Unspecified as i32,
        };
    }
    for s in &here {
        match BillingStatus::from_str_name(&s.status) {
            Ok(BillingStatus::Active) | Ok(BillingStatus::PastDue) => {
                return OrgEntitlements {
                    entitled: true,
                    reason: String::new(),
                    plan_code: s.plan_code.clone(),
                    status: billing_status_to_i32(&s.status),
                };
            }
            _ => {}
        }
    }
    let worst = here[0];
    OrgEntitlements {
        entitled: false,
        reason: "your plan is not in good standing: check Billing".to_owned(),
        plan_code: worst.plan_code.clone(),
        status: billing_status_to_i32(&worst.status),
    }
}

fn most_severe_status(subscriptions: &[sub_db::SubscriptionRow]) -> &'static str {
    fn severity(status: &str) -> u8 {
        match BillingStatus::from_str_name(status) {
            Ok(BillingStatus::Active) | Ok(BillingStatus::Canceled) => 0,
            Ok(BillingStatus::PastDue) => 1,
            Ok(BillingStatus::GracePeriod) => 2,
            Ok(BillingStatus::Suspended) => 3,
            Ok(BillingStatus::Deleted) => 4,
            Err(_) => 0,
        }
    }

    // A cancelled subscription is an org that stopped buying that product,
    // not one in trouble: left in, it could tie with `Active` and win.
    subscriptions
        .iter()
        .map(|s| s.status.as_str())
        .filter(|s| *s != BillingStatus::Canceled.as_str_name())
        .max_by_key(|s| severity(s))
        .map(|s| match BillingStatus::from_str_name(s) {
            Ok(BillingStatus::Active) => BillingStatus::Active.as_str_name(),
            Ok(BillingStatus::PastDue) => BillingStatus::PastDue.as_str_name(),
            Ok(BillingStatus::GracePeriod) => BillingStatus::GracePeriod.as_str_name(),
            Ok(BillingStatus::Suspended) => BillingStatus::Suspended.as_str_name(),
            Ok(BillingStatus::Deleted) => BillingStatus::Deleted.as_str_name(),
            Ok(BillingStatus::Canceled) | Err(_) => BillingStatus::Active.as_str_name(),
        })
        .unwrap_or(BillingStatus::Active.as_str_name())
}

fn subscription_row_to_proto(row: &sub_db::SubscriptionRow) -> Subscription {
    Subscription {
        id: row.id.to_string(),
        organization_id: row.organization_id.clone(),
        storefront: row.storefront.clone(),
        plan_code: row.plan_code.clone(),
        status: billing_status_to_i32(&row.status),
        current_period_start: row.current_period_start,
        current_period_end: row.current_period_end,
        pending_plan_code: row.pending_plan_code.clone().unwrap_or_default(),
        cancel_at_period_end: row.cancel_at_period_end,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn invoice_row_to_proto(row: &inv_db::InvoiceRow, line_items: Vec<inv_db::InvoiceLineItemRow>) -> Invoice {
    Invoice {
        id: row.id.to_string(),
        organization_id: row.organization_id.clone(),
        subscription_id: row.subscription_id.map(|id| id.to_string()).unwrap_or_default(),
        period_start: row.period_start,
        period_end: row.period_end,
        status: row.status.clone(),
        total_cents: row.total_cents,
        currency: row.currency.clone(),
        stripe_payment_intent_id: row.stripe_payment_intent_id.clone().unwrap_or_default(),
        line_items: line_items
            .into_iter()
            .map(|item| InvoiceLineItem {
                unit_code: item.unit_code.unwrap_or_default(),
                description: item.description,
                quantity: item.quantity,
                unit_price_cents: item.unit_price_cents,
                amount_cents: item.amount_cents,
            })
            .collect(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Creates a finalized invoice for `charge_cents` and, if there's anything
/// to collect, a Stripe PaymentIntent for it. Shared by
/// `create_or_upgrade_subscription` and `record_overage_usage` -- both are
/// "here's what this subscription owes for a period, go collect it."
async fn finalize_invoice_and_maybe_charge(
    billing: &Billing,
    organization_id: &str,
    subscription_id: Option<u64>,
    period_start: i64,
    period_end: i64,
    line_items: &[(Option<&str>, String, f64, f64, i64)],
    total_cents: i64,
) -> Result<(inv_db::InvoiceRow, Option<PaymentIntent>), Status> {
    let invoice_id =
        inv_db::insert_draft(&billing.pool, organization_id, subscription_id, period_start, period_end, "usd")
            .await
            .map_err(Status::from)?;

    for (unit_code, description, quantity, unit_price_cents, amount_cents) in line_items {
        inv_db::insert_line_item(&billing.pool, invoice_id, *unit_code, description, *quantity, *unit_price_cents, *amount_cents)
            .await
            .map_err(Status::from)?;
    }

    inv_db::finalize(&billing.pool, invoice_id, total_cents).await.map_err(Status::from)?;

    let payment_intent = if total_cents > 0 {
        // In-process call, not a network hop: `BillingAdminService` and
        // `BillingService` are the same `Billing` struct (see
        // `grpc::service::Billing`'s own doc comment), so this reuses
        // `create_payment_intent`'s full idempotency logic (DB row +
        // Stripe idempotency key) exactly as if a separate caller had
        // dialed it over gRPC, without duplicating that logic here.
        let pi = billing
            .create_payment_intent(Request::new(CreatePaymentIntentRequest {
                consumer: PAYMENT_CONSUMER.to_owned(),
                external_reference: invoice_id.to_string(),
                amount_cents: total_cents,
                currency: "usd".to_owned(),
                metadata: [("organization_id".to_owned(), organization_id.to_owned())].into(),
            }))
            .await?
            .into_inner();

        inv_db::set_stripe_payment_intent(&billing.pool, invoice_id, &pi.stripe_payment_intent_id)
            .await
            .map_err(Status::from)?;

        Some(pi)
    } else {
        None
    };

    let invoice = inv_db::find(&billing.pool, invoice_id)
        .await
        .map_err(Status::from)?
        .ok_or_else(|| Status::internal("invoice vanished mid-create"))?;

    Ok((invoice, payment_intent))
}

/// Makes sure an `open` invoice has a PaymentIntent to pay, and returns its
/// Stripe id. Idempotent: an invoice that already has one is returned as-is,
/// and `create_payment_intent` itself is idempotent per
/// `(consumer, invoice id)`, so a crash between the Stripe call and writing
/// the id back only ever costs a retry. Shared by the period rollover job
/// (which creates invoices with no PaymentIntent) and `RetryInvoicePayment`.
pub(crate) async fn ensure_invoice_payment_intent(billing: &Billing, invoice: &inv_db::InvoiceRow) -> Result<String, Status> {
    if let Some(existing) = &invoice.stripe_payment_intent_id {
        return Ok(existing.clone());
    }

    let pi = billing
        .create_payment_intent(Request::new(CreatePaymentIntentRequest {
            consumer: PAYMENT_CONSUMER.to_owned(),
            external_reference: invoice.id.to_string(),
            amount_cents: invoice.total_cents,
            currency: invoice.currency.clone(),
            metadata: [("organization_id".to_owned(), invoice.organization_id.clone())].into(),
        }))
        .await?
        .into_inner();
    inv_db::set_stripe_payment_intent(&billing.pool, invoice.id, &pi.stripe_payment_intent_id)
        .await
        .map_err(Status::from)?;
    Ok(pi.stripe_payment_intent_id)
}

#[tonic::async_trait]
impl BillingAdminService for Billing {
    async fn get_subscription(&self, request: Request<GetSubscriptionRequest>) -> Result<Response<Subscription>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Read, "not permitted to view billing")
            .await?;

        let row = sub_db::find(&self.pool, &organization_id, &req.storefront)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no {} subscription for this organization", req.storefront)))?;

        Ok(Response::new(subscription_row_to_proto(&row)))
    }

    /// AUTHZ: Action::Purchase on the `subscription` resource type **and**
    /// an elevated token -- see this RPC's own doc comment in
    /// `proto/billing.proto`.
    async fn create_or_upgrade_subscription(
        &self,
        request: Request<CreateOrUpgradeSubscriptionRequest>,
    ) -> Result<Response<SubscriptionCheckout>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        self.elevated(&req.elevated_token, &claims).await?;

        if !self.config.purchasing.enabled {
            return Err(Status::failed_precondition("subscription purchasing is not enabled"));
        }

        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Purchase, "not permitted to purchase subscriptions")
            .await?;

        if !VALID_STOREFRONTS.contains(&req.storefront.as_str()) {
            return Err(Status::invalid_argument(format!("unknown storefront {:?}", req.storefront)));
        }

        let plan = plans_db::find(&self.pool, &req.plan_code)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no such plan {:?}", req.plan_code)))?;
        if !plan.active {
            return Err(Status::failed_precondition(format!("plan {:?} is no longer sold", req.plan_code)));
        }
        if plan.storefront != req.storefront {
            return Err(Status::invalid_argument(format!(
                "plan {:?} belongs to the {:?} storefront, not {:?}",
                req.plan_code, plan.storefront, req.storefront
            )));
        }

        let found = sub_db::find(&self.pool, &organization_id, &req.storefront).await.map_err(Status::from)?;
        // A subscription whose cancellation already ran out is bought afresh
        // (same row, new period) rather than upgraded from the old plan.
        let restarting = found.as_ref().filter(|r| r.status == BillingStatus::Canceled.as_str_name()).map(|r| r.id);
        let existing = found.filter(|r| r.status != BillingStatus::Canceled.as_str_name());
        let now = now();

        let (subscription_id, old_price_cents, period_start, period_end) = match &existing {
            Some(row) => (row.id, {
                let old_plan = plans_db::find(&self.pool, &row.plan_code)
                    .await
                    .map_err(Status::from)?
                    .ok_or_else(|| Status::internal(format!("subscription references unknown plan {:?}", row.plan_code)))?;
                old_plan.price_cents
            }, row.current_period_start, row.current_period_end),
            None => (restarting.unwrap_or(0), 0, now, now + DEFAULT_PERIOD_SECONDS),
        };

        if plan.price_cents <= old_price_cents && existing.is_some() {
            return Err(Status::invalid_argument(
                "this is not an upgrade -- use ScheduleDowngrade for a downgrade or lateral move",
            ));
        }

        let charge_cents =
            crate::proration::prorate_upgrade_charge(old_price_cents, plan.price_cents, now, period_start, period_end);

        let subscription_id = match subscription_id {
            id if existing.is_none() && restarting.is_some() => {
                let status =
                    if charge_cents <= 0 { BillingStatus::Active.as_str_name() } else { BillingStatus::PastDue.as_str_name() };
                sub_db::restart(&self.pool, id, &req.plan_code, status, period_start, period_end)
                    .await
                    .map_err(Status::from)?;
                id
            }
            0 => {
                // Brand new: starts PastDue (not Active) until the invoice
                // created below is actually paid -- unless it's free, in
                // which case there's nothing to wait on. See
                // `SubscriptionCheckout`'s own proto doc comment.
                let status =
                    if charge_cents <= 0 { BillingStatus::Active.as_str_name() } else { BillingStatus::PastDue.as_str_name() };
                sub_db::insert(&self.pool, &organization_id, &req.storefront, &req.plan_code, status, period_start, period_end)
                    .await
                    .map_err(Status::from)?
            }
            id => {
                // An upgrade takes effect immediately; the existing
                // subscription's own status (Active, PastDue, whatever it
                // already was) is left untouched -- a new prorated invoice
                // is its own thing to collect, not a reason to retroactively
                // mark the whole subscription delinquent (see this file's
                // module doc for what's deferred: the suspend/grace/delete
                // lifecycle job is what actually acts on unpaid invoices).
                sub_db::set_plan(&self.pool, id, &req.plan_code).await.map_err(Status::from)?;
                id
            }
        };

        let description = if existing.is_some() {
            format!("Upgrade to {} (prorated)", plan.display_name)
        } else {
            plan.display_name.clone()
        };
        let (invoice, payment_intent) = finalize_invoice_and_maybe_charge(
            self,
            &organization_id,
            Some(subscription_id),
            period_start,
            period_end,
            &[(None, description, 1.0, charge_cents as f64, charge_cents)],
            charge_cents,
        )
        .await?;

        let subscription = sub_db::find_by_id(&self.pool, subscription_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("subscription vanished mid-create"))?;

        Ok(Response::new(SubscriptionCheckout {
            subscription: Some(subscription_row_to_proto(&subscription)),
            invoice: Some(invoice_row_to_proto(&invoice, Vec::new())),
            stripe_client_secret: payment_intent.as_ref().map(|pi| pi.client_secret.clone()).unwrap_or_default(),
            stripe_publishable_key: payment_intent.map(|pi| pi.publishable_key).unwrap_or_default(),
        }))
    }

    async fn schedule_downgrade(
        &self,
        request: Request<ScheduleDowngradeRequest>,
    ) -> Result<Response<Subscription>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Write, "not permitted to change subscriptions")
            .await?;

        let row = sub_db::find(&self.pool, &organization_id, &req.storefront)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no {} subscription for this organization", req.storefront)))?;

        let new_plan = plans_db::find(&self.pool, &req.plan_code)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no such plan {:?}", req.plan_code)))?;
        if new_plan.storefront != req.storefront {
            return Err(Status::invalid_argument("plan does not belong to this storefront"));
        }
        let current_plan = plans_db::find(&self.pool, &row.plan_code)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("subscription references unknown plan"))?;
        if new_plan.price_cents > current_plan.price_cents {
            return Err(Status::invalid_argument(
                "this is an upgrade -- use CreateOrUpgradeSubscription, which charges immediately",
            ));
        }

        sub_db::schedule_downgrade(&self.pool, row.id, &req.plan_code).await.map_err(Status::from)?;

        let updated = sub_db::find_by_id(&self.pool, row.id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("subscription vanished mid-update"))?;
        Ok(Response::new(subscription_row_to_proto(&updated)))
    }

    async fn cancel_subscription(
        &self,
        request: Request<CancelSubscriptionRequest>,
    ) -> Result<Response<Subscription>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Write, "not permitted to change subscriptions")
            .await?;

        let row = sub_db::find(&self.pool, &organization_id, &req.storefront)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no {} subscription for this organization", req.storefront)))?;

        sub_db::set_cancel_at_period_end(&self.pool, row.id, true).await.map_err(Status::from)?;

        let updated = sub_db::find_by_id(&self.pool, row.id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("subscription vanished mid-update"))?;
        Ok(Response::new(subscription_row_to_proto(&updated)))
    }

    async fn record_overage_usage(&self, request: Request<RecordOverageUsageRequest>) -> Result<Response<Invoice>, Status> {
        let req = request.into_inner();

        let subscription = sub_db::find(&self.pool, &req.organization_id, &req.storefront)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no {} subscription for this organization", req.storefront)))?;

        let plan = plans_db::find(&self.pool, &subscription.plan_code)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("subscription references unknown plan"))?;
        let catalog = plans_db::allowances_and_rates_for_plan(&self.pool, &subscription.plan_code)
            .await
            .map_err(Status::from)?;

        let mut usage_by_unit = std::collections::HashMap::new();
        for u in &req.usage {
            usage_by_unit.insert(u.unit_code.as_str(), u.quantity);
        }
        let pool_usage = crate::overage::PoolUsage {
            ram_gb_avg: usage_by_unit.get("ram_gb_month").copied().unwrap_or(0.0),
            vcpu_avg: usage_by_unit.get("vcpu_month").copied().unwrap_or(0.0),
            egress_gb_total: usage_by_unit.get("egress_gb").copied().unwrap_or(0.0),
            email_1k_total: usage_by_unit.get("email_1k").copied().unwrap_or(0.0),
        };
        let overage = crate::overage::calculate_pool_overage(&pool_usage, &catalog.allowances, &catalog.rates);

        let mut line_items: Vec<(Option<&str>, String, f64, f64, i64)> =
            vec![(None, plan.display_name.clone(), 1.0, plan.price_cents as f64, plan.price_cents)];
        for item in &overage.line_items {
            line_items.push((
                Some(item.unit_code),
                format!("{} overage", item.unit_code),
                item.over_qty,
                item.rate_cents_per_unit,
                item.amount_cents,
            ));
        }
        let total_cents = plan.price_cents + overage.total_cents;

        let (invoice, _payment_intent) = finalize_invoice_and_maybe_charge(
            self,
            &req.organization_id,
            Some(subscription.id),
            req.period_start,
            req.period_end,
            &line_items,
            total_cents,
        )
        .await?;

        let items = inv_db::line_items(&self.pool, invoice.id).await.map_err(Status::from)?;
        Ok(Response::new(invoice_row_to_proto(&invoice, items)))
    }

    async fn list_invoices(&self, request: Request<ListInvoicesRequest>) -> Result<Response<ListInvoicesResponse>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Read, "not permitted to view billing")
            .await?;

        let limit = if req.limit > 0 { req.limit as i64 } else { 50 };
        let storefront = if req.storefront.is_empty() { None } else { Some(req.storefront.as_str()) };

        let rows = inv_db::list_for_org(&self.pool, &organization_id, storefront, limit, req.offset as i64)
            .await
            .map_err(Status::from)?;

        let mut invoices = Vec::with_capacity(rows.len());
        for row in rows {
            let items = inv_db::line_items(&self.pool, row.id).await.map_err(Status::from)?;
            invoices.push(invoice_row_to_proto(&row, items));
        }

        let total = inv_db::count_for_org(&self.pool, &organization_id, storefront).await.map_err(Status::from)?;

        Ok(Response::new(ListInvoicesResponse { invoices, total }))
    }

    /// Any signed-in caller: the catalog is the public Price Book.
    async fn list_plans(&self, request: Request<ListPlansRequest>) -> Result<Response<ListPlansResponse>, Status> {
        let req = request.into_inner();
        self.caller(&req.access_token).await?;

        let storefront = if req.storefront.is_empty() { None } else { Some(req.storefront.as_str()) };
        let rows = plans_db::list_active(&self.pool, storefront).await.map_err(Status::from)?;

        let mut plans = Vec::with_capacity(rows.len());
        for row in rows {
            let catalog = plans_db::allowances_and_rates_for_plan(&self.pool, &row.plan_code)
                .await
                .map_err(Status::from)?;
            let mut units: Vec<PlanUnit> = catalog
                .allowances
                .iter()
                .map(|(unit_code, included)| PlanUnit {
                    unit_code: unit_code.clone(),
                    included_qty: *included,
                    overage_rate_cents_per_unit: catalog.rates.get(unit_code).copied().unwrap_or(0.0),
                    overage_billed: catalog.rates.contains_key(unit_code),
                })
                .collect();
            units.sort_by(|a, b| a.unit_code.cmp(&b.unit_code));
            plans.push(Plan {
                plan_code: row.plan_code,
                storefront: row.storefront,
                display_name: row.display_name,
                price_cents: row.price_cents,
                currency: row.currency,
                units,
            });
        }

        Ok(Response::new(ListPlansResponse { plans }))
    }

    async fn list_subscriptions(
        &self,
        request: Request<ListSubscriptionsRequest>,
    ) -> Result<Response<ListSubscriptionsResponse>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Read, "not permitted to view billing")
            .await?;

        let rows = sub_db::list_for_org(&self.pool, &organization_id).await.map_err(Status::from)?;
        Ok(Response::new(ListSubscriptionsResponse { subscriptions: rows.iter().map(subscription_row_to_proto).collect() }))
    }

    /// AUTHZ: Action::Purchase on the `subscription` resource type **and**
    /// an elevated token -- see this RPC's own doc comment in
    /// `proto/billing.proto`.
    async fn retry_invoice_payment(
        &self,
        request: Request<RetryInvoicePaymentRequest>,
    ) -> Result<Response<SubscriptionCheckout>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        self.elevated(&req.elevated_token, &claims).await?;

        if !self.config.purchasing.enabled {
            return Err(Status::failed_precondition("subscription purchasing is not enabled"));
        }

        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Purchase, "not permitted to purchase subscriptions")
            .await?;

        let invoice_id: u64 = req.invoice_id.parse().map_err(|_| Status::invalid_argument("invoice_id is not a number"))?;
        // Another org's invoice is indistinguishable from a missing one.
        let invoice = inv_db::find(&self.pool, invoice_id)
            .await
            .map_err(Status::from)?
            .filter(|i| i.organization_id == organization_id)
            .ok_or_else(|| Status::not_found("no such invoice"))?;
        if invoice.status != "open" {
            return Err(Status::failed_precondition(format!("invoice is {}, nothing to pay", invoice.status)));
        }

        let mut stripe_id = ensure_invoice_payment_intent(self, &invoice).await?;
        let mut stripe_pi = self.stripe.get_payment_intent(&stripe_id).await.map_err(Status::from)?;

        if stripe_pi.status == "succeeded" {
            // Paid, and the webhook just hasn't landed yet.
            return Err(Status::failed_precondition("this invoice is already paid; confirmation is on its way"));
        }
        if stripe_pi.status == "canceled" {
            // A cancelled PaymentIntent can't be paid. Replace it, keyed on
            // the dead one's id so a retry of this same call is idempotent.
            let replacement = self
                .create_payment_intent(Request::new(CreatePaymentIntentRequest {
                    consumer: PAYMENT_CONSUMER.to_owned(),
                    external_reference: format!("{}:after:{stripe_id}", invoice.id),
                    amount_cents: invoice.total_cents,
                    currency: invoice.currency.clone(),
                    metadata: [("organization_id".to_owned(), organization_id.clone())].into(),
                }))
                .await?
                .into_inner();
            inv_db::set_stripe_payment_intent(&self.pool, invoice.id, &replacement.stripe_payment_intent_id)
                .await
                .map_err(Status::from)?;
            stripe_id = replacement.stripe_payment_intent_id;
            stripe_pi = self.stripe.get_payment_intent(&stripe_id).await.map_err(Status::from)?;
        }

        let invoice = inv_db::find(&self.pool, invoice.id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("invoice vanished mid-retry"))?;
        let items = inv_db::line_items(&self.pool, invoice.id).await.map_err(Status::from)?;
        let subscription = match invoice.subscription_id {
            Some(id) => sub_db::find_by_id(&self.pool, id).await.map_err(Status::from)?,
            None => None,
        };

        Ok(Response::new(SubscriptionCheckout {
            subscription: subscription.as_ref().map(subscription_row_to_proto),
            invoice: Some(invoice_row_to_proto(&invoice, items)),
            stripe_client_secret: stripe_pi.client_secret.unwrap_or_default(),
            stripe_publishable_key: self.secrets.stripe_publishable_key.clone(),
        }))
    }

    /// Internal only, no end-user token -- see this RPC's own doc comment in
    /// `proto/billing.proto` for why (the primary caller is another
    /// service's own purchase-gating, with no end-user token of its own to
    /// forward yet).
    async fn get_organization_billing_status(
        &self,
        request: Request<GetOrganizationBillingStatusRequest>,
    ) -> Result<Response<OrgBillingStatus>, Status> {
        let req = request.into_inner();
        if req.organization_id.is_empty() {
            return Err(Status::invalid_argument("organization_id is required"));
        }
        let organization_id = req.organization_id;

        let rows = sub_db::list_for_org(&self.pool, &organization_id).await.map_err(Status::from)?;
        let overall = most_severe_status(&rows);

        Ok(Response::new(OrgBillingStatus {
            organization_id: organization_id.clone(),
            status: billing_status_to_i32(overall),
            subscriptions: rows
                .iter()
                .map(|row| SubscriptionStatusSummary {
                    storefront: row.storefront.clone(),
                    plan_code: row.plan_code.clone(),
                    status: billing_status_to_i32(&row.status),
                })
                .collect(),
        }))
    }

    /// Internal only. See the RPC's doc comment in `proto/billing.proto`.
    async fn get_organization_entitlements(
        &self,
        request: Request<GetOrganizationEntitlementsRequest>,
    ) -> Result<Response<OrgEntitlements>, Status> {
        let req = request.into_inner();
        if req.organization_id.is_empty() {
            return Err(Status::invalid_argument("organization_id is required"));
        }
        if req.storefront.is_empty() {
            return Err(Status::invalid_argument("storefront is required"));
        }
        let rows = sub_db::list_for_org(&self.pool, &req.organization_id).await.map_err(Status::from)?;
        let sub_ids: Vec<u64> = rows.iter().map(|s| s.id).collect();
        let paid_set = inv_db::subscriptions_with_paid_invoice(&self.pool, &sub_ids).await.map_err(Status::from)?;
        Ok(Response::new(entitlement_for(&rows, &req.storefront, &paid_set)))
    }

    async fn get_credit_balance(&self, request: Request<GetCreditBalanceRequest>) -> Result<Response<CreditBalance>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Read, "not permitted to view billing")
            .await?;

        let account = credits_db::get_or_create(&self.pool, &organization_id).await.map_err(Status::from)?;
        Ok(Response::new(CreditBalance {
            organization_id: account.organization_id,
            balance_cents: account.balance_cents,
            monthly_spend_cap_cents: account.monthly_spend_cap_cents.unwrap_or(0),
        }))
    }

    async fn list_credit_ledger(
        &self,
        request: Request<ListCreditLedgerRequest>,
    ) -> Result<Response<ListCreditLedgerResponse>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Read, "not permitted to view billing")
            .await?;

        let limit = if req.limit > 0 { (req.limit as i64).min(200) } else { 50 };
        let (rows, total) = credits_db::list_ledger(&self.pool, &organization_id, limit, (req.offset as i64).max(0))
            .await
            .map_err(Status::from)?;

        Ok(Response::new(ListCreditLedgerResponse {
            entries: rows
                .into_iter()
                .map(|r| CreditLedgerEntry {
                    id: r.id,
                    entry_type: r.entry_type,
                    amount_cents: r.amount_cents,
                    balance_after_cents: r.balance_after_cents,
                    external_reference: r.external_reference.unwrap_or_default(),
                    created_at: r.created_at,
                })
                .collect(),
            total,
        }))
    }

    /// AUTHZ: Action::Purchase on the `subscription` resource type **and**
    /// an elevated token, same bar as `create_or_upgrade_subscription` --
    /// see this RPC's own doc comment in `proto/billing.proto`.
    async fn top_up_credit(&self, request: Request<TopUpCreditRequest>) -> Result<Response<PaymentIntent>, Status> {
        let req = request.into_inner();
        let claims = self.caller(&req.access_token).await?;
        self.elevated(&req.elevated_token, &claims).await?;

        if !self.config.purchasing.enabled {
            return Err(Status::failed_precondition("credit top-ups are not enabled"));
        }

        let organization_id = self.scoped_org(&claims, &req.organization_id)?;
        self.require_subscription_access(&claims, &organization_id, Action::Purchase, "not permitted to purchase credits")
            .await?;
        if req.amount_cents < 2_500 {
            // $25 minimum, the Price Book's stated floor -- otherwise
            // Stripe's own per-transaction fee eats a meaningful share of a
            // very small top-up.
            return Err(Status::invalid_argument("minimum top-up is $25.00 (2500 cents)"));
        }

        let currency = if req.currency.is_empty() { "usd".to_owned() } else { req.currency.to_lowercase() };
        // Idempotent per (organization_id, amount_cents, minute) would be
        // nice but isn't how CreatePaymentIntent's idempotency works today
        // (it's keyed on (consumer, external_reference) only) -- a fresh
        // external_reference per call is correct here: a top-up isn't
        // naturally retriable at the request level the way an invoice
        // charge is (the caller only ever calls this once per intended
        // top-up), so each call creates its own PaymentIntent.
        let external_reference = uuid_like();

        let pi = self
            .create_payment_intent(Request::new(CreatePaymentIntentRequest {
                consumer: super::service::TOPUP_CONSUMER.to_owned(),
                external_reference,
                amount_cents: req.amount_cents,
                currency,
                metadata: [("organization_id".to_owned(), organization_id)].into(),
            }))
            .await?
            .into_inner();

        Ok(Response::new(pi))
    }

    async fn debit_credit(&self, request: Request<DebitCreditRequest>) -> Result<Response<CreditBalance>, Status> {
        let req = request.into_inner();
        if req.organization_id.is_empty() {
            return Err(Status::invalid_argument("organization_id is required"));
        }
        if req.amount_cents <= 0 {
            return Err(Status::invalid_argument("amount_cents must be a positive number of minor units to debit"));
        }

        let idempotency_key = if req.idempotency_key.is_empty() { None } else { Some(req.idempotency_key.as_str()) };
        let external_reference = if req.external_reference.is_empty() { None } else { Some(req.external_reference.as_str()) };

        credits_db::apply_ledger_entry(&self.pool, &req.organization_id, "debit", -req.amount_cents, external_reference, idempotency_key)
            .await
            .map_err(Status::from)?;

        let account = credits_db::get_or_create(&self.pool, &req.organization_id).await.map_err(Status::from)?;
        Ok(Response::new(CreditBalance {
            organization_id: account.organization_id,
            balance_cents: account.balance_cents,
            monthly_spend_cap_cents: account.monthly_spend_cap_cents.unwrap_or(0),
        }))
    }

    async fn preflight_credit_check(
        &self,
        request: Request<PreflightCreditCheckRequest>,
    ) -> Result<Response<PreflightCreditCheckResponse>, Status> {
        let req = request.into_inner();
        if req.organization_id.is_empty() {
            return Err(Status::invalid_argument("organization_id is required"));
        }

        let required_cents = (req.hourly_rate_cents as f64 * req.min_hours_required).ceil() as i64;
        let account = credits_db::get_or_create(&self.pool, &req.organization_id).await.map_err(Status::from)?;

        Ok(Response::new(PreflightCreditCheckResponse {
            sufficient: account.balance_cents >= required_cents,
            balance_cents: account.balance_cents,
        }))
    }
}

/// Not a real UUID -- unique enough to key an `external_reference` that has
/// no more meaningful id to use (a credit top-up isn't itself a row with an
/// id until after the PaymentIntent exists). `uuid` isn't a dependency of
/// this crate; this avoids adding one for a single call site.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!("topup-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use crate::config::{Config, Secrets};

    fn sub(status: &str) -> sub_db::SubscriptionRow {
        sub_db::SubscriptionRow {
            id: 1,
            organization_id: "o".into(),
            storefront: "developer".into(),
            plan_code: "dev_pro".into(),
            status: status.into(),
            current_period_start: 0,
            current_period_end: 0,
            pending_plan_code: None,
            cancel_at_period_end: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_canceled_subscription_never_becomes_the_orgs_overall_status() {
        // Canceled ties with Active on severity; left in, it could win and
        // make domain_management refuse an org that merely stopped a product.
        for order in [["active", "canceled"], ["canceled", "active"]] {
            let subs: Vec<_> = order.iter().map(|s| sub(s)).collect();
            assert_eq!(most_severe_status(&subs), "active");
        }
        assert_eq!(most_severe_status(&[sub("canceled")]), "active");
        assert_eq!(most_severe_status(&[sub("canceled"), sub("past_due")]), "past_due");
    }

    fn sub_in(storefront: &str, status: &str) -> sub_db::SubscriptionRow {
        let mut row = sub(status);
        row.storefront = storefront.into();
        row
    }

    #[test]
    fn having_no_plan_in_the_storefront_is_not_entitled_even_though_the_overall_status_is_active() {
        let none = entitlement_for(&[], "developer", &HashSet::new());
        assert!(!none.entitled);
        assert!(none.reason.contains("choose a plan"), "{}", none.reason);
        // A plan in a different storefront does not count.
        assert!(!entitlement_for(&[sub_in("email", "active")], "developer", &HashSet::new()).entitled);
    }

    #[test]
    fn only_active_and_past_due_plans_are_entitled() {
        for ok in ["active", "past_due"] {
            let e = entitlement_for(&[sub_in("developer", ok)], "developer", &HashSet::new());
            assert!(e.entitled, "{ok}");
            assert_eq!(e.plan_code, "dev_pro");
            assert!(e.reason.is_empty());
        }
        for bad in ["grace_period", "suspended", "deleted", "canceled", "something_new"] {
            let e = entitlement_for(&[sub_in("developer", bad)], "developer", &HashSet::new());
            assert!(!e.entitled, "{bad}");
            assert!(e.reason.contains("good standing"), "{}", e.reason);
        }
    }

    #[test]
    fn a_good_plan_wins_over_a_bad_one_in_the_same_storefront() {
        let rows = [sub_in("developer", "suspended"), sub_in("developer", "active")];
        assert!(entitlement_for(&rows, "developer", &HashSet::new()).entitled);
    }

    fn unique_org() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!("test-org-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
    }

    /// A `Billing` that never actually dials ais_auth or Stripe -- safe to
    /// construct in a test as long as the handler under test doesn't call
    /// `self.auth.validate` or hit a code path where `total_cents > 0`
    /// (which would try a real Stripe HTTP call). Exactly the internal-only
    /// RPCs (`record_overage_usage` with a $0 plan, `debit_credit`,
    /// `preflight_credit_check`) qualify -- see this module's own doc
    /// comment on why the GPU credit RPCs are tested even though nothing
    /// calls them yet.
    async fn test_billing(pool: sqlx::MySqlPool) -> Billing {
        let secrets = Secrets {
            database_url: String::new(),
            stripe_secret_key: String::new(),
            stripe_webhook_secret: String::new(),
            stripe_publishable_key: String::new(),
        };
        Billing::new(Config::default(), secrets, pool).expect("construct Billing")
    }

    /// Requires a real, migrated database (`DATABASE_URL`) -- `cargo test --
    /// --ignored`, matching every other DB integration test in this crate.
    #[tokio::test]
    #[ignore]
    async fn record_overage_usage_within_allowance_finalizes_a_paid_zero_or_base_price_invoice() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let billing = test_billing(pool.clone()).await;
        let org = unique_org();

        // dev_beta is a $0 plan -- staying within its allowance means
        // total_cents = 0 + 0 overage = 0, so this never reaches the Stripe
        // call in `finalize_invoice_and_maybe_charge`.
        sub_db::insert(&pool, &org, "developer", "dev_beta", BillingStatus::Active.as_str_name(), now(), now() + 1000)
            .await
            .expect("seed subscription");

        let req = Request::new(RecordOverageUsageRequest {
            organization_id: org.clone(),
            storefront: "developer".to_owned(),
            period_start: now(),
            period_end: now() + 1000,
            usage: vec![
                UnitUsage { unit_code: "ram_gb_month".to_owned(), quantity: 0.1 }, // dev_beta's allowance is 0.5
                UnitUsage { unit_code: "vcpu_month".to_owned(), quantity: 0.1 },   // allowance is 0.25
            ],
        });

        let invoice = billing.record_overage_usage(req).await.expect("record_overage_usage").into_inner();
        assert_eq!(invoice.organization_id, org);
        assert_eq!(invoice.total_cents, 0);
        assert_eq!(invoice.status, "paid", "a $0 invoice must finalize straight to paid, never sit open awaiting nothing");
        assert_eq!(invoice.line_items.len(), 1, "just the plan's own $0 base-price line -- no overage line items");
    }

    #[tokio::test]
    #[ignore]
    async fn record_overage_usage_refuses_an_organization_with_no_subscription() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let billing = test_billing(pool).await;

        let req = Request::new(RecordOverageUsageRequest {
            organization_id: unique_org(),
            storefront: "developer".to_owned(),
            period_start: now(),
            period_end: now() + 1000,
            usage: vec![],
        });

        let status = billing.record_overage_usage(req).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    #[ignore]
    async fn debit_credit_through_the_real_handler_applies_and_reports_the_new_balance() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let billing = test_billing(pool.clone()).await;
        let org = unique_org();

        credits_db::apply_ledger_entry(&pool, &org, "topup", 5000, None, None).await.expect("seed a balance");

        let req = Request::new(DebitCreditRequest {
            organization_id: org.clone(),
            amount_cents: 750,
            external_reference: "session-abc".to_owned(),
            idempotency_key: "session-abc:1".to_owned(),
        });
        let balance = billing.debit_credit(req).await.expect("debit_credit").into_inner();
        assert_eq!(balance.balance_cents, 4250);

        // A retried call with the same idempotency key must not double-debit.
        let req = Request::new(DebitCreditRequest {
            organization_id: org.clone(),
            amount_cents: 750,
            external_reference: "session-abc".to_owned(),
            idempotency_key: "session-abc:1".to_owned(),
        });
        let balance = billing.debit_credit(req).await.expect("retried debit_credit").into_inner();
        assert_eq!(balance.balance_cents, 4250, "retried debit must not apply twice");
    }

    #[tokio::test]
    #[ignore]
    async fn debit_credit_rejects_a_non_positive_amount() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let billing = test_billing(pool).await;

        let req = Request::new(DebitCreditRequest {
            organization_id: unique_org(),
            amount_cents: 0,
            external_reference: String::new(),
            idempotency_key: String::new(),
        });
        let status = billing.debit_credit(req).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    #[ignore]
    async fn preflight_credit_check_reflects_real_sufficiency() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let billing = test_billing(pool.clone()).await;
        let org = unique_org();

        credits_db::apply_ledger_entry(&pool, &org, "topup", 100, None, None).await.expect("seed a tiny balance");

        // $0.60/hr (a 4090's marked-up rate) * 1 hour = 60 cents required; only 100 cents on hand -- sufficient.
        let req = Request::new(PreflightCreditCheckRequest {
            organization_id: org.clone(),
            hourly_rate_cents: 60,
            min_hours_required: 1.0,
        });
        let resp = billing.preflight_credit_check(req).await.expect("preflight_credit_check").into_inner();
        assert!(resp.sufficient);
        assert_eq!(resp.balance_cents, 100);

        // Same balance, a GPU that costs more than the whole balance covers for 1 hour -- insufficient.
        let req = Request::new(PreflightCreditCheckRequest {
            organization_id: org,
            hourly_rate_cents: 260, // an A100 SXM 80GB's marked-up rate
            min_hours_required: 1.0,
        });
        let resp = billing.preflight_credit_check(req).await.expect("preflight_credit_check").into_inner();
        assert!(!resp.sufficient);
    }
}

#[cfg(test)]
mod derived_tests {
    use super::*;
    use std::collections::HashSet;

    fn sub_with_id(id: u64, status: &str) -> sub_db::SubscriptionRow {
        sub_db::SubscriptionRow {
            id,
            organization_id: "org".into(),
            storefront: "developer".into(),
            plan_code: "dev_pro".into(),
            status: status.into(),
            current_period_start: 0,
            current_period_end: 0,
            pending_plan_code: None,
            cancel_at_period_end: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn past_due_without_paid_invoice_is_not_entitled() {
        let sub = sub_with_id(1, "past_due");
        let subs = vec![sub];
        let paid = HashSet::new();
        let e = entitlement_for(&subs, "developer", &paid);
        assert!(!e.entitled);
        assert_eq!(e.reason, "waiting for your first payment");
    }

    #[test]
    fn past_due_with_paid_invoice_is_entitled() {
        let sub = sub_with_id(2, "past_due");
        let subs = vec![sub];
        let mut paid = HashSet::new();
        paid.insert(2);
        let e = entitlement_for(&subs, "developer", &paid);
        assert!(e.entitled);
        assert!(e.reason.is_empty());
    }

    #[test]
    fn active_is_always_entitled() {
        let sub = sub_with_id(3, "active");
        let subs = vec![sub];
        let paid = HashSet::new();
        let e = entitlement_for(&subs, "developer", &paid);
        assert!(e.entitled);
    }

    #[test]
    fn canceled_is_not_entitled() {
        let sub = sub_with_id(4, "canceled");
        let subs = vec![sub];
        let paid = HashSet::new();
        let e = entitlement_for(&subs, "developer", &paid);
        assert!(!e.entitled);
    }
}
