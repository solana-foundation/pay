//! `pay setup` — generate a keypair, store it, and fund your account.
//!
//! Convenience command that combines `pay account new` + `pay topup`.

use dialoguer::Confirm;
use owo_colors::OwoColorize;

/// Generate a keypair, store it securely, and fund your account.
#[derive(clap::Args)]
pub struct SetupCommand {
    /// Replace existing account with a new one.
    #[arg(long)]
    pub force: bool,

    /// Storage backend: "keychain" (macOS), "gnome-keyring" (Linux),
    /// "windows-hello" (Windows), "1password".
    #[arg(long)]
    pub backend: Option<String>,

    /// 1Password vault name.
    #[arg(long)]
    pub vault: Option<String>,
}

impl SetupCommand {
    pub fn run(self) -> pay_core::Result<()> {
        // Abort before any prompts if the default account already exists.
        if !self.force
            && let Ok(accounts) = pay_core::accounts::AccountsFile::load()
            && accounts
                .accounts
                .get(pay_core::accounts::MAINNET_NETWORK)
                .is_some_and(|net| net.contains_key("default"))
        {
            super::account::list::print_account_list(
                &accounts,
                None::<super::account::list::Highlight>,
            );
            eprintln!(
                "{}",
                "  A default account already exists. Use --force to replace it, or `pay account new --name <name>` to add another.".dimmed()
            );
            eprintln!();
            return Ok(());
        }

        // Offer to install the agent skill if npx is available.
        maybe_install_skill();

        let (pubkey, backend_name) = super::account::new::create_account(
            "default",
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
        )?;

        eprintln!();

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        let received = crate::tui::run_topup_flow(&pubkey, &rpc_url, "default")?;
        super::account::new::print_next_steps("default", backend_name, received.as_ref());
        Ok(())
    }
}

/// If `npx` is on PATH, offer to install the pay agent skill for coding agents.
fn maybe_install_skill() {
    let npx_bin = if cfg!(windows) { "npx.cmd" } else { "npx" };
    let has_npx = std::process::Command::new(npx_bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if !has_npx {
        return;
    }

    eprintln!();
    let install = Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Install pay skill for your coding agents? (Claude Code, Cursor, …)")
        .default(true)
        .interact()
        .unwrap_or(false);

    if !install {
        return;
    }

    eprintln!();
    let status = std::process::Command::new(npx_bin)
        .args([
            "-y",
            "skills",
            "add",
            "https://github.com/solana-foundation/pay",
            "--skill",
            "pay",
            "-y",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("  {} Skill installed", "✔".green());
        }
        _ => {
            eprintln!(
                "{}",
                "  Skill install failed — you can retry later with:".dimmed()
            );
            eprintln!(
                "{}",
                "  npx -y skills add https://github.com/solana-foundation/pay --skill pay -y"
                    .dimmed()
            );
        }
    }
    eprintln!();
}
