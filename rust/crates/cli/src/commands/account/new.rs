//! `pay account new` — generate a fresh keypair and store it.

use dialoguer::Select;
use owo_colors::OwoColorize;
use pay_core::keystore::Keystore;

/// Generate a new keypair and store it securely.
#[derive(clap::Args)]
pub struct NewCommand {
    /// Account name (required).
    pub name: String,

    /// Storage backend: "keychain" (macOS), "gnome-keyring" (Linux),
    /// "windows-hello" (Windows), "file" (headless fallback), or a remote
    /// signing backend registered in `pay_core::remote` ("openfort").
    #[arg(long)]
    pub backend: Option<String>,

    /// Legacy vault name.
    #[arg(long, hide = true)]
    pub vault: Option<String>,

    /// Replace existing account.
    #[arg(long)]
    pub force: bool,

    /// Remote-backend credential read from a file, as `key=@path`,
    /// repeatable (e.g. `--credential secret_key=@/run/secrets/openfort`).
    /// Values are never taken from the command line. Each field also reads
    /// `{PROVIDER}_{FIELD}` from the environment.
    #[arg(long = "credential", value_name = "KEY=@FILE")]
    pub credentials: Vec<String>,

    /// Remote-backend wallet id (env: `{PROVIDER}_WALLET_ID`). Defaults to
    /// the account's only Solana wallet, or prompts to pick.
    #[arg(long)]
    pub wallet_id: Option<String>,
}

impl NewCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let remote = RemoteInputs::resolve(&self.credentials, self.wallet_id.clone())?;
        let (pubkey, backend_name) = create_account(
            &self.name,
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
            &remote,
        )?;
        eprintln!();

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        let completion = crate::tui::run_topup_flow(&pubkey, &rpc_url, &self.name)?;
        print_next_steps(
            &self.name,
            backend_name,
            completion.as_ref().map(|c| &c.received),
        );
        Ok(())
    }
}

/// Core account creation logic. Returns the base58 pubkey on success.
/// Shared by `pay account new` and `pay setup`.
/// Returns `(pubkey_b58, backend_display_name)`.
pub fn create_account(
    name: &str,
    backend: Option<&str>,
    vault: Option<&str>,
    force: bool,
    remote: &RemoteInputs,
) -> pay_core::Result<(String, &'static str)> {
    let backend_id = resolve_backend(backend)?;

    if backend_id == crate::commands::cloud_onboard::CLOUD_BACKEND_FLAG {
        return Err(pay_core::Error::Config(
            "The browser-linked remote wallet is set up with `pay setup --backend cloud`."
                .to_string(),
        ));
    }

    if let Some(provider) = pay_core::remote::provider(&backend_id) {
        let inputs = remote.clone().with_env_wallet_id(provider.id());
        return create_remote_account(name, provider, force, &inputs);
    }

    let (ks, keystore_kind, backend_display, op_info) = build_keystore(&backend_id, vault, name)?;

    if ks.exists(name) && !force {
        let pubkey = ks
            .pubkey(name)
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;
        let pubkey_b58 = bs58::encode(&pubkey).into_string();
        eprintln!();
        crate::components::print_notice(
            crate::components::NoticeLevel::Info,
            "Account already exists",
            &format!(
                "`{name}` is already stored in {backend_display}.\nUse --force to replace it."
            ),
        );

        // Ensure the account is registered in accounts.yml even if the
        // keypair already exists in the keystore (e.g. after a reset).
        save_account(
            name,
            keystore_kind,
            &pubkey_b58,
            op_info.as_ref().and_then(|i| i.vault.clone()),
            None,
            op_info.as_ref().and_then(|i| i.account.clone()),
        )?;

        return Ok((pubkey_b58, backend_display));
    }

    let (keypair_bytes, pubkey_b58) = generate_keypair();

    let sync = if backend_id == "1password" {
        pay_core::keystore::SyncMode::CloudSync
    } else {
        pay_core::keystore::SyncMode::ThisDeviceOnly
    };

    let intent = pay_core::keystore::AuthIntent::create_account(name);
    ks.import_with_intent(name, &keypair_bytes, sync, &intent)
        .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

    save_account(
        name,
        keystore_kind,
        &pubkey_b58,
        op_info
            .as_ref()
            .and_then(|i| i.vault.clone())
            .or(vault.map(|v| v.to_string())),
        None,
        op_info.as_ref().and_then(|i| i.account.clone()),
    )?;

    Ok((pubkey_b58, backend_display))
}

/// Remote-backend credentials supplied up front, so setup can run without
/// a TTY.
///
/// Values are provider-agnostic: `--credential key=@file` (repeatable)
/// names any field the chosen provider declares and reads its value from
/// the file, and each field also falls back to a `{PROVIDER}_{FIELD}`
/// environment variable — `OPENFORT_SECRET_KEY` for Openfort's
/// `secret_key`. A value on the command line itself is refused: argv is
/// readable by every process on the machine and kept in shell history.
/// Anything still missing is prompted for interactively.
#[derive(Clone, Default)]
pub struct RemoteInputs {
    /// Credential values keyed by [`CredentialField::key`](pay_core::remote::CredentialField).
    pub credentials: std::collections::BTreeMap<String, String>,
    /// Provider-side wallet id, or `{PROVIDER}_WALLET_ID`. When absent,
    /// the wallet is discovered from the account.
    pub wallet_id: Option<String>,
}

impl RemoteInputs {
    /// Parse repeated `key=@file` credential flags and the wallet id.
    ///
    /// Errors never echo the flag: an operator who typed a secret where a
    /// path belongs must not see it copied into logs.
    pub fn resolve(credentials: &[String], wallet_id: Option<String>) -> pay_core::Result<Self> {
        let mut map = std::collections::BTreeMap::new();
        for entry in credentials {
            let (key, value) = parse_credential_flag(entry)?;
            map.insert(key, value);
        }
        Ok(Self {
            credentials: map,
            wallet_id: wallet_id
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        })
    }

    /// A credential from the flags, else `{PROVIDER}_{FIELD}` in the
    /// environment.
    fn credential(
        &self,
        provider: &dyn pay_core::remote::RemoteProvider,
        field: &pay_core::remote::CredentialField,
    ) -> Option<String> {
        self.credentials
            .get(field.key)
            .cloned()
            .or_else(|| env_var(&env_key(provider.id(), field.key)))
    }

    /// Fill the wallet id from `{PROVIDER}_WALLET_ID` when no flag set it.
    fn with_env_wallet_id(mut self, provider_id: &str) -> Self {
        if self.wallet_id.is_none() {
            self.wallet_id = env_var(&env_key(provider_id, "wallet_id"))
                // Openfort's wallet id is an account id; accept the name
                // its own docs and dashboard use.
                .or_else(|| env_var(&env_key(provider_id, "account_id")));
        }
        self
    }
}

/// One `--credential KEY=@FILE` flag: the key and the file's trimmed
/// contents. `KEY=VALUE` is refused with the two accepted alternatives.
fn parse_credential_flag(entry: &str) -> pay_core::Result<(String, String)> {
    let Some((key, source)) = entry.split_once('=') else {
        return Err(pay_core::Error::Config(
            "Invalid --credential: expected KEY=@FILE.".to_string(),
        ));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(pay_core::Error::Config(
            "Invalid --credential: the key before `=` is empty.".to_string(),
        ));
    }
    let Some(path) = source.trim().strip_prefix('@') else {
        return Err(pay_core::Error::Config(format!(
            "--credential {key}=<value> is not accepted: a value on the command line is visible \
             to other processes and kept in shell history. Put it in a file and pass \
             --credential {key}=@/path/to/file, or set the {{PROVIDER}}_{} environment variable.",
            key.to_uppercase()
        )));
    };
    if path.is_empty() {
        return Err(pay_core::Error::Config(format!(
            "--credential {key}=@: no file path after `@`."
        )));
    }
    let value = std::fs::read_to_string(path).map_err(|err| {
        pay_core::Error::Config(format!(
            "Could not read --credential {key} from `{path}`: {err}"
        ))
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(pay_core::Error::Config(format!(
            "--credential {key}: `{path}` is empty."
        )));
    }
    Ok((key.to_string(), value.to_string()))
}

/// `openfort` + `secret_key` → `OPENFORT_SECRET_KEY`.
fn env_key(provider_id: &str, field: &str) -> String {
    format!("{provider_id}_{field}")
        .to_uppercase()
        .replace('-', "_")
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Fail with a clear message when a value has to be prompted for but
/// there is no terminal to prompt on.
fn require_tty(missing: &str) -> pay_core::Result<()> {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return Ok(());
    }
    Err(pay_core::Error::Config(format!(
        "No terminal to prompt for `{missing}`.\n\
         Pass it non-interactively instead: --credential {missing}=@<file> (repeatable) \
         and --wallet-id, or the matching {{PROVIDER}}_{{FIELD}} environment variables."
    )))
}

/// Pick which wallet to connect from those the credentials can sign for.
///
/// One wallet is used automatically; several are offered as a list; none
/// defers to the provider's own guidance, since pay does not create
/// wallets on a provider's behalf.
fn choose_wallet(
    wallets: Vec<pay_core::remote::RemoteWallet>,
    provider: &dyn pay_core::remote::RemoteProvider,
    theme: &dialoguer::theme::ColorfulTheme,
) -> pay_core::Result<String> {
    match wallets.len() {
        0 => Err(pay_core::Error::Config(
            provider.no_wallets_hint().to_string(),
        )),
        1 => {
            let wallet = wallets.into_iter().next().expect("len checked");
            eprintln!(
                "  {} {}",
                "Using backend wallet".dimmed(),
                wallet.address.as_str()
            );
            Ok(wallet.id)
        }
        _ => {
            require_tty("backend wallet choice")?;
            let labels: Vec<String> = wallets
                .iter()
                .map(|w| format!("{}  ({})", w.address, w.id))
                .collect();
            let choice = Select::with_theme(theme)
                .with_prompt(format!("Which {}?", provider.display_name()))
                .items(&labels)
                .default(0)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;
            Ok(wallets
                .into_iter()
                .nth(choice)
                .expect("index from Select")
                .id)
        }
    }
}

/// Connect a wallet held by a remote signing backend as a pay account.
///
/// Provider-agnostic: the credentials to collect, how to validate them,
/// how to list wallets, and how to connect all come from the
/// [`RemoteProvider`](pay_core::remote::RemoteProvider) implementation.
/// Each credential is taken from a flag, then the environment, then an
/// interactive prompt. Nothing is persisted until the wallet's address
/// resolves. No keypair is generated — signing happens remotely.
fn create_remote_account(
    name: &str,
    provider: &'static dyn pay_core::remote::RemoteProvider,
    force: bool,
    inputs: &RemoteInputs,
) -> pay_core::Result<(String, &'static str)> {
    let display = provider.display_name();

    // Hardware wallets keep nothing in the secret store: "already
    // connected" means an accounts.yml entry exists.
    let already_connected = if provider.requires_credentials() {
        pay_core::remote::credentials_exist(name)
    } else {
        pay_core::accounts::AccountsFile::load()
            .ok()
            .and_then(|f| {
                f.named_account_for_network(pay_core::accounts::MAINNET_NETWORK, name)
                    .map(|a| a.provider.as_deref() == Some(provider.id()))
            })
            .unwrap_or(false)
    };

    if already_connected && !force {
        let pubkey = pay_core::accounts::AccountsFile::load()
            .ok()
            .and_then(|f| {
                f.named_account_for_network(pay_core::accounts::MAINNET_NETWORK, name)
                    .and_then(|a| a.pubkey.clone())
            })
            .ok_or_else(|| {
                pay_core::Error::Config(format!(
                    "Credentials for `{name}` already exist but the account is not \
                     registered in accounts.yml. Re-run with --force to replace them."
                ))
            })?;
        eprintln!();
        crate::components::print_notice(
            crate::components::NoticeLevel::Info,
            "Account already exists",
            &format!(
                "`{name}` is already connected to a {display}.\n\
                 Use --force to replace the stored credentials."
            ),
        );
        return Ok((pubkey, display));
    }

    let theme = dialoguer::theme::ColorfulTheme::default();
    let fields = provider.credential_fields();
    let will_prompt = fields
        .iter()
        .any(|f| inputs.credential(provider, f).is_none())
        && std::io::IsTerminal::is_terminal(&std::io::stderr());
    if will_prompt || !provider.requires_credentials() {
        eprintln!();
        eprintln!("  {}", provider.credentials_hint());
    }

    // Collect credentials in the provider's declared order, stopping at
    // the first invalid value so a typo costs one prompt, not a round trip.
    let mut credentials = pay_core::remote::Credentials::new();
    let mut discovered: Option<Vec<pay_core::remote::RemoteWallet>> = None;

    for field in fields {
        let value = match inputs.credential(provider, field) {
            Some(value) => value,
            None => {
                require_tty(field.key)?;
                prompt_credential(&theme, field)?
            }
        };
        provider.validate_credential(field.key, &value)?;
        credentials.insert(field.key.to_string(), value);

        // Discover as soon as the provider has enough to list wallets:
        // it validates the credentials so far, and the wallet is chosen
        // before the remaining secrets are asked for.
        if discovered.is_none() && inputs.wallet_id.is_none() {
            discovered = provider.discover(&credentials).ok();
        }
    }

    let wallet_id = match &inputs.wallet_id {
        Some(id) => id.trim().to_string(),
        None => {
            let wallets = match discovered {
                Some(wallets) => wallets,
                None => provider.discover(&credentials)?,
            };
            choose_wallet(wallets, provider, &theme)?
        }
    };
    provider.validate_wallet_id(&wallet_id)?;

    // Resolve the wallet's address before persisting anything.
    eprintln!("  {}", format!("Verifying with {display}…").dimmed());
    let pubkey = pay_core::remote::fetch_wallet_address(provider, &credentials, &wallet_id)?;

    if provider.requires_credentials() {
        let ks = platform_credential_keystore()?;
        let intent = pay_core::keystore::AuthIntent::create_account(name);
        pay_core::remote::store_credentials(&ks, name, &credentials, &intent)?;
    }

    save_account_remote(name, provider.id(), &pubkey, &wallet_id)?;

    Ok((pubkey, display))
}

/// Prompt for one credential, masked when the provider marks it secret.
fn prompt_credential(
    theme: &dialoguer::theme::ColorfulTheme,
    field: &pay_core::remote::CredentialField,
) -> pay_core::Result<String> {
    let value = if field.secret {
        dialoguer::Password::with_theme(theme)
            .with_prompt(field.label)
            .interact()
    } else {
        dialoguer::Input::<String>::with_theme(theme)
            .with_prompt(field.label)
            .interact_text()
    };
    Ok(value
        .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?
        .trim()
        .to_string())
}

/// Platform secret store used for remote credential blobs, with the
/// same setup-time gating fallbacks as the keypair backends.
pub(crate) fn platform_credential_keystore() -> pay_core::Result<Keystore> {
    let platform = pay_core::backend::platform().ok_or_else(|| {
        pay_core::Error::Config(
            "Remote-backend accounts require a platform secret store, which is unavailable on \
             this platform."
                .to_string(),
        )
    })?;
    if !platform.is_available() {
        return Err(backend_unavailable_error(platform));
    }
    let gate = setup_gate(platform)?;
    platform.keystore(&pay_core::backend::StoreParams::default(), gate)
}

/// Resolved 1Password account info for storing in accounts.yml.
pub struct OpAccountInfo {
    pub vault: Option<String>,
    pub account: Option<String>,
}

pub(super) fn build_keystore(
    backend_id: &str,
    vault: Option<&str>,
    account_name: &str,
) -> pay_core::Result<(
    Keystore,
    pay_core::accounts::BackendKind,
    &'static str,
    Option<OpAccountInfo>,
)> {
    use pay_core::accounts::BackendKind;
    use pay_core::backend::StoreParams;

    let Some(local) = pay_core::backend::local_by_flag(backend_id) else {
        return Err(pay_core::Error::Config(format!(
            "Unknown backend: {backend_id}. Use {}.",
            available_backends_hint()
        )));
    };
    if let Some(reason) = local.deprecated() {
        return Err(pay_core::Error::Config(reason.to_string()));
    }
    if !local.is_available() {
        return Err(backend_unavailable_error(local));
    }

    let kind = local.kind();
    let file_path = file_backend_path(account_name)
        .to_string_lossy()
        .into_owned();
    let op_account = match kind {
        BackendKind::OnePassword => resolve_op_account()?,
        _ => None,
    };
    let params = match kind {
        BackendKind::OnePassword => StoreParams {
            vault,
            op_account: op_account.as_deref(),
            file_path: None,
        },
        BackendKind::File => StoreParams {
            file_path: Some(&file_path),
            ..StoreParams::default()
        },
        _ => StoreParams::default(),
    };
    let op_info = match kind {
        BackendKind::OnePassword => Some(OpAccountInfo {
            vault: vault.map(|v| v.to_string()),
            account: op_account.clone(),
        }),
        _ => None,
    };

    let gate = setup_gate(local)?;
    let ks = local.keystore(&params, gate)?;
    Ok((ks, kind, local.display_name(), op_info))
}

/// The approval gate to put in front of a store at account-creation time.
///
/// Creating or importing an account is explicit consent to write the key,
/// so setup relaxes the gate on hosts where the platform prompt cannot run
/// (no enrolled Touch ID, no local polkit agent). The persisted account
/// keeps `auth_required: true`, so runtime signing is still approved
/// through the platform prompt or the MCP elicitation override.
pub(super) fn setup_gate(
    local: &dyn pay_core::backend::LocalKeystoreBackend,
) -> pay_core::Result<pay_core::backend::Gate> {
    use pay_core::accounts::BackendKind;
    use pay_core::backend::Gate;

    match local.kind() {
        BackendKind::AppleKeychain => {
            if local.platform_gate_available() {
                Ok(Gate::Platform)
            } else {
                eprintln!(
                    "Note: Touch ID is not enrolled on this Mac; storing in Apple Keychain without a biometric gate. Runtime signing will still require approval via the configured auth path."
                );
                Ok(Gate::Disabled)
            }
        }
        BackendKind::GnomeKeyring => {
            if !local.is_available() {
                return Err(gnome_keyring_unavailable_error());
            }
            if local.platform_gate_available() {
                #[cfg(target_os = "linux")]
                crate::commands::setup::install_linux_polkit_policy_if_needed()?;
                Ok(Gate::Platform)
            } else {
                eprintln!(
                    "Note: No local Polkit prompt is available; using the already-unlocked GNOME Keyring without a setup-time prompt. Runtime signing still requires MCP approval or a configured Polkit agent."
                );
                Ok(Gate::Disabled)
            }
        }
        BackendKind::WindowsHello => {
            if local.platform_gate_available() {
                Ok(Gate::Platform)
            } else {
                Err(pay_core::Error::Config(
                    "Windows Hello is not configured.".to_string(),
                ))
            }
        }
        // 1Password prompts through `op`; file writes never prompt.
        BackendKind::OnePassword | BackendKind::File => Ok(Gate::Platform),
        BackendKind::Ephemeral | BackendKind::Remote => Err(pay_core::Error::Config(format!(
            "`{}` is not a local keystore backend.",
            local.id()
        ))),
    }
}

fn backend_unavailable_error(
    local: &dyn pay_core::backend::LocalKeystoreBackend,
) -> pay_core::Error {
    match local.kind() {
        pay_core::accounts::BackendKind::GnomeKeyring => gnome_keyring_unavailable_error(),
        pay_core::accounts::BackendKind::OnePassword => pay_core::Error::Config(
            "1Password CLI (`op`) is not installed or not signed in.".to_string(),
        ),
        pay_core::accounts::BackendKind::WindowsHello => {
            pay_core::Error::Config("Windows Hello is not configured.".to_string())
        }
        _ => pay_core::Error::Config(format!(
            "{} is not available on this platform.",
            local.display_name()
        )),
    }
}

/// Comma-separated list of backends that work on the current OS, platform
/// keystore first and every registered remote backend after it. Used in
/// error messages so we don't suggest `keychain` to a Linux user.
fn available_backends_hint() -> String {
    let platform = pay_core::backend::platform().filter(|p| p.is_available());
    let Some(platform) = platform else {
        return if cfg!(target_os = "linux") {
            "'file'".to_string()
        } else {
            "a supported platform backend".to_string()
        };
    };

    std::iter::once(platform.flag())
        .chain(std::iter::once(
            crate::commands::cloud_onboard::CLOUD_BACKEND_FLAG,
        ))
        .chain(pay_core::remote::providers().map(|p| p.flag()))
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(" or ")
}

pub(super) fn file_backend_path(account_name: &str) -> std::path::PathBuf {
    pay_core::accounts::FileAccountsStore::default_keypair_path(account_name)
}

/// Resolve and preflight the backend before setup performs any unrelated
/// configuration writes.
pub fn resolve_backend(backend: Option<&str>) -> pay_core::Result<String> {
    let backend = match backend {
        Some(backend) => backend.to_string(),
        None => pick_backend()?,
    };

    if let Some(local) = pay_core::backend::local_by_flag(&backend) {
        if let Some(reason) = local.deprecated() {
            return Err(pay_core::Error::Config(reason.to_string()));
        }
        if !local.is_available() {
            return Err(backend_unavailable_error(local));
        }
    }

    Ok(backend)
}

fn gnome_keyring_unavailable_error() -> pay_core::Error {
    pay_core::Error::Config(
        "GNOME Keyring Secret Service is not reachable in this session.\n\
         On a headless Linux server, run and pre-unlock GNOME Keyring as the same service user, \
         then ensure pay/Hermes inherits that session's DBUS_SESSION_BUS_ADDRESS.\n\
         Install the `gnome-keyring` package if it is missing, then retry with \
         `pay setup --backend gnome-keyring`. Pay will not start or unlock the service automatically."
            .to_string(),
    )
}

/// Resolve which 1Password account to use. If only one account is
/// configured, use it automatically. If multiple, prompt the user.
pub fn resolve_op_account() -> pay_core::Result<Option<String>> {
    let output = std::process::Command::new("op")
        .args(["account", "list", "--format=json"])
        .output()
        .map_err(|e| pay_core::Error::Config(format!("op account list: {e}")))?;

    if !output.status.success() {
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct OpAccount {
        account_uuid: String,
        email: String,
        url: String,
    }

    let accounts: Vec<OpAccount> = serde_json::from_slice(&output.stdout).unwrap_or_default();

    match accounts.len() {
        0 => Ok(None),
        1 => Ok(Some(accounts[0].account_uuid.clone())),
        _ => {
            let labels: Vec<String> = accounts
                .iter()
                .map(|a| format!("{} ({})", a.email, a.url))
                .collect();

            let selection =
                dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt("Which 1Password account?")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

            Ok(Some(accounts[selection].account_uuid.clone()))
        }
    }
}

/// Interactive backend picker. Returns the backend id string.
pub fn pick_backend() -> pay_core::Result<String> {
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if !has_tty {
        return Err(pay_core::Error::Config(format!(
            "No --backend specified and no interactive terminal available.\n  \
             Pass --backend=<one of {}>.",
            available_backends_hint()
        )));
    }

    struct Opt {
        id: &'static str,
        name: String,
        detail: String,
    }

    fn opt(backend: &dyn pay_core::backend::SigningBackend) -> Opt {
        Opt {
            id: backend.flag(),
            name: backend.display_name().to_string(),
            detail: backend.description().to_string(),
        }
    }

    // The OS-native store comes first. Linux without a reachable keyring
    // falls back to the plain file; Windows without Hello offers nothing.
    let platform = pay_core::backend::platform().filter(|p| p.is_available());
    let mut options: Vec<Opt> = match platform {
        Some(p) => vec![opt(p)],
        None if cfg!(target_os = "linux") => vec![opt(&pay_core::backend::File)],
        None => Vec::new(),
    };

    // The remote wallet is set up from the browser; its token lives in the
    // platform secret store, so only offer it when one is available. A
    // bring-your-own custody provider (`--backend openfort`) is a flag, not
    // a picker entry: the browser flow is the remote wallet.
    if platform.is_some() {
        options.push(Opt {
            id: crate::commands::cloud_onboard::CLOUD_BACKEND_FLAG,
            name: crate::commands::cloud_onboard::CLOUD_BACKEND_NAME.to_string(),
            detail: crate::commands::cloud_onboard::CLOUD_BACKEND_DETAIL.to_string(),
        });
    }

    // Hardware wallets need no secret store at all, so they are offered
    // whenever this build includes them, plugged in or not.
    options.extend(
        pay_core::remote::providers()
            .filter(|p| p.custody() == pay_core::backend::Custody::Hardware)
            .map(|p| opt(p)),
    );

    if options.is_empty() {
        #[cfg(target_os = "linux")]
        return Err(gnome_keyring_unavailable_error());

        #[cfg(not(target_os = "linux"))]
        return Err(pay_core::Error::Config(
            "No supported keystore backend is available on this system.".to_string(),
        ));
    }

    // Two aligned columns: the backend name, then what it means for the
    // user, dimmed. The theme highlights the whole active row.
    let name_width = options
        .iter()
        .map(|o| o.name.chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<String> = options
        .iter()
        .map(|o| format!("{:<name_width$}   {}", o.name, o.detail.dimmed()))
        .collect();

    eprintln!();
    let selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Where should pay keep your wallet?")
        .items(&items)
        .default(0)
        .report(true)
        .interact_opt()
        .map_err(|e| pay_core::Error::Config(format!("Selection failed: {e}")))?
        .ok_or_else(|| pay_core::Error::Config("Setup cancelled.".to_string()))?;

    Ok(options[selection].id.to_string())
}

pub fn save_account(
    name: &str,
    backend: pay_core::accounts::BackendKind,
    pubkey: &str,
    vault: Option<String>,
    path: Option<String>,
    account: Option<String>,
) -> pay_core::Result<()> {
    let mut accounts = pay_core::accounts::AccountsFile::load()?;
    accounts.upsert(
        pay_core::accounts::MAINNET_NETWORK,
        name,
        pay_core::accounts::Account {
            backend,
            provider: None,
            active: false,
            auth_required: Some(true),
            pubkey: Some(pubkey.to_string()),
            vault,
            account,
            path,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        },
    );
    accounts.save()
}

/// Register a remote-backend account: `keystore: remote` plus the
/// provider id and the provider-side wallet id.
pub fn save_account_remote(
    name: &str,
    provider_id: &str,
    pubkey: &str,
    wallet_id: &str,
) -> pay_core::Result<()> {
    let mut accounts = pay_core::accounts::AccountsFile::load()?;
    accounts.upsert(
        pay_core::accounts::MAINNET_NETWORK,
        name,
        pay_core::accounts::Account {
            backend: pay_core::accounts::BackendKind::Remote,
            provider: Some(provider_id.to_string()),
            active: false,
            auth_required: Some(true),
            pubkey: Some(pubkey.to_string()),
            vault: None,
            account: Some(wallet_id.to_string()),
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        },
    );
    accounts.save()
}

/// Print the post-setup summary and next-step hints.
///
/// Shows `✔` confirmation lines for keystore and (if funded) the received
/// amount. Skips the topup hint when the user already funded during setup.
pub fn print_next_steps(
    name: &str,
    backend_name: &str,
    received: Option<&pay_core::client::balance::ReceivedFunds>,
) {
    eprintln!();
    eprintln!(
        "  {} Account secured in {}",
        "✔".green(),
        backend_name.green()
    );

    if let Some(r) = received {
        let amount = format_received(r);
        if !amount.is_empty() {
            eprintln!("  {} Account funded with {}", "✔".green(), amount.green());
        }
        eprintln!();
        crate::components::print_notice(
            crate::components::NoticeLevel::Info,
            "Ready to go. Time to make HTTP pay for itself.",
            "$ claude -p \"what can i do with pay?\"",
        );
    } else {
        eprintln!();
        crate::components::print_notice(
            crate::components::NoticeLevel::Warning,
            "Top-up required",
            &topup_required_body(name),
        );
    }

    eprintln!();
}

fn topup_required_body(name: &str) -> String {
    format!(
        "A top-up is required before making paid requests.\n$ {}",
        crate::commands::topup::topup_retry_command(name)
    )
}

pub fn format_received(r: &pay_core::client::balance::ReceivedFunds) -> String {
    if let Some(usdc) = r.tokens.iter().find(|t| t.is_symbol("USDC")) {
        return format!("${:.2}", usdc.ui_amount);
    }
    if let Some(token) = r.tokens.first() {
        let sym = token.symbol_or("tokens");
        return format!("{:.2} {sym}", token.ui_amount);
    }
    if r.sol_lamports > 0 {
        return format!("{:.4} SOL", r.sol_lamports as f64 / 1_000_000_000.0);
    }
    String::new()
}

pub fn generate_keypair() -> (Vec<u8>, String) {
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    let mut keypair_bytes = Vec::with_capacity(64);
    keypair_bytes.extend_from_slice(&signing_key.to_bytes());
    keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

    let pubkey_b58 = bs58::encode(&verifying_key.to_bytes()).into_string();
    (keypair_bytes, pubkey_b58)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_flag_reads_the_value_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, "  sk_live_abc\n").unwrap();
        let inputs = RemoteInputs::resolve(
            &[format!("secret_key=@{}", path.display())],
            Some(" acc_1 ".to_string()),
        )
        .unwrap();
        assert_eq!(inputs.credentials["secret_key"], "sk_live_abc");
        assert_eq!(inputs.wallet_id.as_deref(), Some("acc_1"));
    }

    #[test]
    fn credential_flag_refuses_a_value_on_the_command_line_without_echoing_it() {
        let Err(pay_core::Error::Config(msg)) =
            RemoteInputs::resolve(&["secret_key=sk_live_topsecret".to_string()], None)
        else {
            panic!("argv values must be refused");
        };
        assert!(!msg.contains("topsecret"), "{msg}");
        assert!(
            msg.contains("--credential secret_key=@/path/to/file"),
            "{msg}"
        );
        assert!(msg.contains("{PROVIDER}_SECRET_KEY"), "{msg}");

        let Err(pay_core::Error::Config(msg)) =
            RemoteInputs::resolve(&["sk_live_topsecret".to_string()], None)
        else {
            panic!("a flag without `=` must be refused");
        };
        assert!(!msg.contains("topsecret"), "{msg}");
    }

    #[test]
    fn credential_flag_reports_missing_and_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let Err(pay_core::Error::Config(msg)) =
            RemoteInputs::resolve(&[format!("secret_key=@{}", missing.display())], None)
        else {
            panic!("a missing file is an error");
        };
        assert!(
            msg.contains("Could not read --credential secret_key"),
            "{msg}"
        );

        let empty = dir.path().join("empty");
        std::fs::write(&empty, "\n").unwrap();
        let Err(pay_core::Error::Config(msg)) =
            RemoteInputs::resolve(&[format!("secret_key=@{}", empty.display())], None)
        else {
            panic!("an empty file is an error");
        };
        assert!(msg.ends_with("is empty."), "{msg}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unavailable_keyring_error_explains_headless_session_requirements() {
        let message = gnome_keyring_unavailable_error().to_string();

        assert!(message.contains("Secret Service is not reachable"));
        assert!(message.contains("pre-unlock"));
        assert!(message.contains("DBUS_SESSION_BUS_ADDRESS"));
    }

    #[test]
    fn file_backend_path_matches_account_default() {
        assert_eq!(
            file_backend_path("server"),
            pay_core::accounts::FileAccountsStore::default_keypair_path("server")
        );
    }

    #[test]
    fn topup_required_body_uses_default_topup_command_for_default_account() {
        assert_eq!(
            topup_required_body("default"),
            "A top-up is required before making paid requests.\n$ pay topup"
        );
    }

    #[test]
    fn topup_required_body_uses_named_account_topup_command() {
        assert_eq!(
            topup_required_body("test-2"),
            "A top-up is required before making paid requests.\n$ pay topup --account test-2"
        );
    }
}
