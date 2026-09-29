//! `BillingService` implementation.
//!
//! No `access_token`/`elevated_token` field appears on any request in
//! `proto/billing.proto`, and no handler below calls out to `ais_auth`.
//! This is deliberate, not an oversight: Billing is an internal-only
//! service reached over mTLS, and the caller (domain_management today,
//! whichever other system leverages this next) has already validated the
//! end user and enforced its own spending guardrails (`Pricing`,
//! `Purchasing`, whatever the equivalent is for a future consumer) before
//! ever reaching here. Billing's job is "talk to Stripe safely on behalf
//! of an already-authenticated internal caller," not "re-derive who the
//! end user is." The `consumer` field on every request is that caller's
//! own name for itself, used only to key rows in `payment_intents` and to
//! stamp Stripe's metadata for a human to trace a charge by later -- it is
//! never used as an authorization decision, since nothing here verifies
//! that the mTLS-authenticated process is honest about it beyond mTLS
//! itself already proving *which* internal service is calling.

use std::time::Duration;
use tonic::{Request, Response, Status};

use crate::auth::AuthClient;
use crate::config::{Config, Secrets};
use crate::db::credits as credits_db;
use crate::db::customers as customers_db;
use crate::db::payment_intents as pi_db;
use crate::proto::billing::*;
use crate::proto::billing::billing_service_server::BillingService;
use crate::stripe::StripeClient;

/// Backs both `BillingService` (this file) and `BillingAdminService`
/// (`grpc::admin_service`) -- one struct, two tonic services registered
/// against the same gRPC server in `grpc::serve`. `Clone` so each
/// `add_service` call can hold its own handle; every field is itself cheap
/// to clone (a connection pool, a channel, plain config data).
#[derive(Clone)]
pub struct Billing {
    pub(crate) config: Config,
    pub(crate) secrets: Secrets,
    pub(crate) pool: sqlx::MySqlPool,
    pub(crate) stripe: StripeClient,
    pub(crate) auth: AuthClient,
}

impl Billing {
    pub fn new(config: Config, secrets: Secrets, pool: sqlx::MySqlPool) -> Result<Self, crate::error::Error> {
        let stripe = StripeClient::new(&secrets.stripe_secret_key)?;
        let auth = AuthClient::new(&config.auth.grpc_addr)?;
        Ok(Self { config, secrets, pool, stripe, auth })
    }
}

/// A raw Stripe status string to this service's own proto enum. Anything
/// unrecognised maps to `UNSPECIFIED` rather than panicking -- Stripe is
/// allowed to grow a status this build has not heard of.
fn status_code(status: &str) -> i32 {
    match status {
        "requires_payment_method" => PaymentIntentStatus::RequiresPaymentMethod as i32,
        "requires_confirmation" => PaymentIntentStatus::RequiresConfirmation as i32,
        "requires_action" => PaymentIntentStatus::RequiresAction as i32,
        "processing" => PaymentIntentStatus::Processing as i32,
        "requires_capture" => PaymentIntentStatus::RequiresCapture as i32,
        "canceled" => PaymentIntentStatus::Canceled as i32,
        "succeeded" => PaymentIntentStatus::Succeeded as i32,
        _ => PaymentIntentStatus::Unspecified as i32,
    }
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "canceled")
}

fn row_response(row: pi_db::PaymentIntentRow, client_secret: Option<String>) -> PaymentIntent {
    PaymentIntent {
        id: row.id.to_string(),
        stripe_payment_intent_id: row.stripe_payment_intent_id,
        consumer: row.consumer,
        external_reference: row.external_reference,
        amount_cents: row.amount_cents,
        currency: row.currency,
        status: status_code(&row.status),
        // Never persisted -- see this module's own doc comment and
        // `proto/billing.proto`'s `PaymentIntent.client_secret` comment.
        // `None` on every path except the one that just created it.
        client_secret: client_secret.unwrap_or_default(),
        last_error: row.last_error.unwrap_or_default(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        // Same rule as client_secret: only CreatePaymentIntent's own
        // response ever carries this.
        publishable_key: String::new(),
    }
}

#[tonic::async_trait]
impl BillingService for Billing {
    /// Idempotent per `(consumer, external_reference)`: an existing row
    /// for that pair is returned as-is, no new Stripe call made, so a
    /// retried request from the caller's side is always safe.
    async fn create_payment_intent(
        &self,
        request: Request<CreatePaymentIntentRequest>,
    ) -> Result<Response<PaymentIntent>, Status> {
        let req = request.into_inner();
        if req.consumer.is_empty() {
            return Err(Status::invalid_argument("consumer is required"));
        }
        if req.external_reference.is_empty() {
            return Err(Status::invalid_argument("external_reference is required"));
        }
        if req.amount_cents <= 0 {
            return Err(Status::invalid_argument("amount_cents must be a positive number of minor units"));
        }

        if let Some(existing) =
            pi_db::find_by_consumer_reference(&self.pool, &req.consumer, &req.external_reference)
                .await
                .map_err(Status::from)?
        {
            return Ok(Response::new(row_response(existing, None)));
        }

        let currency = if req.currency.is_empty() { "usd".to_owned() } else { req.currency.to_lowercase() };

        // Deterministic from (consumer, external_reference): a retried
        // HTTP call to Stripe itself (this client timing out after Stripe
        // already committed) returns the same PaymentIntent rather than a
        // second charge. See `StripeClient::create_payment_intent`'s own
        // doc comment for how this differs from the DB-level uniqueness
        // above -- the two guard against different retries.
        let idempotency_key = format!("{}:{}", req.consumer, req.external_reference);

        let mut metadata: Vec<(&str, &str)> =
            vec![("consumer", req.consumer.as_str()), ("external_reference", req.external_reference.as_str())];
        for (key, value) in &req.metadata {
            metadata.push((key.as_str(), value.as_str()));
        }

        let stripe_pi = self
            .stripe
            .create_payment_intent(
                req.amount_cents,
                &currency,
                &metadata,
                Some(req.stripe_customer_id.as_str()).filter(|c| !c.is_empty()),
                req.save_payment_method,
                &idempotency_key,
            )
            .await
            .map_err(Status::from)?;

        let id = pi_db::insert(
            &self.pool,
            &stripe_pi.id,
            &req.consumer,
            &req.external_reference,
            req.amount_cents,
            &currency,
            &stripe_pi.status,
        )
        .await
        .map_err(Status::from)?;

        Ok(Response::new(PaymentIntent {
            id: id.to_string(),
            stripe_payment_intent_id: stripe_pi.id,
            consumer: req.consumer,
            external_reference: req.external_reference,
            amount_cents: req.amount_cents,
            currency,
            status: status_code(&stripe_pi.status),
            client_secret: stripe_pi.client_secret.unwrap_or_default(),
            last_error: String::new(),
            created_at: chrono::Utc::now().timestamp(),
            updated_at: chrono::Utc::now().timestamp(),
            publishable_key: self.secrets.stripe_publishable_key.clone(),
        }))
    }

    async fn get_payment_intent(
        &self,
        request: Request<GetPaymentIntentRequest>,
    ) -> Result<Response<PaymentIntent>, Status> {
        let req = request.into_inner();
        let row = pi_db::find(&self.pool, &req.id_or_reference)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no payment intent {}", req.id_or_reference)))?;

        Ok(Response::new(row_response(row, None)))
    }

    type WatchPaymentIntentStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<PaymentIntent, Status>> + Send + 'static>>;

    /// Polls the database (not Stripe -- the webhook handler and
    /// `GetPaymentIntent` callers are what keep the row fresh) and yields
    /// a value only when `status` changes, ending the stream once a
    /// terminal status (`succeeded`/`canceled`) is reached. Mirrors the
    /// shape `domain_management`'s own `WatchDomain` RPC uses for the same
    /// reason: a caller that wants to know the moment a charge resolves
    /// without polling `GetPaymentIntent` in its own loop.
    async fn watch_payment_intent(
        &self,
        request: Request<WatchPaymentIntentRequest>,
    ) -> Result<Response<Self::WatchPaymentIntentStream>, Status> {
        let req = request.into_inner();
        let pool = self.pool.clone();

        if pi_db::find(&pool, &req.id_or_reference).await.map_err(Status::from)?.is_none() {
            return Err(Status::not_found(format!("no payment intent {}", req.id_or_reference)));
        }

        let stream = async_stream::try_stream! {
            let mut last_status: Option<String> = None;
            loop {
                let Some(row) = pi_db::find(&pool, &req.id_or_reference).await.map_err(Status::from)? else {
                    // The row existed a moment ago; disappearing mid-watch
                    // is not expected (nothing deletes rows), but ending
                    // the stream cleanly beats looping forever on nothing.
                    return;
                };

                if last_status.as_deref() != Some(row.status.as_str()) {
                    last_status = Some(row.status.clone());
                    let terminal = is_terminal(&row.status);
                    yield row_response(row, None);
                    if terminal {
                        return;
                    }
                }

                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };

        Ok(Response::new(Box::pin(stream)))
    }

    async fn cancel_payment_intent(
        &self,
        request: Request<CancelPaymentIntentRequest>,
    ) -> Result<Response<PaymentIntent>, Status> {
        let req = request.into_inner();
        let row = pi_db::find(&self.pool, &req.id_or_reference)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("no payment intent {}", req.id_or_reference)))?;

        if is_terminal(&row.status) {
            return Err(Status::failed_precondition(format!(
                "payment intent {} is already {} and cannot be cancelled",
                req.id_or_reference, row.status
            )));
        }

        let stripe_pi = self.stripe.cancel_payment_intent(&row.stripe_payment_intent_id).await.map_err(Status::from)?;
        pi_db::update_status(&self.pool, &row.stripe_payment_intent_id, &stripe_pi.status, None)
            .await
            .map_err(Status::from)?;

        let updated = pi_db::find_by_stripe_id(&self.pool, &row.stripe_payment_intent_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("payment intent vanished mid-cancel"))?;

        Ok(Response::new(row_response(updated, None)))
    }

    /// AUTHZ note, since this is the one RPC in this service that isn't
    /// mTLS-authenticated at all: the caller is Stripe, not an internal
    /// service, and the only thing that authenticates it is an HMAC of
    /// the raw request body against `secrets.stripe_webhook_secret`
    /// (`crate::stripe::webhook::verify_signature`), with the timestamp
    /// checked for replay. Whatever relays this (Portal, or any public
    /// ingress) forwards the body and `Stripe-Signature` header
    /// untouched precisely so the signature still verifies here.
    async fn handle_stripe_webhook(
        &self,
        request: Request<StripeWebhookRequest>,
    ) -> Result<Response<StripeWebhookResponse>, Status> {
        let req = request.into_inner();

        crate::stripe::webhook::verify_signature(&req.payload, &req.signature, &self.secrets.stripe_webhook_secret, 300)
            .map_err(Status::from)?;

        let event: serde_json::Value = serde_json::from_slice(&req.payload)
            .map_err(|e| Status::invalid_argument(format!("undecodable webhook payload: {e}")))?;

        let handled = self.process_event(&event).await?;
        Ok(Response::new(StripeWebhookResponse { handled }))
    }
}

impl Billing {
    /// The organization's Stripe Customer, creating it (once) if it doesn't
    /// exist yet. Returns the row and whether this call created it.
    ///
    /// Idempotent at two levels, guarding different retries: the database
    /// row (a repeat call finds it and makes no Stripe call), and Stripe's
    /// own idempotency key `customer:<org>` (this client retrying after a
    /// timeout returns the original Customer rather than a duplicate).
    /// Two truly concurrent first calls both reach Stripe with the same key,
    /// get the same Customer back, and the insert is a no-op for the loser.
    pub(crate) async fn ensure_customer(
        &self,
        organization_id: &str,
        name: Option<&str>,
        email: Option<&str>,
    ) -> Result<(crate::db::customers::CustomerRow, bool), Status> {
        if organization_id.is_empty() {
            return Err(Status::invalid_argument("organization_id is required"));
        }
        if let Some(existing) = customers_db::find(&self.pool, organization_id).await.map_err(Status::from)? {
            return Ok((existing, false));
        }

        let customer = self
            .stripe
            .create_customer(
                name.filter(|v| !v.is_empty()),
                email.filter(|v| !v.is_empty()),
                &[("organization_id", organization_id)],
                &format!("customer:{organization_id}"),
            )
            .await
            .map_err(Status::from)?;
        customers_db::insert(&self.pool, organization_id, &customer.id).await.map_err(Status::from)?;

        let row = customers_db::find(&self.pool, organization_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::internal("billing customer vanished mid-create"))?;
        Ok((row, true))
    }

    /// Acts on one signature-verified Stripe event. Split from
    /// [`BillingService::handle_stripe_webhook`] so it can be tested with
    /// crafted events, without having to sign them.
    ///
    /// Every event is recorded in the `stripe_events` inbox first. One already
    /// marked processed is acknowledged without acting again (`Ok(true)`:
    /// Stripe redelivers, and a duplicate must not be an error or Stripe
    /// keeps retrying). An event that errors part-way is left unprocessed so
    /// Stripe's retry re-runs it -- safe because every effect below is
    /// idempotent (a status write to the same value; a ledger entry with a
    /// unique idempotency key).
    ///
    /// Returns whether the event was one this service acts on (`false` for an
    /// event type it ignores, or a PaymentIntent it never created).
    pub(crate) async fn process_event(&self, event: &serde_json::Value) -> Result<bool, Status> {
        let event_id = event.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        if event_id.is_empty() {
            return Err(Status::invalid_argument("webhook event has no id"));
        }

        if crate::db::stripe_events::begin(&self.pool, event_id, event_type).await.map_err(Status::from)? {
            return Ok(true);
        }

        let object = event.pointer("/data/object").cloned().unwrap_or_default();
        let handled = match event_type {
            "payment_intent.succeeded"
            | "payment_intent.payment_failed"
            | "payment_intent.canceled"
            | "payment_intent.processing" => self.on_payment_intent_event(event_type, &object).await?,
            "charge.refunded" => self.on_charge_refunded(&object).await?,
            "charge.dispute.created" => self.on_dispute_created(&object).await?,
            "charge.dispute.closed" => self.on_dispute_closed(&object).await?,
            // Not every Stripe event concerns this service. Acknowledging
            // rather than erroring keeps Stripe from retrying an event we
            // were never going to act on.
            _ => false,
        };

        crate::db::stripe_events::mark_processed(&self.pool, event_id).await.map_err(Status::from)?;
        Ok(handled)
    }

    async fn on_payment_intent_event(&self, event_type: &str, object: &serde_json::Value) -> Result<bool, Status> {
        let payment_intent_id = object.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        if payment_intent_id.is_empty() {
            return Ok(false);
        }

        let status = match event_type {
            "payment_intent.succeeded" => "succeeded",
            "payment_intent.payment_failed" => "requires_payment_method",
            "payment_intent.canceled" => "canceled",
            _ => "processing",
        };

        let last_error = object.pointer("/last_payment_error/message").and_then(|v| v.as_str()).map(str::to_owned);

        let updated = pi_db::update_status(&self.pool, payment_intent_id, status, last_error.as_deref())
            .await
            .map_err(Status::from)?;

        if status == "succeeded" {
            // "Money isn't real until Stripe says so" (see the doc comment on
            // `TopUpCredit` in the proto): a subscription invoice tied to this
            // PaymentIntent only becomes `paid`, and only then flips its
            // subscription from `PastDue` to `Active`, once Stripe confirms
            // the charge actually succeeded here -- never optimistically at
            // `CreateOrUpgradeSubscription` time. Not every PaymentIntent is
            // tied to a subscription invoice, so a lookup miss is expected.
            if let Some(invoice) =
                crate::db::invoices::mark_paid_by_payment_intent(&self.pool, payment_intent_id).await.map_err(Status::from)?
            {
                crate::db::subscriptions::set_status_for_invoice(
                    &self.pool,
                    invoice.id,
                    crate::domain::BillingStatus::Active.as_str_name(),
                )
                .await
                .map_err(Status::from)?;
            }

            self.credit_topup_if_any(payment_intent_id, object).await?;

            // A card saved during this payment (`save_payment_method`) becomes
            // the customer's default for off-session use, if they have none.
            if let (Some(customer), Some(payment_method)) = (
                object.get("customer").and_then(|v| v.as_str()).filter(|v| !v.is_empty()),
                object.get("payment_method").and_then(|v| v.as_str()).filter(|v| !v.is_empty()),
            ) {
                if object.get("setup_future_usage").and_then(|v| v.as_str()).is_some() {
                    customers_db::set_default_payment_method_if_none(&self.pool, customer, payment_method)
                        .await
                        .map_err(Status::from)?;
                }
            }
        }

        Ok(updated)
    }

    /// If this PaymentIntent funded a GPU credit top-up (`TopUpCredit`), adds
    /// the credit to the org's ledger. Exactly-once: the ledger entry's
    /// idempotency key is derived from the PaymentIntent id, so Stripe
    /// redelivering `payment_intent.succeeded` (or a crash and retry between
    /// the status write and here) cannot credit twice.
    async fn credit_topup_if_any(&self, payment_intent_id: &str, object: &serde_json::Value) -> Result<(), Status> {
        let Some(row) = pi_db::find_by_stripe_id(&self.pool, payment_intent_id).await.map_err(Status::from)? else {
            return Ok(());
        };
        if row.consumer != crate::credit::TOPUP_CONSUMER {
            return Ok(());
        }

        // The organization comes from the metadata this service stamped on
        // the PaymentIntent at creation. A top-up without one can't be
        // credited to anyone: fail loudly (Stripe will retry and the error
        // is visible) rather than silently swallowing a customer's payment.
        let organization_id = object
            .pointer("/metadata/organization_id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Status::internal(format!("credit top-up {payment_intent_id} has no organization_id metadata")))?;

        // What Stripe actually collected must match what this service asked
        // for; a mismatch means something other than TopUpCredit touched it.
        let collected = object.get("amount_received").and_then(|v| v.as_i64()).or_else(|| object.get("amount").and_then(|v| v.as_i64()));
        if collected != Some(row.amount_cents) {
            return Err(Status::failed_precondition(format!(
                "credit top-up {payment_intent_id}: Stripe collected {collected:?} cents but {} were requested",
                row.amount_cents
            )));
        }

        credits_db::apply_ledger_entry(
            &self.pool,
            organization_id,
            "topup",
            crate::credit::cents_to_micros(row.amount_cents),
            Some(payment_intent_id),
            Some(&format!("topup:{payment_intent_id}")),
        )
        .await
        .map_err(Status::from)?;
        Ok(())
    }

    /// `charge.refunded`: reverses the refunded part of a credit top-up.
    /// Stripe reports a running total (`amount_refunded`), so this brings the
    /// ledger's reversals up to that total (see `credits::reverse_to_cumulative`).
    /// Refunds of anything that isn't a credited top-up (a subscription
    /// invoice) are ignored here.
    async fn on_charge_refunded(&self, charge: &serde_json::Value) -> Result<bool, Status> {
        let Some(payment_intent_id) = charge.get("payment_intent").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) else {
            return Ok(false);
        };
        let Some((organization_id, topup_micros)) =
            credits_db::find_topup_for_payment_intent(&self.pool, payment_intent_id).await.map_err(Status::from)?
        else {
            return Ok(false);
        };

        let refunded_cents = charge.get("amount_refunded").and_then(|v| v.as_i64()).unwrap_or(0);
        // Never reverse more than the top-up itself put in.
        let cumulative = crate::credit::cents_to_micros(refunded_cents).min(topup_micros);
        credits_db::reverse_to_cumulative(&self.pool, &organization_id, payment_intent_id, "refund", cumulative)
            .await
            .map_err(Status::from)?;
        Ok(true)
    }

    /// `charge.dispute.created`: claws the disputed amount back out of the
    /// balance immediately, since the funds are withdrawn from us while the
    /// dispute is open. Reinstated by [`Self::on_dispute_closed`] if we win.
    async fn on_dispute_created(&self, dispute: &serde_json::Value) -> Result<bool, Status> {
        let Some((organization_id, payment_intent_id, dispute_id, micros)) = self.dispute_target(dispute).await? else {
            return Ok(false);
        };
        credits_db::apply_ledger_entry(
            &self.pool,
            &organization_id,
            "adjustment",
            -micros,
            Some(&payment_intent_id),
            Some(&format!("dispute:{dispute_id}")),
        )
        .await
        .map_err(Status::from)?;
        Ok(true)
    }

    /// `charge.dispute.closed`: a `won` dispute gives the credit back; a
    /// `lost` one leaves the clawback in place. Only reinstates a dispute we
    /// actually clawed back (its `dispute:<id>` entry exists).
    async fn on_dispute_closed(&self, dispute: &serde_json::Value) -> Result<bool, Status> {
        if dispute.get("status").and_then(|v| v.as_str()) != Some("won") {
            return Ok(false);
        }
        let Some((organization_id, payment_intent_id, dispute_id, micros)) = self.dispute_target(dispute).await? else {
            return Ok(false);
        };
        if !credits_db::has_entry(&self.pool, &organization_id, &format!("dispute:{dispute_id}")).await.map_err(Status::from)? {
            return Ok(false);
        }
        credits_db::apply_ledger_entry(
            &self.pool,
            &organization_id,
            "adjustment",
            micros,
            Some(&payment_intent_id),
            Some(&format!("dispute_won:{dispute_id}")),
        )
        .await
        .map_err(Status::from)?;
        Ok(true)
    }

    /// `(organization_id, payment_intent_id, dispute_id, micros)` for a
    /// dispute against a credited top-up, `None` if it concerns anything else.
    async fn dispute_target(&self, dispute: &serde_json::Value) -> Result<Option<(String, String, String, i64)>, Status> {
        let dispute_id = dispute.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let payment_intent_id = dispute.get("payment_intent").and_then(|v| v.as_str()).unwrap_or_default();
        if dispute_id.is_empty() || payment_intent_id.is_empty() {
            return Ok(None);
        }
        let Some((organization_id, topup_micros)) =
            credits_db::find_topup_for_payment_intent(&self.pool, payment_intent_id).await.map_err(Status::from)?
        else {
            return Ok(None);
        };
        let disputed_cents = dispute.get("amount").and_then(|v| v.as_i64()).unwrap_or(0);
        let micros = crate::credit::cents_to_micros(disputed_cents).min(topup_micros);
        if micros <= 0 {
            return Ok(None);
        }
        Ok(Some((organization_id, payment_intent_id.to_owned(), dispute_id.to_owned(), micros)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_billing() -> Billing {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let secrets = Secrets {
            database_url: String::new(),
            stripe_secret_key: String::new(),
            stripe_webhook_secret: String::new(),
            stripe_publishable_key: String::new(),
        };
        // Plaintext so construction needs no mTLS cert files; never dialed.
        let mut config = Config::default();
        config.auth.grpc_addr = "http://127.0.0.1:50051".to_owned();
        Billing::new(config, secrets, pool).expect("construct Billing")
    }

    fn unique(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!("{prefix}_{nanos}_{}", N.fetch_add(1, Ordering::Relaxed))
    }

    /// Seeds the local row `TopUpCredit` would have written, returning the
    /// Stripe PaymentIntent id.
    async fn seed_topup(billing: &Billing, cents: i64) -> String {
        let pi = unique("pi");
        pi_db::insert(&billing.pool, &pi, crate::credit::TOPUP_CONSUMER, &unique("topup"), cents, "usd", "requires_payment_method")
            .await
            .expect("seed payment_intents row");
        pi
    }

    fn pi_succeeded(event_id: &str, pi: &str, org: &str, cents: i64) -> serde_json::Value {
        json!({
            "id": event_id, "type": "payment_intent.succeeded",
            "data": {"object": {"id": pi, "amount": cents, "amount_received": cents, "metadata": {"organization_id": org}}}
        })
    }

    fn charge_refunded(event_id: &str, pi: &str, refunded_cents: i64) -> serde_json::Value {
        json!({
            "id": event_id, "type": "charge.refunded",
            "data": {"object": {"id": "ch_1", "payment_intent": pi, "amount_refunded": refunded_cents}}
        })
    }

    fn dispute(event_id: &str, event_type: &str, dispute_id: &str, pi: &str, cents: i64, status: &str) -> serde_json::Value {
        json!({
            "id": event_id, "type": event_type,
            "data": {"object": {"id": dispute_id, "payment_intent": pi, "amount": cents, "status": status}}
        })
    }

    async fn balance(billing: &Billing, org: &str) -> i64 {
        credits_db::get_or_create(&billing.pool, org).await.expect("balance").balance_micros
    }

    #[tokio::test]
    #[ignore]
    async fn a_succeeded_topup_credits_the_ledger_exactly_once() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 2_500).await;

        let event = pi_succeeded(&unique("evt"), &pi, &org, 2_500);
        assert!(billing.process_event(&event).await.unwrap());
        assert_eq!(balance(&billing, &org).await, 25_000_000, "$25.00 == 25,000,000 micros");

        // Stripe redelivering the very same event.
        assert!(billing.process_event(&event).await.unwrap());
        assert_eq!(balance(&billing, &org).await, 25_000_000, "duplicate delivery must not credit again");

        // A *different* event id for the same PaymentIntent (defence in
        // depth: the ledger's own idempotency key must also hold).
        let other = pi_succeeded(&unique("evt"), &pi, &org, 2_500);
        billing.process_event(&other).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 25_000_000, "same PaymentIntent must never credit twice");
    }

    #[tokio::test]
    #[ignore]
    async fn a_failed_or_processing_payment_credits_nothing() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 2_500).await;

        for event_type in ["payment_intent.payment_failed", "payment_intent.processing", "payment_intent.canceled"] {
            let event = json!({
                "id": unique("evt"), "type": event_type,
                "data": {"object": {"id": pi, "amount": 2500, "metadata": {"organization_id": org}}}
            });
            billing.process_event(&event).await.unwrap();
        }
        assert_eq!(balance(&billing, &org).await, 0);
    }

    #[tokio::test]
    #[ignore]
    async fn a_topup_whose_collected_amount_differs_is_refused_and_not_credited() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 2_500).await;

        let event = pi_succeeded(&unique("evt"), &pi, &org, 9_999);
        let status = billing.process_event(&event).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(balance(&billing, &org).await, 0);
    }

    #[tokio::test]
    #[ignore]
    async fn a_topup_without_organization_metadata_errors_so_stripe_retries() {
        let billing = test_billing().await;
        let pi = seed_topup(&billing, 2_500).await;
        let event = json!({
            "id": unique("evt"), "type": "payment_intent.succeeded",
            "data": {"object": {"id": pi, "amount": 2500, "amount_received": 2500, "metadata": {}}}
        });
        let status = billing.process_event(&event).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Internal);
    }

    #[tokio::test]
    #[ignore]
    async fn a_failed_event_stays_unprocessed_and_succeeds_on_retry() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 2_500).await;
        let event_id = unique("evt");

        // First delivery is missing the org metadata -> errors, event not marked processed.
        let broken = json!({
            "id": event_id, "type": "payment_intent.succeeded",
            "data": {"object": {"id": pi, "amount": 2500, "amount_received": 2500, "metadata": {}}}
        });
        assert!(billing.process_event(&broken).await.is_err());

        // Redelivery with the same event id is re-run (not skipped as processed) and now credits.
        let fixed = pi_succeeded(&event_id, &pi, &org, 2_500);
        assert!(billing.process_event(&fixed).await.unwrap());
        assert_eq!(balance(&billing, &org).await, 25_000_000);
    }

    #[tokio::test]
    #[ignore]
    async fn a_payment_that_is_not_a_credit_topup_never_touches_the_ledger() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = unique("pi");
        pi_db::insert(&billing.pool, &pi, "domain_management", &unique("order"), 1_200, "usd", "requires_payment_method")
            .await
            .unwrap();

        billing.process_event(&pi_succeeded(&unique("evt"), &pi, &org, 1_200)).await.unwrap();
        billing.process_event(&charge_refunded(&unique("evt"), &pi, 1_200)).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 0);
    }

    #[tokio::test]
    #[ignore]
    async fn refunds_reverse_the_cumulative_amount_once_even_if_redelivered_or_reordered() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 5_000).await;
        billing.process_event(&pi_succeeded(&unique("evt"), &pi, &org, 5_000)).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 50_000_000);

        // $10 refunded, then $30 in total, then the older "$10 total" event arrives late.
        billing.process_event(&charge_refunded(&unique("evt"), &pi, 1_000)).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 40_000_000);

        let thirty = charge_refunded(&unique("evt"), &pi, 3_000);
        billing.process_event(&thirty).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 20_000_000, "only the $20 difference is reversed");

        billing.process_event(&thirty).await.unwrap();
        billing.process_event(&charge_refunded(&unique("evt"), &pi, 1_000)).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 20_000_000, "duplicate and stale events change nothing");

        // Over-reporting can't reverse more than the top-up put in.
        billing.process_event(&charge_refunded(&unique("evt"), &pi, 9_999_999)).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 0);
    }

    #[tokio::test]
    #[ignore]
    async fn spent_credit_can_leave_the_balance_negative_after_a_refund() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 2_500).await;
        billing.process_event(&pi_succeeded(&unique("evt"), &pi, &org, 2_500)).await.unwrap();
        credits_db::apply_ledger_entry(&billing.pool, &org, "debit", -20_000_000, None, None).await.unwrap();

        billing.process_event(&charge_refunded(&unique("evt"), &pi, 2_500)).await.unwrap();
        assert_eq!(balance(&billing, &org).await, -20_000_000);
    }

    #[tokio::test]
    #[ignore]
    async fn a_dispute_claws_back_and_a_win_reinstates_it_but_a_loss_does_not() {
        let billing = test_billing().await;
        let org = unique("org");

        // Lost: clawback stays.
        let pi = seed_topup(&billing, 2_500).await;
        billing.process_event(&pi_succeeded(&unique("evt"), &pi, &org, 2_500)).await.unwrap();
        let d = unique("dp");
        let created = dispute(&unique("evt"), "charge.dispute.created", &d, &pi, 2_500, "needs_response");
        billing.process_event(&created).await.unwrap();
        billing.process_event(&created).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 0, "clawed back exactly once");
        billing.process_event(&dispute(&unique("evt"), "charge.dispute.closed", &d, &pi, 2_500, "lost")).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 0);

        // Won: reinstated, once.
        let org2 = unique("org");
        let pi2 = seed_topup(&billing, 2_500).await;
        billing.process_event(&pi_succeeded(&unique("evt"), &pi2, &org2, 2_500)).await.unwrap();
        let d2 = unique("dp");
        billing.process_event(&dispute(&unique("evt"), "charge.dispute.created", &d2, &pi2, 2_500, "needs_response")).await.unwrap();
        assert_eq!(balance(&billing, &org2).await, 0);
        let won = dispute(&unique("evt"), "charge.dispute.closed", &d2, &pi2, 2_500, "won");
        billing.process_event(&won).await.unwrap();
        billing.process_event(&won).await.unwrap();
        assert_eq!(balance(&billing, &org2).await, 25_000_000);
    }

    #[tokio::test]
    #[ignore]
    async fn a_won_dispute_we_never_clawed_back_is_not_credited() {
        let billing = test_billing().await;
        let org = unique("org");
        let pi = seed_topup(&billing, 2_500).await;
        billing.process_event(&pi_succeeded(&unique("evt"), &pi, &org, 2_500)).await.unwrap();

        // `created` was never seen (lost/undelivered), only `closed: won`.
        billing.process_event(&dispute(&unique("evt"), "charge.dispute.closed", &unique("dp"), &pi, 2_500, "won")).await.unwrap();
        assert_eq!(balance(&billing, &org).await, 25_000_000, "no phantom credit");
    }

    #[tokio::test]
    #[ignore]
    async fn unrelated_event_types_are_acknowledged_and_recorded_but_not_acted_on() {
        let billing = test_billing().await;
        let event_id = unique("evt");
        let event = json!({"id": event_id, "type": "customer.created", "data": {"object": {"id": "cus_1"}}});
        assert!(!billing.process_event(&event).await.unwrap());
        assert!(crate::db::stripe_events::begin(&billing.pool, &event_id, "customer.created").await.unwrap(), "recorded as processed");
    }

    #[tokio::test]
    #[ignore]
    async fn an_event_with_no_id_is_rejected() {
        let billing = test_billing().await;
        let status = billing.process_event(&json!({"type": "payment_intent.succeeded"})).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    async fn billing_with_mock_stripe(responses: Vec<(u16, String)>) -> (Billing, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (base, requests) = crate::stripe::tests::mock_server(responses).await;
        let mut billing = test_billing().await;
        billing.stripe = StripeClient::with_base("sk_test_123", &base).unwrap();
        (billing, requests)
    }

    #[tokio::test]
    #[ignore]
    async fn ensure_customer_creates_one_stripe_customer_per_org_and_is_idempotent() {
        // Only ONE canned response: a second Stripe call would find nothing listening.
        let (billing, mut requests) = billing_with_mock_stripe(vec![(200, r#"{"id": "cus_new1"}"#.to_owned())]).await;
        let org = unique("org");

        let (row, created) = billing.ensure_customer(&org, Some("Acme Inc"), Some("billing@acme.test")).await.unwrap();
        assert!(created);
        assert_eq!(row.stripe_customer_id, "cus_new1");
        assert_eq!(row.default_payment_method_id, None);

        let request = requests.recv().await.unwrap();
        assert!(request.contains(&format!("idempotency-key: customer:{org}")), "{request}");
        assert!(request.contains(&format!("metadata%5Borganization_id%5D={org}")), "{request}");

        let (again, created_again) = billing.ensure_customer(&org, None, None).await.unwrap();
        assert!(!created_again, "an org that already has a customer is returned unchanged");
        assert_eq!(again.stripe_customer_id, "cus_new1");
    }

    #[tokio::test]
    #[ignore]
    async fn a_stripe_failure_creates_no_local_customer_so_a_retry_can_succeed() {
        let (billing, _requests) = billing_with_mock_stripe(vec![(
            500,
            r#"{"error": {"type": "api_error", "message": "boom"}}"#.to_owned(),
        )])
        .await;
        let org = unique("org");
        assert!(billing.ensure_customer(&org, None, None).await.is_err());
        assert!(customers_db::find(&billing.pool, &org).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn ensure_customer_requires_an_organization_id() {
        let billing = test_billing().await;
        let status = billing.ensure_customer("", None, None).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    #[ignore]
    async fn a_card_saved_during_a_topup_becomes_the_default_payment_method_once() {
        let billing = test_billing().await;
        let (org, cus) = (unique("org"), unique("cus"));
        customers_db::insert(&billing.pool, &org, &cus).await.unwrap();

        let saved = |pm: &str, pi: &str| {
            json!({
                "id": unique("evt"), "type": "payment_intent.succeeded",
                "data": {"object": {
                    "id": pi, "amount": 2500, "amount_received": 2500,
                    "metadata": {"organization_id": org},
                    "customer": cus, "payment_method": pm, "setup_future_usage": "off_session"
                }}
            })
        };
        let pi1 = seed_topup(&billing, 2_500).await;
        billing.process_event(&saved("pm_first", &pi1)).await.unwrap();
        let pi2 = seed_topup(&billing, 2_500).await;
        billing.process_event(&saved("pm_second", &pi2)).await.unwrap();

        let row = customers_db::find(&billing.pool, &org).await.unwrap().unwrap();
        assert_eq!(row.default_payment_method_id.as_deref(), Some("pm_first"), "a later card must not silently replace the default");
        assert_eq!(balance(&billing, &org).await, 50_000_000, "both top-ups still credited");
    }

    #[tokio::test]
    #[ignore]
    async fn a_payment_that_did_not_save_a_card_leaves_the_default_unset() {
        let billing = test_billing().await;
        let (org, cus) = (unique("org"), unique("cus"));
        customers_db::insert(&billing.pool, &org, &cus).await.unwrap();

        let pi = seed_topup(&billing, 2_500).await;
        let event = json!({
            "id": unique("evt"), "type": "payment_intent.succeeded",
            "data": {"object": {
                "id": pi, "amount": 2500, "amount_received": 2500,
                "metadata": {"organization_id": org}, "customer": cus, "payment_method": "pm_x"
            }}
        });
        billing.process_event(&event).await.unwrap();
        assert_eq!(customers_db::find(&billing.pool, &org).await.unwrap().unwrap().default_payment_method_id, None);
    }
}
