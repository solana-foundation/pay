//! `pay solana` — pass-through to the Solana CLI with automatic keypair injection.
//!
//! Loads the keypair from pay's keystore, writes it to a temporary file,
//! and passes `--keypair <tempfile>` to the `solana` CLI. The temp file
//! is deleted when the process exits.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run a Solana CLI command using your pay account keypair.
///
/// All arguments are forwarded to the `solana` binary with `--keypair`
/// injected automatically from your pay account.
///
/// Examples:
///   pay solana balance
///   pay solana transfer <recipient> 0.1
///   pay solana airdrop 1
#[derive(clap::Args)]
#[command(disable_help_flag = true)]
pub struct SolanaCommand {
    /// Arguments forwarded to the solana CLI.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub args: Vec<String>,
}

impl SolanaCommand {
    pub fn run(self, active_account_name: Option<&str>) -> pay_core::Result<i32> {
        let keypair_bytes = if active_account_name.is_none()
            && let Ok(accounts) = pay_core::accounts::AccountsFile::load()
            && let Some((name, account)) = accounts.default_account()
        {
            pay_core::signer::load_keypair_bytes_from_account_with_reason(
                account,
                name,
                pay_core::accounts::MAINNET_NETWORK,
                "Use your pay account with the Solana CLI.",
            )?
        } else {
            let source = active_account_name
                .map(|s| s.to_string())
                .or_else(|| {
                    let config = pay_core::Config::load().unwrap_or_default();
                    config.default_active_account_name()
                })
                .ok_or_else(|| {
                    pay_core::Error::Config(
                        "No account configured. Run `pay setup` first.".to_string(),
                    )
                })?;
            load_keypair_bytes(&source)?
        };

        // Write to a secure temp file (auto-deleted on drop)
        let dir = tempfile::tempdir()?;
        let keypair_path = dir.path().join("keypair.json");
        {
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;

            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).write(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);

            let mut file = opts.open(&keypair_path)?;
            let json = serde_json::to_string(&*keypair_bytes)?;
            file.write_all(json.as_bytes())?;
        }

        // Check if user already passed --keypair (don't double it)
        let has_keypair_flag = self
            .args
            .iter()
            .any(|a| a == "--keypair" || a.starts_with("--keypair=") || a == "-k");

        let mut cmd = Command::new("solana");
        if !has_keypair_flag {
            cmd.arg("--keypair").arg(&keypair_path);
        }
        cmd.args(&self.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let status = cmd.status().map_err(|e| {
            pay_core::Error::Config(format!(
                "Failed to launch solana: {e}. Is the Solana CLI installed?"
            ))
        })?;

        // dir drops here → temp keypair file is deleted
        Ok(status.code().unwrap_or(1))
    }
}

fn load_keypair_bytes(source: &str) -> pay_core::Result<pay_core::keystore::Zeroizing<Vec<u8>>> {
    use pay_core::keystore::Keystore;

    let (backend_name, account) = if let Some(account) = source.strip_prefix("keychain:") {
        ("keychain", Some(account))
    } else if let Some(account) = source.strip_prefix("gnome-keyring:") {
        ("gnome-keyring", Some(account))
    } else if let Some(account) = source.strip_prefix("windows-hello:") {
        ("windows-hello", Some(account))
    } else if let Some(account) = source.strip_prefix("1password:") {
        ("1password", Some(account))
    } else {
        ("file", None)
    };

    if let Some(account) = account {
        let ks = match backend_name {
            #[cfg(target_os = "macos")]
            "keychain" => Keystore::apple_keychain(),
            #[cfg(not(target_os = "macos"))]
            "keychain" => {
                return Err(pay_core::Error::Config(
                    "Keychain not available on this platform".into(),
                ));
            }
            #[cfg(target_os = "linux")]
            "gnome-keyring" => Keystore::gnome_keyring(),
            #[cfg(not(target_os = "linux"))]
            "gnome-keyring" => {
                return Err(pay_core::Error::Config(
                    "GNOME Keyring not available on this platform".into(),
                ));
            }
            #[cfg(target_os = "windows")]
            "windows-hello" => Keystore::windows_hello(),
            #[cfg(not(target_os = "windows"))]
            "windows-hello" => {
                return Err(pay_core::Error::Config(
                    "Windows Hello not available on this platform".into(),
                ));
            }
            "1password" => Keystore::onepassword(None),
            _ => unreachable!(),
        };
        return ks
            .load_keypair(account, "Use your pay account with the Solana CLI.")
            .map_err(|e| pay_core::Error::Config(format!("{backend_name}: {e}")));
    }

    // File-based
    let expanded = shellexpand::tilde(source);
    let data = std::fs::read_to_string(expanded.as_ref())
        .map_err(|e| pay_core::Error::Config(format!("Failed to read {source}: {e}")))?;
    let bytes: Vec<u8> = serde_json::from_str(&data)
        .map_err(|e| pay_core::Error::Config(format!("Invalid keypair JSON: {e}")))?;
    Ok(pay_core::keystore::Zeroizing::new(bytes))
}
