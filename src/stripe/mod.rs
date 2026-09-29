//! Stripe API client: PaymentIntents only, since that's the whole surface
//! this service needs.
//!
//! Hand-rolled over `reqwest` rather than a general-purpose Stripe crate --
//! the same "a handful of endpoints doesn't need a whole SDK" reasoning
//! `domain_management::cloudflare`'s own module doc gives, and for the same
//! reason that client isn't reused here: Stripe's API is HTTP Basic Auth
//! (the secret key as username, no password) over form-encoded bodies, not
//! JSON, which is a different enough shape to not be worth sharing code
//! with a JSON+bearer-token client.

pub mod webhook;

use serde::Deserialize;
use std::time::Duration;

use crate::error::{Error, Result};

const LIVE_BASE: &str = "https://api.stripe.com/v1";

#[derive(Clone)]
pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
    /// Always [`LIVE_BASE`] outside tests. A field, not a constant baked
    /// into `send`, for the same reason `cloudflare::Api` takes its base
    /// URL as a constructor argument: a test can point it at a local mock
    /// and exercise the real request-building code, rather than a
    /// reimplementation of it.
    base: String,
}

/// The subset of Stripe's own `PaymentIntent` object this service reads.
/// `status` stays a raw Stripe string here (`"requires_payment_method"`,
/// `"succeeded"`, ...) rather than this crate's own
/// [`crate::proto::billing::PaymentIntentStatus`] -- converting the wire
/// value into this service's proto enum is the gRPC handler's job, not
/// this client's; this type only has to agree with Stripe.
#[derive(Debug, Clone, Deserialize)]
pub struct Customer {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaymentIntent {
    pub id: String,
    pub amount: i64,
    pub currency: String,
    pub status: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StripeErrorEnvelope {
    error: StripeErrorDetail,
}

#[derive(Debug, Deserialize)]
struct StripeErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

impl StripeClient {
    pub fn new(secret_key: &str) -> Result<Self> {
        Self::with_base(secret_key, LIVE_BASE)
    }

    pub(crate) fn with_base(secret_key: &str, base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("billing/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Stripe(format!("building http client: {e}")))?;
        Ok(Self { http, secret_key: secret_key.to_owned(), base: base.to_owned() })
    }

    pub fn has_key(&self) -> bool {
        !self.secret_key.is_empty()
    }

    /// Creates a PaymentIntent. `idempotency_key` becomes Stripe's own
    /// `Idempotency-Key` header -- a second call with the same key, made
    /// because our own HTTP call timed out after Stripe had already
    /// committed the first one, returns the original PaymentIntent rather
    /// than creating a second charge. This is Stripe-side idempotency,
    /// distinct from (and in addition to) this service's own
    /// `(consumer, external_reference)` uniqueness at the database layer:
    /// the two protect against different retries -- ours against a caller
    /// retrying `CreatePaymentIntent`, Stripe's against *this* client
    /// retrying its own HTTP call to Stripe.
    ///
    /// With `customer`, the PaymentIntent belongs to that Stripe Customer;
    /// with `save_payment_method` as well, the card the customer pays with
    /// is saved to it for later off-session use (`setup_future_usage=
    /// off_session`), which requires a customer.
    pub async fn create_payment_intent(
        &self,
        amount_cents: i64,
        currency: &str,
        metadata: &[(&str, &str)],
        customer: Option<&str>,
        save_payment_method: bool,
        idempotency_key: &str,
    ) -> Result<PaymentIntent> {
        let mut form: Vec<(String, String)> =
            vec![("amount".to_owned(), amount_cents.to_string()), ("currency".to_owned(), currency.to_owned())];
        if let Some(customer) = customer {
            form.push(("customer".to_owned(), customer.to_owned()));
            if save_payment_method {
                form.push(("setup_future_usage".to_owned(), "off_session".to_owned()));
            }
        }
        for (key, value) in metadata {
            form.push((format!("metadata[{key}]"), (*value).to_owned()));
        }
        self.send(reqwest::Method::POST, "payment_intents", &form, Some(idempotency_key)).await
    }

    /// Creates a Customer. The idempotency key makes a retried call (this
    /// client timing out after Stripe committed) return the original
    /// Customer instead of creating a duplicate.
    pub async fn create_customer(
        &self,
        name: Option<&str>,
        email: Option<&str>,
        metadata: &[(&str, &str)],
        idempotency_key: &str,
    ) -> Result<Customer> {
        let mut form: Vec<(String, String)> = Vec::new();
        if let Some(name) = name {
            form.push(("name".to_owned(), name.to_owned()));
        }
        if let Some(email) = email {
            form.push(("email".to_owned(), email.to_owned()));
        }
        for (key, value) in metadata {
            form.push((format!("metadata[{key}]"), (*value).to_owned()));
        }
        self.send(reqwest::Method::POST, "customers", &form, Some(idempotency_key)).await
    }

    pub async fn get_payment_intent(&self, id: &str) -> Result<PaymentIntent> {
        self.send(reqwest::Method::GET, &format!("payment_intents/{id}"), &[], None).await
    }

    pub async fn cancel_payment_intent(&self, id: &str) -> Result<PaymentIntent> {
        self.send(reqwest::Method::POST, &format!("payment_intents/{id}/cancel"), &[], None).await
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(String, String)],
        idempotency_key: Option<&str>,
    ) -> Result<T> {
        if self.secret_key.is_empty() {
            return Err(Error::Stripe(format!("no Stripe secret key configured; cannot call {path}")));
        }

        let url = format!("{}/{path}", self.base);
        let mut request = self.http.request(method.clone(), &url).basic_auth(&self.secret_key, None::<&str>);
        if method == reqwest::Method::POST {
            request = request.form(form);
        }
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }

        let response = request.send().await.map_err(|e| Error::Stripe(format!("{path}: {e}")))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| Error::Stripe(format!("{path}: reading body: {e}")))?;

        if !status.is_success() {
            if let Ok(envelope) = serde_json::from_str::<StripeErrorEnvelope>(&text) {
                return Err(Error::Stripe(format!(
                    "{path}: {} ({})",
                    envelope.error.message, envelope.error.error_type
                )));
            }
            let snippet: String = text.chars().take(500).collect();
            return Err(Error::Stripe(format!("{path}: HTTP {status}: {snippet}")));
        }

        serde_json::from_str(&text).map_err(|e| {
            let snippet: String = text.chars().take(300).collect();
            Error::Stripe(format!("{path}: HTTP {status}, undecodable response ({e}): {snippet}"))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// The same minimal sequential HTTP/1.1 mock `domain_management`'s
    /// `cloudflare::dns`/`cloudflare::registrar` tests use -- one canned
    /// `(status, body)` response per accepted connection, plus the raw
    /// request text sent back over a channel for assertions on it.
    pub(crate) async fn mock_server(responses: Vec<(u16, String)>) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let read = socket.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..read]).into_owned());

                let response = format!(
                    "HTTP/1.1 {status} status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        (format!("http://{addr}"), rx)
    }

    fn client(base: &str) -> StripeClient {
        StripeClient::with_base("sk_test_123", base).unwrap()
    }

    /// Verbatim shape of a Stripe PaymentIntent create response (trimmed
    /// to the fields this client reads).
    const PAYMENT_INTENT_RESPONSE: &str = r#"{
        "id": "pi_123",
        "object": "payment_intent",
        "amount": 1099,
        "currency": "usd",
        "status": "requires_payment_method",
        "client_secret": "pi_123_secret_abc"
    }"#;

    #[tokio::test]
    async fn create_payment_intent_sends_a_form_encoded_body_with_basic_auth_and_idempotency_key() {
        let (base, mut requests) = mock_server(vec![(200, PAYMENT_INTENT_RESPONSE.to_owned())]).await;
        let result = client(&base)
            .create_payment_intent(1099, "usd", &[("consumer", "domain_management")], None, false, "idem-key-1")
            .await
            .unwrap();

        assert_eq!(result.id, "pi_123");
        assert_eq!(result.amount, 1099);
        assert_eq!(result.status, "requires_payment_method");
        assert_eq!(result.client_secret.as_deref(), Some("pi_123_secret_abc"));

        let request = requests.recv().await.unwrap();
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("POST /payment_intents"), "{request_line}");
        assert!(request.to_lowercase().contains("authorization: basic"), "{request}");
        assert!(request.contains("idempotency-key: idem-key-1"), "{request}");

        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(body.contains("amount=1099"), "{body}");
        assert!(body.contains("currency=usd"), "{body}");
        assert!(body.contains("metadata%5Bconsumer%5D=domain_management"), "{body}");
    }

    #[tokio::test]
    async fn get_payment_intent_sends_no_body() {
        let (base, mut requests) = mock_server(vec![(200, PAYMENT_INTENT_RESPONSE.to_owned())]).await;
        client(&base).get_payment_intent("pi_123").await.unwrap();

        let request = requests.recv().await.unwrap();
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("GET /payment_intents/pi_123"), "{request_line}");
    }

    #[tokio::test]
    async fn cancel_payment_intent_posts_to_the_cancel_path() {
        let (base, mut requests) = mock_server(vec![(200, PAYMENT_INTENT_RESPONSE.to_owned())]).await;
        client(&base).cancel_payment_intent("pi_123").await.unwrap();

        let request = requests.recv().await.unwrap();
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("POST /payment_intents/pi_123/cancel"), "{request_line}");
    }

    #[tokio::test]
    async fn a_stripe_error_response_surfaces_its_message_and_type() {
        let error_body = r#"{"error": {"type": "card_error", "message": "Your card was declined."}}"#;
        let (base, _rx) = mock_server(vec![(402, error_body.to_owned())]).await;
        let err = client(&base).get_payment_intent("pi_123").await.unwrap_err();

        assert!(err.to_string().contains("Your card was declined"), "{err}");
        assert!(err.to_string().contains("card_error"), "{err}");
    }

    #[tokio::test]
    async fn an_undecodable_response_is_a_stripe_error_not_a_panic() {
        let (base, _rx) = mock_server(vec![(200, "not json at all".to_owned())]).await;
        let err = client(&base).get_payment_intent("pi_123").await.unwrap_err();
        assert!(err.to_string().contains("undecodable"), "{err}");
    }

    #[tokio::test]
    async fn create_payment_intent_refuses_without_a_secret_key() {
        let stripe_client = StripeClient::with_base("", "http://127.0.0.1:1").unwrap();
        let err = stripe_client.create_payment_intent(100, "usd", &[], None, false, "key-1").await.unwrap_err();
        assert!(err.to_string().contains("no Stripe secret key"), "{err}");
    }

    #[tokio::test]
    async fn create_payment_intent_attaches_the_customer_and_saves_the_card_only_when_asked() {
        let (base, mut requests) = mock_server(vec![(200, PAYMENT_INTENT_RESPONSE.to_owned()); 3]).await;
        let c = client(&base);

        c.create_payment_intent(2500, "usd", &[], Some("cus_1"), true, "k1").await.unwrap();
        let body = requests.recv().await.unwrap();
        let body = body.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        assert!(body.contains("customer=cus_1"), "{body}");
        assert!(body.contains("setup_future_usage=off_session"), "{body}");

        c.create_payment_intent(2500, "usd", &[], Some("cus_1"), false, "k2").await.unwrap();
        let body = requests.recv().await.unwrap();
        let body = body.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        assert!(body.contains("customer=cus_1") && !body.contains("setup_future_usage"), "{body}");

        // Saving a card without a customer is meaningless; nothing is sent for it.
        c.create_payment_intent(2500, "usd", &[], None, true, "k3").await.unwrap();
        let body = requests.recv().await.unwrap();
        let body = body.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        assert!(!body.contains("customer") && !body.contains("setup_future_usage"), "{body}");
    }

    #[tokio::test]
    async fn create_customer_posts_name_email_metadata_and_an_idempotency_key() {
        let (base, mut requests) = mock_server(vec![(200, r#"{"id": "cus_123", "object": "customer"}"#.to_owned())]).await;
        let customer = client(&base)
            .create_customer(Some("Acme Inc"), Some("billing@acme.test"), &[("organization_id", "org-1")], "customer:org-1")
            .await
            .unwrap();
        assert_eq!(customer.id, "cus_123");

        let request = requests.recv().await.unwrap();
        let request_line = request.lines().next().unwrap_or_default();
        assert!(request_line.starts_with("POST /customers"), "{request_line}");
        assert!(request.contains("idempotency-key: customer:org-1"), "{request}");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(body.contains("name=Acme+Inc") || body.contains("name=Acme%20Inc"), "{body}");
        assert!(body.contains("email=billing%40acme.test"), "{body}");
        assert!(body.contains("metadata%5Borganization_id%5D=org-1"), "{body}");
    }

    #[tokio::test]
    async fn create_customer_omits_absent_name_and_email() {
        let (base, mut requests) = mock_server(vec![(200, r#"{"id": "cus_9"}"#.to_owned())]).await;
        client(&base).create_customer(None, None, &[("organization_id", "org-2")], "customer:org-2").await.unwrap();
        let request = requests.recv().await.unwrap();
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(!body.contains("name=") && !body.contains("email="), "{body}");
    }
}
