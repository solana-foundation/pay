use std::path::PathBuf;

use clap::Args;

/// Start the local MCP server with optional fail-closed payment permissions.
#[derive(Debug, Default, Args)]
pub struct McpCommand {
    /// YAML permission file. CLI permission flags extend or override it.
    #[arg(long, value_name = "FILE")]
    pub permissions: Option<PathBuf>,

    /// Permit payments to this exact HTTP(S) origin. Repeatable.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    pub allowed_origins: Vec<String>,

    /// Permit a Solana cluster (mainnet, devnet, or localnet). Repeatable.
    #[arg(long = "allow-network", value_name = "NETWORK")]
    pub allowed_networks: Vec<String>,

    /// Maximum stablecoin amount per payment, for example "$1.00".
    #[arg(long, value_name = "USD", conflicts_with = "unlimited_amount")]
    pub max_payment: Option<String>,

    /// Remove the per-payment stablecoin cap.
    #[arg(long)]
    pub unlimited_amount: bool,

    /// Permit SPL assets outside pay-kit's known stablecoins.
    #[arg(long)]
    pub allow_any_asset: bool,
}

impl McpCommand {
    pub fn options(&self) -> Result<pay_mcp::McpOptions, String> {
        let has_inline = !self.allowed_origins.is_empty()
            || !self.allowed_networks.is_empty()
            || self.max_payment.is_some()
            || self.unlimited_amount
            || self.allow_any_asset;
        if self.permissions.is_none() && !has_inline {
            return Ok(pay_mcp::McpOptions::default());
        }

        let mut config = match &self.permissions {
            Some(path) => pay_mcp::PermissionConfig::from_yaml_file(path)?,
            None => pay_mcp::PermissionConfig::default(),
        };
        if !self.allowed_origins.is_empty() {
            config
                .origins
                .get_or_insert_with(Vec::new)
                .extend(self.allowed_origins.iter().cloned());
        }
        if !self.allowed_networks.is_empty() {
            config
                .networks
                .get_or_insert_with(Vec::new)
                .extend(self.allowed_networks.iter().cloned());
        }
        if let Some(cap) = &self.max_payment {
            config.max_payment = Some(cap.clone());
            config.unlimited_amount = false;
        }
        if self.unlimited_amount {
            config.max_payment = None;
            config.unlimited_amount = true;
        }
        config.allow_any_asset |= self.allow_any_asset;

        Ok(pay_mcp::McpOptions {
            permissions: Some(pay_mcp::McpPermissions::from_config(&config)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_flags_preserve_the_existing_interactive_behavior() {
        assert!(
            McpCommand::default()
                .options()
                .unwrap()
                .permissions
                .is_none()
        );
    }

    #[test]
    fn inline_flags_compile_pay_kit_permissions() {
        let command = McpCommand {
            allowed_origins: vec!["https://api.example.com".into()],
            allowed_networks: vec!["mainnet".into()],
            max_payment: Some("$2.00".into()),
            ..Default::default()
        };
        assert!(command.options().unwrap().permissions.is_some());
    }
}
