use dialoguer::Select;
use owo_colors::OwoColorize;
use pay_core::keystore::KeystoreBackend;

/// Set up your payment account. Generates a secure wallet.
///
/// If an account already exists, shows your wallet address.
/// Use --force to replace it with a new one.
#[derive(clap::Args)]
pub struct SetupCommand {
    /// Replace existing account with a new one.
    #[arg(long)]
    pub force: bool,

    /// Storage backend: "keychain" (macOS only), "1password".
    /// If omitted, shows an interactive picker.
    #[arg(long)]
    pub backend: Option<String>,

    /// 1Password vault name (defaults to your default vault).
    #[arg(long)]
    pub vault: Option<String>,
}

/// A backend option shown in the interactive picker.
struct BackendOption {
    id: &'static str,
    label: String,
    available: bool,
}

impl SetupCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let backend = match &self.backend {
            Some(b) => b.clone(),
            None => self.pick_backend()?,
        };

        match backend.as_str() {
            #[cfg(target_os = "macos")]
            "keychain" => self.run_keychain(),
            #[cfg(not(target_os = "macos"))]
            "keychain" => Err(pay_core::Error::Config(
                "Keychain backend is only available on macOS".to_string(),
            )),
            "1password" => self.run_1password(),
            other => Err(pay_core::Error::Config(format!(
                "Unknown backend: {other}. Use 'keychain' or '1password'."
            ))),
        }
    }

    fn pick_backend(&self) -> pay_core::Result<String> {
        let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
        if !has_tty {
            return Err(pay_core::Error::Config(
                "No --backend specified and no interactive terminal available.\n  \
                 Use --backend=keychain or --backend=1password."
                    .to_string(),
            ));
        }

        let keychain_available = cfg!(target_os = "macos");
        let op_available = pay_core::keystore::OnePassword::is_available();

        let options = [
            BackendOption {
                id: "keychain",
                label: if keychain_available {
                    "macOS Keychain (Touch ID)".to_string()
                } else {
                    "macOS Keychain (Touch ID) — macOS only".to_string()
                },
                available: keychain_available,
            },
            BackendOption {
                id: "1password",
                label: if op_available {
                    "1Password".to_string()
                } else {
                    "1Password — `op` CLI not found".to_string()
                },
                available: op_available,
            },
        ];

        let items: Vec<String> = options
            .iter()
            .map(|o| {
                if o.available {
                    o.label.clone()
                } else {
                    format!("{}", o.label.dimmed())
                }
            })
            .collect();

        // Default to the first available option
        let default = options.iter().position(|o| o.available).unwrap_or(0);

        eprintln!();
        let selection = Select::new()
            .with_prompt("Where should pay store your keypair?")
            .items(&items)
            .default(default)
            .interact()
            .map_err(|e| pay_core::Error::Config(format!("Selection cancelled: {e}")))?;

        let chosen = &options[selection];
        if !chosen.available {
            let hint = match chosen.id {
                "keychain" => "Keychain is only available on macOS.",
                "1password" => {
                    "Install the 1Password CLI: https://developer.1password.com/docs/cli/get-started"
                }
                _ => "This backend is not available.",
            };
            return Err(pay_core::Error::Config(hint.to_string()));
        }

        Ok(chosen.id.to_string())
    }

    #[cfg(target_os = "macos")]
    fn run_keychain(&self) -> pay_core::Result<()> {
        use pay_core::keystore::AppleKeychain;

        let backend = AppleKeychain;

        if backend.exists("default") && !self.force {
            return self.show_existing(&backend);
        }

        AppleKeychain::authenticate("set up your payment account")
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        let (keypair_bytes, pubkey_b58) = generate_keypair();

        backend
            .import(
                "default",
                &keypair_bytes,
                pay_core::keystore::SyncMode::ThisDeviceOnly,
            )
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Your account:".dimmed());
        eprintln!();
        eprintln!(
            "{}",
            "  Stored in macOS Keychain — Touch ID required to pay.".dimmed()
        );

        save_account(
            "default",
            pay_core::accounts::Keystore::AppleKeychain,
            &pubkey_b58,
            None,
            None,
        )?;
        self.show_next_steps(&pubkey_b58)
    }

    fn run_1password(&self) -> pay_core::Result<()> {
        use pay_core::keystore::OnePassword;

        if !OnePassword::is_available() {
            return Err(pay_core::Error::Config(
                "1Password CLI (`op`) is not installed or you are not signed in.\n  \
                 Install: https://developer.1password.com/docs/cli/get-started"
                    .to_string(),
            ));
        }

        let backend = match &self.vault {
            Some(vault) => OnePassword::with_vault(vault),
            None => OnePassword::new(),
        };

        if backend.exists("default") && !self.force {
            return self.show_existing(&backend);
        }

        let (keypair_bytes, pubkey_b58) = generate_keypair();

        backend
            .import(
                "default",
                &keypair_bytes,
                pay_core::keystore::SyncMode::CloudSync,
            )
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Your account:".dimmed());
        eprintln!();
        let vault_msg = match &self.vault {
            Some(v) => format!("  Stored in 1Password vault \"{v}\"."),
            None => "  Stored in 1Password (default vault).".to_string(),
        };
        eprintln!("{}", vault_msg.dimmed());

        save_account(
            "default",
            pay_core::accounts::Keystore::OnePassword,
            &pubkey_b58,
            self.vault.clone(),
            None,
        )?;
        self.show_next_steps(&pubkey_b58)
    }

    fn show_existing(&self, backend: &dyn KeystoreBackend) -> pay_core::Result<()> {
        let pubkey = backend
            .pubkey("default")
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;
        let pubkey_b58 = bs58::encode(&pubkey).into_string();
        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Your account:".dimmed());
        eprintln!();
        eprintln!(
            "{}",
            "  You're all set. Fund this address to start paying for APIs.".dimmed()
        );
        eprintln!(
            "{}",
            "  Run `pay setup --force` to create a new account.".dimmed()
        );
        eprintln!();
        Ok(())
    }

    fn show_next_steps(&self, pubkey_b58: &str) -> pay_core::Result<()> {
        eprintln!();
        eprintln!(
            "{}",
            "  Next: fund your account, then run `pay curl <url>` to access paid APIs.".dimmed()
        );
        eprintln!();

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        crate::tui::run_topup_flow(pubkey_b58, &rpc_url)
    }
}

fn save_account(
    name: &str,
    keystore: pay_core::accounts::Keystore,
    pubkey: &str,
    vault: Option<String>,
    path: Option<String>,
) -> pay_core::Result<()> {
    let mut accounts = pay_core::accounts::AccountsFile::load()?;
    accounts.upsert(
        name,
        pay_core::accounts::Account {
            keystore,
            pubkey: Some(pubkey.to_string()),
            vault,
            path,
        },
    );
    accounts.save()
}

fn generate_keypair() -> (Vec<u8>, String) {
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    let mut keypair_bytes = Vec::with_capacity(64);
    keypair_bytes.extend_from_slice(&signing_key.to_bytes());
    keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

    let pubkey_b58 = bs58::encode(&verifying_key.to_bytes()).into_string();
    (keypair_bytes, pubkey_b58)
}
