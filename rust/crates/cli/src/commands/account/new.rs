//! `pay account new` — generate a fresh keypair and store it.

use dialoguer::Select;
use owo_colors::OwoColorize;
use pay_core::keystore::Keystore;

/// Generate a new keypair and store it securely.
#[derive(clap::Args)]
pub struct NewCommand {
    /// Account name. Defaults to "default".
    #[arg(long, default_value = "default")]
    pub name: String,

    /// Storage backend: "keychain" (macOS), "gnome-keyring" (Linux),
    /// "windows-hello" (Windows), "1password".
    #[arg(long)]
    pub backend: Option<String>,

    /// 1Password vault name.
    #[arg(long)]
    pub vault: Option<String>,

    /// Replace existing account.
    #[arg(long)]
    pub force: bool,
}

impl NewCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let pubkey = create_account(&self.name, self.backend.as_deref(), self.vault.as_deref(), self.force)?;
        eprintln!();
        eprintln!("  {} {pubkey}", "Your account:".dimmed());
        eprintln!();
        Ok(())
    }
}

/// Core account creation logic. Returns the base58 pubkey on success.
/// Shared by `pay account new` and `pay setup`.
pub fn create_account(
    name: &str,
    backend: Option<&str>,
    vault: Option<&str>,
    force: bool,
) -> pay_core::Result<String> {
    let backend_id = match backend {
        Some(b) => b.to_string(),
        None => pick_backend()?,
    };

    let (ks, keystore_kind, backend_msg) = build_keystore(&backend_id, vault)?;

    if ks.exists(name) && !force {
        let pubkey = ks.pubkey(name).map_err(|e| pay_core::Error::Config(format!("{e}")))?;
        let pubkey_b58 = bs58::encode(&pubkey).into_string();
        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Your account:".dimmed());
        eprintln!();
        eprintln!("{}", "  Account already exists. Use --force to replace it.".dimmed());
        eprintln!();
        return Ok(pubkey_b58);
    }

    // Authenticate before generating (for backends like Apple Keychain)
    if backend_id == "keychain" {
        ks.authenticate("set up your payment account")
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;
    }

    let (keypair_bytes, pubkey_b58) = generate_keypair();

    let sync = if backend_id == "1password" {
        pay_core::keystore::SyncMode::CloudSync
    } else {
        pay_core::keystore::SyncMode::ThisDeviceOnly
    };

    ks.import(name, &keypair_bytes, sync)
        .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

    eprintln!("{}", format!("  {backend_msg}").dimmed());

    save_account(name, keystore_kind, &pubkey_b58, vault.map(|v| v.to_string()), None)?;

    Ok(pubkey_b58)
}

fn build_keystore(
    backend_id: &str,
    vault: Option<&str>,
) -> pay_core::Result<(Keystore, pay_core::accounts::Keystore, &'static str)> {
    match backend_id {
        #[cfg(target_os = "macos")]
        "keychain" => Ok((
            Keystore::apple_keychain(),
            pay_core::accounts::Keystore::AppleKeychain,
            "Stored in macOS Keychain — Touch ID required to pay.",
        )),
        #[cfg(not(target_os = "macos"))]
        "keychain" => Err(pay_core::Error::Config(
            "Keychain is only available on macOS".to_string(),
        )),

        #[cfg(target_os = "linux")]
        "gnome-keyring" => {
            if !Keystore::gnome_keyring_available() {
                return Err(pay_core::Error::Config(
                    "GNOME Keyring is not available.".to_string(),
                ));
            }
            Ok((
                Keystore::gnome_keyring(),
                pay_core::accounts::Keystore::GnomeKeyring,
                "Stored in GNOME Keyring — password prompt required to pay.",
            ))
        }
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => Err(pay_core::Error::Config(
            "GNOME Keyring is only available on Linux".to_string(),
        )),

        #[cfg(target_os = "windows")]
        "windows-hello" => {
            if !Keystore::windows_hello_available() {
                return Err(pay_core::Error::Config(
                    "Windows Hello is not configured.".to_string(),
                ));
            }
            Ok((
                Keystore::windows_hello(),
                pay_core::accounts::Keystore::WindowsHello,
                "Stored in Windows Credential Manager — Windows Hello required to pay.",
            ))
        }
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => Err(pay_core::Error::Config(
            "Windows Hello is only available on Windows".to_string(),
        )),

        "1password" => {
            if !Keystore::onepassword_available() {
                return Err(pay_core::Error::Config(
                    "1Password CLI (`op`) is not installed or not signed in.".to_string(),
                ));
            }
            let ks = match vault {
                Some(v) => Keystore::onepassword_with_vault(v),
                None => Keystore::onepassword(),
            };
            Ok((
                ks,
                pay_core::accounts::Keystore::OnePassword,
                "Stored in 1Password.",
            ))
        }

        other => Err(pay_core::Error::Config(format!(
            "Unknown backend: {other}. Use 'keychain', 'gnome-keyring', 'windows-hello', or '1password'."
        ))),
    }
}

/// Interactive backend picker. Returns the backend id string.
pub fn pick_backend() -> pay_core::Result<String> {
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if !has_tty {
        return Err(pay_core::Error::Config(
            "No --backend specified and no interactive terminal available.\n  \
             Use --backend=keychain or --backend=1password."
                .to_string(),
        ));
    }

    struct Opt {
        id: &'static str,
        label: String,
        available: bool,
    }

    let op_available = Keystore::onepassword_available();

    let mut options = Vec::new();

    // Only show platform-native backend on the current OS
    #[cfg(target_os = "macos")]
    options.push(Opt {
        id: "keychain",
        label: "macOS Keychain (Touch ID)".into(),
        available: true,
    });

    #[cfg(target_os = "linux")]
    {
        let gnome_available = Keystore::gnome_keyring_available();
        options.push(Opt {
            id: "gnome-keyring",
            label: if gnome_available { "GNOME Keyring (password prompt)".into() }
                   else { "GNOME Keyring — not available (desktop session required)".into() },
            available: gnome_available,
        });
    }

    #[cfg(target_os = "windows")]
    {
        let wh_available = Keystore::windows_hello_available();
        options.push(Opt {
            id: "windows-hello",
            label: if wh_available { "Windows Hello (fingerprint / face / PIN)".into() }
                   else { "Windows Hello — not configured".into() },
            available: wh_available,
        });
    }

    options.push(Opt {
        id: "1password",
        label: if op_available { "1Password".into() }
               else { "1Password — `op` CLI not found".into() },
        available: op_available,
    });

    let items: Vec<String> = options.iter().map(|o| {
        if o.available { o.label.clone() } else { format!("{}", o.label.dimmed()) }
    }).collect();

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
        return Err(pay_core::Error::Config("Selected backend is not available.".to_string()));
    }

    Ok(chosen.id.to_string())
}

pub fn save_account(
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

pub fn generate_keypair() -> (Vec<u8>, String) {
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    let mut keypair_bytes = Vec::with_capacity(64);
    keypair_bytes.extend_from_slice(&signing_key.to_bytes());
    keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

    let pubkey_b58 = bs58::encode(&verifying_key.to_bytes()).into_string();
    (keypair_bytes, pubkey_b58)
}
