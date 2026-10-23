//! Who a tool call acts for.
//!
//! The tools used to read the accounts file and `PAY_*` variables straight
//! from the process. That is right for `pay mcp` on a laptop and wrong for
//! a hosted server where every call belongs to a different tenant. A
//! [`PayContext`] turns a request into a [`CallScope`]: the accounts to
//! use, the overrides, and how approval works. [`LocalContext`] reproduces
//! the laptop behaviour; pay-connect supplies a tenant context.

use std::sync::Arc;

use pay_core::accounts::{AccountsStore, FileAccountsStore};
use pay_core::signer::AuthOverride;
use rmcp::service::{Peer, RequestContext, RoleServer};

/// Resolves the caller of a tool.
pub trait PayContext: Send + Sync {
    /// Everything this call needs to act for its caller. Errors when the
    /// call carries no usable identity.
    fn scope(&self, call: &RequestContext<RoleServer>) -> Result<CallScope, rmcp::ErrorData>;

    /// Resolve trusted configuration only; consultation happens after the SDK quote.
    /// Optional runtime spending authority. Resolve caller identity from trusted
    /// host/transport context, never clientInfo or agent-supplied arguments.
    /// Default preserves existing local/hosted behavior and YAML permissions.
    fn spending_authority(
        &self,
        _call: &RequestContext<RoleServer>,
    ) -> Result<Option<crate::authority::AuthorityBinding>, rmcp::ErrorData> {
        Ok(None)
    }
}

/// One call's view of the world.
pub struct CallScope {
    /// Accounts the tools operate on, and where remote credentials live.
    pub accounts: Arc<dyn AccountsStore>,
    /// Force this network, as `--sandbox` does.
    pub network_override: Option<String>,
    /// Use this account rather than the network's default.
    pub account_override: Option<String>,
    /// RPC endpoint for networks other than mainnet, when configured.
    pub rpc_url_override: Option<String>,
    /// How a signature gets approved for this caller.
    pub approval: Arc<dyn ApprovalPolicy>,
    /// Optional fail-closed payment permissions configured for local MCP.
    pub payment_permissions: Option<Arc<crate::McpPermissions>>,
    /// Whether `curl` may read a local file as the request body. Only a
    /// process on the user's own machine has files to read.
    pub body_files: bool,
    /// Network destinations `curl` may reach. Hosted contexts must use the
    /// public-only policy; local contexts retain localhost/dev access.
    pub outbound_policy: pay_core::client::fetch::OutboundPolicy,
}

impl CallScope {
    /// The gate to place in front of this call's signatures.
    pub fn auth_override(&self, peer: Option<&Peer<RoleServer>>) -> AuthOverride {
        self.approval.gate(peer)
    }

    /// Run one signing operation under this caller's approval. Whatever the
    /// approval reserved is kept only when `sign` returns its signed result;
    /// a refusal or a later build/sign failure releases it.
    pub fn with_approval<T>(
        &self,
        peer: Option<&Peer<RoleServer>>,
        sign: impl FnOnce(AuthOverride) -> pay_core::Result<T>,
    ) -> pay_core::Result<T> {
        let (gate, hold) = self.approval.gate_with_hold(peer);
        let signed = sign(gate)?;
        hold.commit();
        Ok(signed)
    }

    /// RPC endpoint for `network`.
    pub fn rpc_url(&self, network: &str) -> String {
        if network == pay_core::accounts::MAINNET_NETWORK {
            return pay_core::balance::mainnet_rpc_url();
        }
        self.rpc_url_override
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url)
    }
}

/// How signatures are approved for a caller.
pub trait ApprovalPolicy: Send + Sync {
    /// A fresh gate for one signing operation, or `None` to let the
    /// account's own policy (the platform prompt) decide.
    fn gate(&self, peer: Option<&Peer<RoleServer>>) -> AuthOverride;

    /// [`Self::gate`] plus a hold on anything the gate reserves while it
    /// authorizes. Dropping the hold uncommitted releases the reservation.
    fn gate_with_hold(&self, peer: Option<&Peer<RoleServer>>) -> (AuthOverride, ApprovalHold) {
        (self.gate(peer), ApprovalHold::default())
    }
}

/// Reservations made for one signing operation, released on drop unless
/// [`ApprovalHold::commit`] is called.
#[derive(Default)]
pub struct ApprovalHold {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl ApprovalHold {
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }

    /// Keep the reservations: a signature was produced.
    pub fn commit(mut self) {
        self.release = None;
    }
}

impl Drop for ApprovalHold {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Whether the connected client can show an approval prompt of its own.
pub fn peer_supports_elicitation(peer: &Peer<RoleServer>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.capabilities.elicitation.is_some())
}

/// The laptop policy: the platform prompt (Touch ID, Windows Hello,
/// polkit) when the machine has one, else the MCP client's elicitation.
/// `PAY_FORCE_ELICITATION=1` picks elicitation even with a prompt available,
/// for remote sessions and demos.
pub struct LocalApproval;

impl ApprovalPolicy for LocalApproval {
    fn gate(&self, peer: Option<&Peer<RoleServer>>) -> AuthOverride {
        let peer = peer?;
        let force = std::env::var("PAY_FORCE_ELICITATION")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        if !force && pay_keystore::Keystore::any_biometric_available() {
            return None;
        }
        Some(Box::new(crate::ElicitationAuth::new(peer.clone())))
    }
}

/// `pay mcp` on the user's machine: the accounts file, the `PAY_*`
/// overrides the CLI sets, the platform prompt.
pub struct LocalContext {
    accounts: Arc<FileAccountsStore>,
    permissions: Option<Arc<crate::McpPermissions>>,
}

impl Default for LocalContext {
    fn default() -> Self {
        Self {
            accounts: Arc::new(FileAccountsStore::default_path()),
            permissions: None,
        }
    }
}

impl LocalContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_permissions(mut self, permissions: Option<crate::McpPermissions>) -> Self {
        self.permissions = permissions.map(Arc::new);
        self
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl PayContext for LocalContext {
    fn scope(&self, _call: &RequestContext<RoleServer>) -> Result<CallScope, rmcp::ErrorData> {
        // Read per call: `pay claude` and friends set these for the child
        // process, and tests set them on the fly.
        Ok(CallScope {
            accounts: self.accounts.clone(),
            network_override: env_non_empty("PAY_NETWORK_ENFORCED"),
            account_override: env_non_empty("PAY_ACTIVE_ACCOUNT"),
            rpc_url_override: env_non_empty("PAY_RPC_URL"),
            approval: Arc::new(LocalApproval),
            payment_permissions: self.permissions.clone(),
            body_files: true,
            outbound_policy: pay_core::client::fetch::OutboundPolicy::Unrestricted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_url_is_mainnet_unless_overridden_off_mainnet() {
        let scope = CallScope {
            accounts: Arc::new(pay_core::accounts::MemoryAccountsStore::new()),
            network_override: None,
            account_override: None,
            rpc_url_override: Some("http://127.0.0.1:8899".to_string()),
            approval: Arc::new(LocalApproval),
            payment_permissions: None,
            body_files: true,
            outbound_policy: pay_core::client::fetch::OutboundPolicy::Unrestricted,
        };
        assert_eq!(
            scope.rpc_url("mainnet"),
            pay_core::balance::mainnet_rpc_url()
        );
        assert_eq!(scope.rpc_url("localnet"), "http://127.0.0.1:8899");
    }

    #[test]
    fn empty_env_values_do_not_count_as_overrides() {
        assert_eq!(env_non_empty("PAY_TEST_UNSET_VARIABLE_XYZ"), None);
    }
}
