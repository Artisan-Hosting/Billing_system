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
use crate::db::payment_intents as pi_db;
use crate::proto::billing::*;
use crate::proto::billing::billing_service_server::BillingService;
use crate::stripe::StripeClient;

/// `payment_intents.consumer` for a credit top-up; the webhook credits the
/// ledger only for PaymentIntents carrying this consumer.
pub(crate) const TOPUP_CONSUMER: &str = "billing_credit_topup";

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
            .create_payment_intent(req.amount_cents, &currency, &metadata, &idempotency_key)
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

        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        let payment_intent_id =
            event.pointer("/data/object/id").and_then(|v| v.as_str()).unwrap_or_default().to_owned();

        if payment_intent_id.is_empty() {
            // Not every Stripe event is about a PaymentIntent; this
            // endpoint only cares about the ones that are. Acknowledging
            // rather than erroring keeps Stripe from retrying an event
            // this service was never going to act on.
            return Ok(Response::new(StripeWebhookResponse { handled: false }));
        }

        let status = match event_type {
            "payment_intent.succeeded" => "succeeded",
            "payment_intent.payment_failed" => "requires_payment_method",
            "payment_intent.canceled" => "canceled",
            "payment_intent.processing" => "processing",
            _ => return Ok(Response::new(StripeWebhookResponse { handled: false })),
        };

        let last_error = event
            .pointer("/data/object/last_payment_error/message")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let updated = pi_db::update_status(&self.pool, &payment_intent_id, status, last_error.as_deref())
            .await
            .map_err(Status::from)?;

        // "Money isn't real until Stripe says so" (see this module's own doc
        // comment on `TopUpCredit` in the proto): a subscription invoice tied
        // to this PaymentIntent only becomes `paid`, and only then flips its
        // subscription from `PastDue` to `Active`, once Stripe confirms the
        // charge actually succeeded here -- never optimistically at
        // `CreateOrUpgradeSubscription` time. Not every PaymentIntent is tied
        // to a subscription invoice (domain orders and future GPU top-ups
        // aren't), so a lookup miss is expected, not an error.
        if status == "succeeded" {
            if let Some(invoice) = crate::db::invoices::mark_paid_by_payment_intent(&self.pool, &payment_intent_id)
                .await
                .map_err(Status::from)?
            {
                crate::db::subscriptions::set_status_for_invoice(
                    &self.pool,
                    invoice.id,
                    crate::domain::BillingStatus::Active.as_str_name(),
                )
                .await
                .map_err(Status::from)?;
            }

            credit_confirmed_topup(&self.pool, &event, &payment_intent_id).await.map_err(Status::from)?;
        }

        Ok(Response::new(StripeWebhookResponse { handled: updated }))
    }
}

/// A credit top-up is only real once Stripe confirms it: this is the one
/// place the ledger gets its `topup` row. The amount comes from our own row
/// (written when we created the PaymentIntent), not from the event body, and
/// the PaymentIntent id is the idempotency key, so Stripe re-delivering the
/// event lands once. Returns whether a ledger entry was newly applied.
pub(crate) async fn credit_confirmed_topup(
    pool: &sqlx::MySqlPool,
    event: &serde_json::Value,
    payment_intent_id: &str,
) -> crate::error::Result<bool> {
    let Some(row) = pi_db::find_by_stripe_id(pool, payment_intent_id).await? else {
        return Ok(false);
    };
    if row.consumer != TOPUP_CONSUMER {
        return Ok(false);
    }
    let org = event
        .pointer("/data/object/metadata/organization_id")
        .and_then(|v| v.as_str())
        .filter(|org| !org.is_empty());
    let Some(org) = org else {
        // Unrecoverable by retrying, so acknowledge rather than have Stripe
        // re-send it for days; the row stays `succeeded` with no ledger entry
        // for reconciliation.
        eprintln!("billing: top-up {payment_intent_id} succeeded but carries no organization_id metadata");
        return Ok(false);
    };
    let (_, applied) = crate::db::credits::apply_ledger_entry(
        pool,
        org,
        "topup",
        row.amount_cents,
        Some(payment_intent_id),
        Some(payment_intent_id),
    )
    .await?;
    Ok(applied)
}

#[cfg(test)]
mod topup_tests {
    use super::*;

    fn event(org: Option<&str>) -> serde_json::Value {
        let mut obj = serde_json::json!({ "id": "pi_topup_test" });
        if let Some(org) = org {
            obj["metadata"] = serde_json::json!({ "organization_id": org });
        }
        serde_json::json!({ "type": "payment_intent.succeeded", "data": { "object": obj } })
    }

    #[tokio::test]
    #[ignore]
    async fn a_confirmed_topup_credits_once_even_when_the_webhook_is_replayed() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let org = format!("test-org-{nonce}");
        let pi = format!("pi_topup_{nonce}");
        let mut ev = event(Some(&org));
        ev["data"]["object"]["id"] = serde_json::json!(pi);

        pi_db::insert(&pool, &pi, TOPUP_CONSUMER, &format!("ref-{nonce}"), 5000, "usd", "requires_payment_method")
            .await
            .expect("seed payment intent");

        assert!(credit_confirmed_topup(&pool, &ev, &pi).await.unwrap());
        assert!(!credit_confirmed_topup(&pool, &ev, &pi).await.unwrap(), "replay must not credit twice");

        let account = crate::db::credits::get_or_create(&pool, &org).await.unwrap();
        assert_eq!(account.balance_cents, 5000);
        let (entries, total) = crate::db::credits::list_ledger(&pool, &org, 10, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(entries[0].entry_type, "topup");
        assert_eq!(entries[0].external_reference.as_deref(), Some(pi.as_str()));
    }

    #[tokio::test]
    #[ignore]
    async fn a_non_topup_payment_or_missing_org_metadata_credits_nothing() {
        let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL to a migrated test database");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let pi_other = format!("pi_other_{nonce}");
        let pi_bare = format!("pi_bare_{nonce}");
        pi_db::insert(&pool, &pi_other, "domain_management", &format!("o-{nonce}"), 1200, "usd", "succeeded").await.unwrap();
        pi_db::insert(&pool, &pi_bare, TOPUP_CONSUMER, &format!("b-{nonce}"), 2500, "usd", "succeeded").await.unwrap();

        assert!(!credit_confirmed_topup(&pool, &event(Some("org-x")), &pi_other).await.unwrap());
        assert!(!credit_confirmed_topup(&pool, &event(None), &pi_bare).await.unwrap());
        assert!(!credit_confirmed_topup(&pool, &event(Some("org-x")), "pi_unknown").await.unwrap());
    }
}
