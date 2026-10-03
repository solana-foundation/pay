//! `sell_inference`: sellers' OpenAI-shaped endpoints, served by their agents.
//!
//! A seller's MCP tool creates an endpoint here and gets back its public URL
//! and an owner token. The public route, `/endpoints/<id>/v1/chat/completions`,
//! is an ordinary paid API: the payment gate challenges and verifies with the
//! seller's own payment backends ([`backends`]), built from the paywall
//! `pay_core::sell_inference` writes. A paid request is not proxied anywhere;
//! it is parked in the endpoint's [`queue`] until the seller's worker, holding
//! the owner token, drains it, streams the agent's answer back, and closes it.
//! The gate settles from the completed response as it would for any upstream.
//!
//! State is in memory and bounded; an endpoint dies with the process. Redis
//! comes with the rest of pay-connect's durable state.

pub mod backends;
pub mod queue;
pub mod routes;

pub use backends::{EndpointBackends, Operator};
pub use queue::{EndpointQueue, Event, ParkedRequest, QueueError};
pub use routes::{EndpointRegistry, SellState, router};
