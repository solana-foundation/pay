//! Typed Solana network helpers for CLI presentation and routing.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaNetwork {
    Mainnet,
    Devnet,
    Localnet,
    Unknown(String),
}

impl SolanaNetwork {
    pub fn from_slug(slug: impl AsRef<str>) -> Self {
        let slug = slug.as_ref().trim();
        match slug {
            "mainnet" | "mainnet-beta" => Self::Mainnet,
            "devnet" => Self::Devnet,
            "localnet" | "sandbox" => Self::Localnet,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn slug(&self) -> &str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Devnet => "devnet",
            Self::Localnet => "localnet",
            Self::Unknown(slug) => slug,
        }
    }

    pub fn is_throwaway(&self) -> bool {
        matches!(self, Self::Devnet | Self::Localnet)
    }

    pub fn default_rpc_url(&self, sandbox: bool) -> String {
        match self {
            Self::Localnet => pay_core::config::SANDBOX_RPC_URL.to_string(),
            Self::Devnet => "https://api.devnet.solana.com".to_string(),
            Self::Mainnet => "https://api.mainnet-beta.solana.com".to_string(),
            Self::Unknown(_) if sandbox => pay_core::config::SANDBOX_RPC_URL.to_string(),
            Self::Unknown(_) => pay_core::config::LOCAL_RPC_URL.to_string(),
        }
    }

    pub fn explorer_cluster(&self, rpc_url: &str) -> SolanaExplorerCluster {
        match self {
            Self::Mainnet => SolanaExplorerCluster::Mainnet,
            Self::Devnet => SolanaExplorerCluster::Devnet,
            Self::Localnet => SolanaExplorerCluster::Custom {
                rpc_url: rpc_url.to_string(),
            },
            Self::Unknown(_) => SolanaExplorerCluster::Default,
        }
    }
}

impl std::fmt::Display for SolanaNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaExplorerCluster {
    Default,
    Mainnet,
    Devnet,
    Custom { rpc_url: String },
}

impl SolanaExplorerCluster {
    pub fn query_suffix(&self) -> String {
        match self {
            Self::Default | Self::Mainnet => String::new(),
            Self::Devnet => "?cluster=devnet".to_string(),
            Self::Custom { rpc_url } => {
                format!("?cluster=custom&customUrl={}", urlencoding::encode(rpc_url))
            }
        }
    }

    pub fn transaction_receipt_query_suffix(&self) -> String {
        match self {
            Self::Default => "?view=receipt".to_string(),
            Self::Mainnet => "?cluster=mainnet-beta&view=receipt".to_string(),
            Self::Devnet => "?cluster=devnet&view=receipt".to_string(),
            Self::Custom { rpc_url } => {
                format!(
                    "?cluster=custom&customUrl={}&view=receipt",
                    urlencoding::encode(rpc_url)
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_slug_normalizes_aliases() {
        assert_eq!(
            SolanaNetwork::from_slug("mainnet-beta"),
            SolanaNetwork::Mainnet
        );
        assert_eq!(SolanaNetwork::from_slug("sandbox"), SolanaNetwork::Localnet);
        assert_eq!(SolanaNetwork::from_slug("devnet").slug(), "devnet");
    }

    #[test]
    fn explorer_cluster_query_uses_typed_network() {
        assert_eq!(
            SolanaNetwork::Mainnet
                .explorer_cluster("https://api.mainnet-beta.solana.com")
                .query_suffix(),
            ""
        );
        assert_eq!(
            SolanaNetwork::Devnet
                .explorer_cluster("https://api.devnet.solana.com")
                .query_suffix(),
            "?cluster=devnet"
        );
        assert_eq!(
            SolanaNetwork::Localnet
                .explorer_cluster("http://localhost:8899")
                .query_suffix(),
            "?cluster=custom&customUrl=http%3A%2F%2Flocalhost%3A8899"
        );
    }

    #[test]
    fn transaction_receipt_query_uses_cluster_and_rpc_url() {
        assert_eq!(
            SolanaNetwork::Mainnet
                .explorer_cluster("https://api.mainnet-beta.solana.com")
                .transaction_receipt_query_suffix(),
            "?cluster=mainnet-beta&view=receipt"
        );
        assert_eq!(
            SolanaNetwork::Localnet
                .explorer_cluster("http://localhost:8899")
                .transaction_receipt_query_suffix(),
            "?cluster=custom&customUrl=http%3A%2F%2Flocalhost%3A8899&view=receipt"
        );
    }
}
