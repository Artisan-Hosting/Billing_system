//! `Billing` -- internal gRPC gateway to Stripe, and the platform's
//! usage-cost calculator.
//!
//! Two responsibilities in one binary, historically and for now kept
//! together rather than split into two services:
//!
//! 1. **Payment intents** ([`grpc`], [`stripe`], [`db`]) -- a thin,
//!    product-agnostic gRPC surface over Stripe. `domain_management`'s
//!    domain-purchase flow is the first consumer; the design (an opaque
//!    `consumer` + `external_reference` pair, no product concepts leaking
//!    into this crate) is meant for other services to reuse without
//!    Billing needing to change for them -- see `src/grpc/service.rs`'s
//!    own module doc for the authorization model this implies.
//! 2. **Usage cost calculation** ([`usage`]) -- the pre-existing `POST
//!    /calculate` HTTP route, unrelated to Stripe, kept running unchanged
//!    alongside the new gRPC server.
//!
//! Exposed as a library as well as a binary so integration tests can reach
//! the pieces that need a real database or a real HTTP mock -- the same
//! reason `domain_management` does this.

pub mod auth;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod grpc;
pub mod mtls_client;
pub mod overage;
pub mod proration;
pub mod proto;
pub mod stripe;
pub mod usage;
