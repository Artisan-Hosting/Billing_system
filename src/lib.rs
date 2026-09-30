//! `Billing` -- internal gRPC gateway to Stripe.
//!
//! A thin, product-agnostic gRPC surface over Stripe ([`grpc`], [`stripe`],
//! [`db`]), plus plan/subscription/invoice management and pool-overage pricing
//! ([`overage`], via `BillingAdminService::RecordOverageUsage`).
//! `domain_management`'s domain-purchase flow is the first consumer; the design
//! (an opaque `consumer` + `external_reference` pair, no product concepts
//! leaking into this crate) is meant for other services to reuse without
//! Billing needing to change for them -- see `src/grpc/service.rs`'s own
//! module doc for the authorization model this implies.
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
pub mod rollover;
pub mod stripe;
