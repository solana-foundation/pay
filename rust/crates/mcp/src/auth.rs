//! [`AuthGate`] backed by MCP elicitation.
//!
//! When the connected MCP client advertises the `elicitation` capability
//! AND no local platform biometric is available, pay-mcp installs this
//! gate so signing confirmations flow through the LLM client's UI (Claude
//! Desktop dialog, Hermes approval prompt, Telegram message, etc.) instead
//! of the (missing) platform biometric prompt.
//!
//! When a local biometric IS available (Touch ID, Windows Hello, polkit),
//! the platform gate is preferred — a native prompt is faster and more
//! familiar than a round-trip through the MCP client UI. The install-site
//! check lives in `mcp/src/tools/curl.rs::make_auth_override`; set
//! `PAY_FORCE_ELICITATION=1` to override and route every approval through
//! the MCP client anyway.
//!
//! The [`AuthGate`] trait is synchronous, but rmcp's elicitation call is
//! `async`. We bridge with [`tokio::task::block_in_place`] + the current
//! [`tokio::runtime::Handle`] — the entire pay-mcp server runs on a
//! multi-threaded Tokio runtime, so the calling thread can yield to the
//! runtime while we wait on the elicitation round-trip. This is the same
//! shape that platform gates already use (Touch ID is also a blocking
//! "wait for a human" call from Rust's view; the difference is purely
//! plumbing).
//!
//! All failure modes map to [`pay_keystore::Error::AuthDenied`]: declined
//! responses, cancelled responses, transport errors, and timeouts. The
//! caller treats any non-Accept outcome as "user did not approve".

use std::time::Duration;

use pay_keystore::{AuthGate, AuthIntent, Error as KeystoreError};
use rmcp::Peer;
use rmcp::model::{
    CreateElicitationRequestParam, CreateElicitationResult, ElicitationAction,
    ElicitationSchema,
};
use rmcp::service::RoleServer;
use tokio::runtime::Handle;

/// Outer deadline for a single elicitation round-trip, including the
/// human's response time. Matches Hermes' gateway approval default so
/// users on async surfaces (Telegram, Slack) have time to respond.
const ELICITATION_TIMEOUT: Duration = Duration::from_secs(300);

/// `AuthGate` that asks the connected MCP client for approval via
/// `elicitation/create` instead of a platform biometric prompt.
pub struct ElicitationAuth {
    peer: Peer<RoleServer>,
}

impl ElicitationAuth {
    /// Construct a new gate bound to the active MCP session's peer.
    ///
    /// The peer is cheap to clone — it holds an inner `Arc` — so callers
    /// usually take a clone out of the rmcp tool-call context and hand it
    /// here per signing operation.
    pub fn new(peer: Peer<RoleServer>) -> Self {
        Self { peer }
    }
}

impl AuthGate for ElicitationAuth {
    fn authenticate(&self, intent: &AuthIntent) -> Result<(), KeystoreError> {
        let params = build_request(intent);
        let peer = self.peer.clone();

        // Bridge sync → async. `block_in_place` permits blocking on the
        // current multi-threaded runtime worker without starving other
        // tasks; `Handle::current().block_on` drives the elicitation
        // future to completion. Equivalent to how the macOS Touch ID
        // gate blocks the calling thread on `LAContext.evaluatePolicy`.
        let outcome: Result<CreateElicitationResult, rmcp::ServiceError> =
            tokio::task::block_in_place(|| {
                Handle::current().block_on(async move {
                    tokio::time::timeout(ELICITATION_TIMEOUT, peer.create_elicitation(params))
                        .await
                        .map_err(|_| {
                            rmcp::ServiceError::Timeout {
                                timeout: ELICITATION_TIMEOUT,
                            }
                        })?
                })
            });

        match outcome {
            Ok(res) => match res.action {
                ElicitationAction::Accept => Ok(()),
                ElicitationAction::Decline => Err(KeystoreError::AuthDenied(
                    "user declined the request via the MCP client".to_string(),
                )),
                ElicitationAction::Cancel => Err(KeystoreError::AuthDenied(
                    "user cancelled the request via the MCP client".to_string(),
                )),
            },
            Err(err) => Err(KeystoreError::AuthDenied(format!(
                "elicitation transport failed: {err}"
            ))),
        }
    }

    fn is_available(&self) -> bool {
        // We don't ping the peer here — `authenticate()` would surface a
        // transport failure as AuthDenied anyway, and is_available() is
        // called from contexts where blocking is undesirable.
        true
    }
}

/// Build the `elicitation/create` request body for an [`AuthIntent`].
///
/// Per the design decisions for v1:
/// - **Schema is structured** (boolean `approved` + optional `limit_label`),
///   so clients that render forms can present a confirmation UI; clients
///   that fall back to yes/no still get the message text.
/// - **Per-call only**: no server-side state binds approvals across calls.
fn build_request(intent: &AuthIntent) -> CreateElicitationRequestParam {
    // Builder validates required fields against declared properties.
    // The combination below is statically sound; `expect` would only
    // fire if rmcp's validation contract changes in a future release.
    let schema = ElicitationSchema::builder()
        .required_bool("approved")
        .build()
        .expect("required_bool registers `approved` in properties");

    CreateElicitationRequestParam {
        message: intent.message().to_string(),
        requested_schema: schema,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_carries_intent_message() {
        let intent = AuthIntent::authorize_payment("$0.50", "accessing API api.example.com");
        let req = build_request(&intent);
        assert!(
            req.message.contains("$0.50"),
            "message should include amount: {:?}",
            req.message,
        );
        assert!(
            req.message.contains("api.example.com"),
            "message should include operator: {:?}",
            req.message,
        );
    }

    #[test]
    fn build_request_includes_approved_boolean_field() {
        let intent = AuthIntent::default_payment();
        let req = build_request(&intent);
        // The schema must include an `approved` property so even
        // form-rendering clients have a concrete confirmation field.
        let json = serde_json::to_value(&req.requested_schema)
            .expect("schema should serialize");
        let props = json.get("properties").expect("schema has properties");
        assert!(
            props.get("approved").is_some(),
            "schema should expose `approved` boolean: {json}",
        );
    }
}
