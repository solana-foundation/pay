//! Local MPP channel deployment and hook trust policy. Never applies to x402.

use crate::{Error, Result};
use pay_kit::core::payment_channels_v2::{ChannelHooks, resolve_deployment};
use solana_pubkey::Pubkey;

fn environment(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(Error::Config(format!("{name} must contain UTF-8 text"))),
    }
}

/// Server opt-in configuration. An explicit program never silently falls back.
pub fn server_profile() -> Result<(Pubkey, Option<u8>, Option<ChannelHooks>)> {
    parse_server_profile(
        environment("PAY_PAYMENT_CHANNELS_PROGRAM_ID")?.as_deref(),
        environment("PAY_MPP_CHANNEL_VERSION")?.as_deref(),
        environment("PAY_MPP_CHANNEL_HOOKS")?.as_deref(),
    )
}

fn parse_server_profile(
    program: Option<&str>,
    version: Option<&str>,
    hooks: Option<&str>,
) -> Result<(Pubkey, Option<u8>, Option<ChannelHooks>)> {
    let program = program
        .map(str::parse)
        .transpose()
        .map_err(|_| Error::Config("invalid PAY_PAYMENT_CHANNELS_PROGRAM_ID".into()))?
        .unwrap_or_else(pay_kit::mpp::program::payment_channels::default_program_id);
    let version = version
        .map(str::parse::<u8>)
        .transpose()
        .map_err(|_| Error::Config("invalid PAY_MPP_CHANNEL_VERSION".into()))?;
    resolve_deployment(program, version)
        .map_err(|error| Error::Config(format!("invalid MPP channel deployment: {error}")))?;
    let hooks: Option<ChannelHooks> = hooks
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| Error::Config(format!("invalid PAY_MPP_CHANNEL_HOOKS: {error}")))?;
    if let Some(hooks) = &hooks {
        if version != Some(2) {
            return Err(Error::Config("MPP channel hooks require version 2".into()));
        }
        hooks
            .validate()
            .map_err(|error| Error::Config(error.to_string()))?;
    }
    Ok((program, version, hooks))
}

/// Enable channel-backed charges only for an explicitly selected V2 deployment.
/// The operator owns the channel; the merchant remains the payout recipient.
pub fn server_charge_config(
    signer: Option<std::sync::Arc<dyn pay_kit::solana_keychain::TransactionSigner>>,
) -> Result<Option<pay_kit::mpp::server::ChannelChargeConfig>> {
    let (_, version, hooks) = server_profile()?;
    if version != Some(2) {
        return Ok(None);
    }
    let payee_signer = signer.ok_or_else(|| {
        Error::Config("V2 channel-backed charges require an operator transaction signer".into())
    })?;
    Ok(Some(pay_kit::mpp::server::ChannelChargeConfig {
        payee_signer,
        hooks: hooks.unwrap_or_default(),
    }))
}

/// Issue the configured charge mode. Used both for challenges and for rebuilding
/// route pricing during verification; native charge behavior is unchanged.
pub fn charge_with_options(
    mpp: &pay_kit::mpp::server::Mpp,
    amount: &str,
    options: pay_kit::mpp::server::ChargeOptions<'_>,
) -> std::result::Result<pay_kit::mpp::PaymentChallenge, pay_kit::mpp::Error> {
    if mpp.channel_charge_enabled() {
        mpp.charge_channel_with_options(amount, options)
    } else {
        mpp.charge_with_options(amount, options)
    }
}

/// Validate the remote profile before requesting wallet access. Hook execution
/// requires an exact descriptor match with local policy, not just a program ID.
pub fn validate_challenge(request: &pay_kit::mpp::SessionRequest) -> Result<()> {
    let details = &request.method_details;
    let program = details
        .channel_program
        .parse()
        .map_err(|_| Error::Mpp("invalid session channel program".into()))?;
    pay_kit::mpp::program::channel_deployment::resolve_for_network(
        program,
        details.channel_version,
        details.channel_hooks.as_ref(),
        &details.network,
    )
    .map_err(|error| Error::Mpp(format!("invalid session channel deployment: {error}")))?;
    if let Some(hooks) = &details.channel_hooks {
        if details.channel_version != Some(2) {
            return Err(Error::Mpp("session hooks require version 2".into()));
        }
        hooks
            .validate()
            .map_err(|error| Error::Mpp(error.to_string()))?;
        let trusted = trusted_hooks()?;
        require_trusted_hooks(hooks, trusted.as_ref())?;
    }
    Ok(())
}

fn trusted_hooks() -> Result<Option<ChannelHooks>> {
    environment("PAY_MPP_TRUSTED_CHANNEL_HOOKS")?
        .map(|value| serde_json::from_str::<ChannelHooks>(&value))
        .transpose()
        .map_err(|_| Error::Config("invalid PAY_MPP_TRUSTED_CHANNEL_HOOKS".into()))
}

/// Validate channel-backed charge policy before wallet access. Native charges
/// return `None` and do not consult the hook policy.
pub fn validate_charge_challenge(
    request: &pay_kit::mpp::ChargeRequest,
) -> Result<Option<ChannelHooks>> {
    let Some(profile) = request
        .method_details
        .as_ref()
        .and_then(|details| details.get("channelCharge"))
    else {
        return Ok(None);
    };
    let trusted = trusted_hooks()?;
    validate_charge_profile(
        profile,
        request
            .method_details
            .as_ref()
            .and_then(|details| details.get("network"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("mainnet"),
        trusted.as_ref(),
    )
    .map(Some)
}

fn validate_charge_profile(
    profile: &serde_json::Value,
    network: &str,
    trusted: Option<&ChannelHooks>,
) -> Result<ChannelHooks> {
    let profile: pay_kit::mpp::protocol::channel_charge::ChannelChargeProfile =
        serde_json::from_value(profile.clone())
            .map_err(|error| Error::Mpp(format!("invalid channel charge profile: {error}")))?;
    if profile.channel_version != 2 {
        return Err(Error::Mpp(
            "channel-backed charges require version 2".into(),
        ));
    }
    pay_kit::mpp::program::channel_deployment::resolve_for_network(
        profile
            .channel_program
            .parse()
            .map_err(|_| Error::Mpp("invalid charge channel program".into()))?,
        Some(profile.channel_version),
        Some(&profile.channel_hooks),
        network,
    )
    .map_err(|error| Error::Mpp(format!("invalid charge channel deployment: {error}")))?;
    profile
        .channel_hooks
        .validate()
        .map_err(|error| Error::Mpp(error.to_string()))?;
    // Even an empty descriptor requires explicit approval for this opt-in path.
    require_trusted_hooks(&profile.channel_hooks, trusted)?;
    Ok(profile.channel_hooks)
}

fn require_trusted_hooks(hooks: &ChannelHooks, trusted: Option<&ChannelHooks>) -> Result<()> {
    if trusted != Some(hooks) {
        return Err(Error::Mpp(
            "MPP channel hooks are not approved by local PAY_MPP_TRUSTED_CHANNEL_HOOKS policy"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_charge_requires_exact_policy_and_valid_v2_profile() {
        let hooks = ChannelHooks::default();
        let profile = serde_json::json!({
            "channelProgram": "CHNLxBtXw7VPqd5GMcbtVoEbn4ehyaSoiHj56Ao5sWkJ",
            "channelVersion": 2,
            "channelHooks": {},
            "recentSlot": 42
        });
        assert!(validate_charge_profile(&profile, "devnet", None).is_err());
        assert_eq!(
            validate_charge_profile(&profile, "devnet", Some(&hooks)).unwrap(),
            hooks
        );
        let mut invalid = profile.clone();
        invalid["channelVersion"] = serde_json::json!(1);
        assert!(validate_charge_profile(&invalid, "devnet", Some(&hooks)).is_err());
        let mut invalid = profile.clone();
        invalid["channelProgram"] = serde_json::json!(Pubkey::new_unique().to_string());
        assert!(validate_charge_profile(&invalid, "devnet", Some(&hooks)).is_err());
        let mut invalid = profile.clone();
        invalid.as_object_mut().unwrap().remove("recentSlot");
        assert!(validate_charge_profile(&invalid, "devnet", Some(&hooks)).is_err());
        assert!(validate_charge_profile(&serde_json::Value::Null, "devnet", Some(&hooks)).is_err());
    }

    #[test]
    fn default_is_v1_and_invalid_explicit_program_fails_closed() {
        let (program, version, hooks) = parse_server_profile(None, None, None).unwrap();
        assert_eq!(
            program,
            pay_kit::mpp::program::payment_channels::default_program_id()
        );
        assert_eq!(version, None);
        assert_eq!(hooks, None);
        assert!(parse_server_profile(Some("invalid"), None, None).is_err());
    }

    #[test]
    fn both_deployments_accept_v1_and_explicit_v2_but_hooks_require_opt_in() {
        let v2 = "CHNLxBtXw7VPqd5GMcbtVoEbn4ehyaSoiHj56Ao5sWkJ";
        assert!(parse_server_profile(Some(v2), None, None).is_ok());
        assert!(parse_server_profile(None, Some("2"), None).is_ok());
        assert!(parse_server_profile(Some(v2), Some("3"), None).is_err());
        assert!(parse_server_profile(Some(v2), Some("2"), None).is_ok());
        assert!(parse_server_profile(None, None, Some("{}")).is_err());
        let hooks: ChannelHooks = serde_json::from_str("{}").unwrap();
        assert!(require_trusted_hooks(&hooks, None).is_err());
        assert!(require_trusted_hooks(&hooks, Some(&hooks)).is_ok());
    }

    #[test]
    fn trusting_a_program_does_not_trust_changed_hook_arguments() {
        let descriptor = serde_json::json!({
            "postDistribute": {
                "programId": Pubkey::new_unique().to_string(),
                "entrypoint": [1, 2, 3, 4, 5, 6, 7, 8],
                "accounts": [{
                    "address": Pubkey::new_unique().to_string(),
                    "writable": false
                }],
                "staticData": [1]
            }
        });
        let trusted: ChannelHooks = serde_json::from_value(descriptor.clone()).unwrap();
        assert!(require_trusted_hooks(&trusted, Some(&trusted)).is_ok());
        let mut changed = descriptor.clone();
        changed["postDistribute"]["staticData"] = serde_json::json!([2]);
        let changed: ChannelHooks = serde_json::from_value(changed).unwrap();
        assert!(require_trusted_hooks(&changed, Some(&trusted)).is_err());
        let mut changed = descriptor;
        changed["postDistribute"]["accounts"][0]["writable"] = serde_json::json!(true);
        let changed: ChannelHooks = serde_json::from_value(changed).unwrap();
        assert!(require_trusted_hooks(&changed, Some(&trusted)).is_err());
    }
}
