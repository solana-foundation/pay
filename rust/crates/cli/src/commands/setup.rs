//! `pay setup` — generate a keypair, store it, and fund your account.
//!
//! Convenience command that combines `pay account new` + `pay topup`.

use dialoguer::Confirm;
use owo_colors::OwoColorize;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

#[cfg(target_os = "linux")]
const POLKIT_POLICY_PATH: &str = "/usr/share/polkit-1/actions/sh.pay.unlock-keypair.policy";

#[cfg(target_os = "linux")]
const POLKIT_POLICY: &str = include_str!("../../../../config/polkit/sh.pay.unlock-keypair.policy");

/// Generate a keypair, store it securely, and fund your account.
#[derive(clap::Args)]
pub struct SetupCommand {
    /// Replace existing account with a new one.
    #[arg(long)]
    pub force: bool,

    /// Storage backend: "keychain" (macOS), "gnome-keyring" (Linux),
    /// or "windows-hello" (Windows).
    #[arg(long)]
    pub backend: Option<String>,

    /// Legacy vault name.
    #[arg(long, hide = true)]
    pub vault: Option<String>,

    /// Re-install MCP configs and agent skill without creating a new account.
    #[arg(long)]
    pub update: bool,
}

impl SetupCommand {
    pub fn run(self) -> pay_core::Result<()> {
        if self.update {
            return run_update();
        }

        // Account name comes from the system user (`$USER` / `$USERNAME` /
        // `whoami`), sanitised. Cross-platform — works on Windows, macOS,
        // Linux. Falls back to "default" if nothing is resolvable.
        let account_name = crate::system::current_user_account_name();

        // Abort before any prompts if this user already has an account.
        if !self.force
            && let Ok(accounts) = pay_core::accounts::AccountsFile::load()
            && accounts
                .accounts
                .get(pay_core::accounts::MAINNET_NETWORK)
                .is_some_and(|net| net.contains_key(&account_name))
        {
            super::account::list::print_account_list(
                &accounts,
                None::<super::account::list::Highlight>,
            );
            crate::components::print_notice(
                crate::components::NoticeLevel::Info,
                "Account already exists",
                &format!(
                    "`{account_name}` is already configured.\nUse --force to replace it, or `pay account new <NAME>` to add another."
                ),
            );
            eprintln!();
            return Ok(());
        }

        // Offer to install the agent skill if npx is available.
        maybe_install_skill();

        // Install MCP configs into Claude / Codex / Claude Desktop.
        install_mcp_configs();

        let (pubkey, backend_name) = super::account::new::create_account(
            &account_name,
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
        )?;

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        let completion = crate::tui::run_topup_flow(&pubkey, &rpc_url, &account_name)?;
        if let Some(completion) = completion {
            print_setup_success(backend_name, &completion, &rpc_url);
        } else {
            print_setup_aborted(&account_name, backend_name);
        }
        Ok(())
    }
}

fn print_setup_success(
    backend_name: &str,
    completion: &crate::tui::TopupCompletion,
    rpc_url: &str,
) {
    crate::components::print_notice(
        crate::components::NoticeLevel::Success,
        "Setup complete",
        &setup_success_body(backend_name, completion, rpc_url),
    );
}

fn setup_success_body(
    backend_name: &str,
    completion: &crate::tui::TopupCompletion,
    rpc_url: &str,
) -> String {
    let mut lines = vec![format!("Account secured in {backend_name}")];
    if let Some(amount) = super::topup::topup_received_amount(&completion.received) {
        lines.push(format!("Account funded with {amount}"));
    }
    if let Some(hash) = &completion.tx_hash {
        let cluster = crate::network::SolanaNetwork::Mainnet.explorer_cluster(rpc_url);
        lines.push(format!(
            "{} {hash}",
            crate::components::solana_transaction_link(hash, &cluster)
        ));
    }
    lines.push(String::new());
    lines.push("Ready to go. Time to make HTTP pay for itself.".to_string());
    lines.push("$ pay claude".to_string());
    lines.push("$ pay codex".to_string());
    lines.join("\n")
}

fn print_setup_aborted(account_name: &str, backend_name: &str) {
    crate::components::print_notice(
        crate::components::NoticeLevel::Warning,
        "Setup aborted",
        &setup_aborted_body(account_name, backend_name),
    );
}

fn setup_aborted_body(account_name: &str, backend_name: &str) -> String {
    let topup_cmd = crate::commands::topup::topup_retry_command(account_name);
    format!(
        "Account secured in {backend_name}\nA top-up is required before making paid requests.\n$ {topup_cmd}"
    )
}

/// `pay setup --update`: re-install MCP configs and agent skill.
fn run_update() -> pay_core::Result<()> {
    eprintln!();
    maybe_install_skill();
    install_mcp_configs();
    eprintln!("  {}", "Update complete.".dimmed());
    eprintln!();
    Ok(())
}

// ── MCP config installation ────────────────────────────────────────────────

/// Well-known MCP config locations for each supported app.
fn mcp_config_targets() -> Vec<(&'static str, std::path::PathBuf)> {
    let mut targets = Vec::new();

    // Claude Code: ~/.claude.json
    if let Some(home) = home_dir() {
        targets.push(("Claude Code", home.join(".claude.json")));
    }

    // Claude Desktop
    if let Some(path) = claude_desktop_config_path() {
        targets.push(("Claude Desktop", path));
    }

    targets
}

fn claude_desktop_config_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA").ok().map(|appdata| {
            std::path::PathBuf::from(appdata)
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }
    #[cfg(target_os = "linux")]
    {
        home_dir().map(|h| {
            h.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }
}

fn codex_config_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| h.join(".codex").join("config.toml"))
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(std::path::PathBuf::from)
}

/// Resolve the `pay` binary path for the MCP config.
fn pay_command() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "pay".to_string())
}

/// Install the pay MCP server entry into all detected app configs.
fn install_mcp_configs() {
    let pay_bin = pay_command();
    let targets = mcp_config_targets();
    let mut installed_any = false;

    for (app_name, config_path) in &targets {
        // Only install into apps the user actually has (config dir exists).
        if let Some(parent) = config_path.parent()
            && !parent.exists()
        {
            continue;
        }

        match add_mcp_entry(config_path, &pay_bin) {
            Ok(McpInstallResult::Added) => {
                eprintln!("  {} pay MCP added to {app_name}", "✔".green());
                installed_any = true;
            }
            Ok(McpInstallResult::AlreadyPresent) => {
                eprintln!("  {} pay MCP already configured in {app_name}", "✔".green());
                installed_any = true;
            }
            Ok(McpInstallResult::Updated) => {
                eprintln!("  {} pay MCP updated in {app_name}", "✔".green());
                installed_any = true;
            }
            Err(e) => {
                eprintln!("  {} Failed to configure {app_name}: {e}", "!".yellow());
            }
        }
    }

    if let Some(config_path) = codex_config_path()
        && config_path.parent().is_none_or(|parent| parent.exists())
    {
        match add_codex_mcp_entry(&config_path, &pay_bin) {
            Ok(McpInstallResult::Added) => {
                eprintln!("  {} pay MCP added to Codex", "✔".green());
                installed_any = true;
            }
            Ok(McpInstallResult::AlreadyPresent) => {
                eprintln!("  {} pay MCP already configured in Codex", "✔".green());
                installed_any = true;
            }
            Ok(McpInstallResult::Updated) => {
                eprintln!("  {} pay MCP updated in Codex", "✔".green());
                installed_any = true;
            }
            Err(e) => {
                eprintln!("  {} Failed to configure Codex: {e}", "!".yellow());
            }
        }
    }

    if !installed_any {
        eprintln!(
            "{}",
            "  No supported apps found. Add pay MCP manually:".dimmed()
        );
        eprintln!(
            "{}",
            "  https://github.com/solana-foundation/pay#mcp-server".dimmed()
        );
    }
    eprintln!();
}

enum McpInstallResult {
    Added,
    AlreadyPresent,
    Updated,
}

/// Add or update the `pay` MCP server entry in a JSON config file.
fn add_mcp_entry(config_path: &std::path::Path, pay_bin: &str) -> Result<McpInstallResult, String> {
    let mut config: serde_json::Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| format!("read {}: {e}", config_path.display()))?;
        if raw.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&raw)
                .map_err(|e| format!("parse {}: {e}", config_path.display()))?
        }
    } else {
        serde_json::json!({})
    };

    let servers = config
        .as_object_mut()
        .ok_or("config is not a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let new_entry = serde_json::json!({
        "command": pay_bin,
        "args": ["mcp"]
    });

    let result = if let Some(existing) = servers.get("pay") {
        if existing.get("command").and_then(|v| v.as_str()) == Some(pay_bin)
            && existing.get("args") == Some(&serde_json::json!(["mcp"]))
        {
            McpInstallResult::AlreadyPresent
        } else {
            servers["pay"] = new_entry;
            McpInstallResult::Updated
        }
    } else {
        servers
            .as_object_mut()
            .ok_or("mcpServers is not an object")?
            .insert("pay".to_string(), new_entry);
        McpInstallResult::Added
    };

    if !matches!(result, McpInstallResult::AlreadyPresent) {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&config).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(config_path, json + "\n")
            .map_err(|e| format!("write {}: {e}", config_path.display()))?;
    }

    Ok(result)
}

/// Add or update the `pay` MCP server entry in Codex's TOML config.
fn add_codex_mcp_entry(
    config_path: &std::path::Path,
    pay_bin: &str,
) -> Result<McpInstallResult, String> {
    let raw = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(|e| format!("read {}: {e}", config_path.display()))?
    } else {
        String::new()
    };
    let mut config = if raw.trim().is_empty() {
        DocumentMut::new()
    } else {
        raw.parse::<DocumentMut>()
            .map_err(|e| format!("parse {}: {e}", config_path.display()))?
    };

    let existed = codex_pay_entry_exists(&config);
    let already_present = codex_pay_entry_matches(&config, pay_bin);
    if already_present {
        return Ok(McpInstallResult::AlreadyPresent);
    }

    {
        let mcp_servers = ensure_toml_table(&mut config["mcp_servers"]);
        let pay = ensure_toml_table(&mut mcp_servers["pay"]);
        pay["command"] = value(pay_bin);
        pay["args"] = string_array_item(&["mcp"]);
        pay["enabled"] = value(true);
        pay["enabled_tools"] = string_array_item(super::codex::PAY_MCP_ENABLED_TOOLS);
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    }
    std::fs::write(config_path, config.to_string())
        .map_err(|e| format!("write {}: {e}", config_path.display()))?;

    Ok(if existed {
        McpInstallResult::Updated
    } else {
        McpInstallResult::Added
    })
}

fn ensure_toml_table(item: &mut Item) -> &mut Table {
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("item was just set to a table")
}

fn string_array_item(values: &[&str]) -> Item {
    let mut array = Array::new();
    for value in values {
        array.push(*value);
    }
    Item::Value(Value::Array(array))
}

fn codex_pay_entry_exists(config: &DocumentMut) -> bool {
    config
        .get("mcp_servers")
        .and_then(Item::as_table)
        .and_then(|servers| servers.get("pay"))
        .is_some()
}

fn codex_pay_entry_matches(config: &DocumentMut, pay_bin: &str) -> bool {
    let Some(pay) = config
        .get("mcp_servers")
        .and_then(Item::as_table)
        .and_then(|servers| servers.get("pay"))
        .and_then(Item::as_table)
    else {
        return false;
    };

    let command_matches = pay
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|command| command == pay_bin);
    let args_match = item_string_array(pay.get("args")).is_some_and(|args| args == ["mcp"]);
    let enabled_matches = pay
        .get("enabled")
        .and_then(Item::as_bool)
        .unwrap_or_default();
    let enabled_tools_match = item_string_array(pay.get("enabled_tools"))
        .is_some_and(|tools| tools == super::codex::PAY_MCP_ENABLED_TOOLS);

    command_matches && args_match && enabled_matches && enabled_tools_match
}

fn item_string_array(item: Option<&Item>) -> Option<Vec<&str>> {
    item?
        .as_array()?
        .iter()
        .map(|value| value.as_str())
        .collect()
}

// ── Skill installation ─────────────────────────────────────────────────────

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

// ── Linux polkit ───────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub(crate) fn install_linux_polkit_policy_if_needed() -> pay_core::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if std::fs::read_to_string(POLKIT_POLICY_PATH).is_ok_and(|installed| installed == POLKIT_POLICY)
    {
        return Ok(());
    }

    eprintln!();
    eprintln!(
        "{}",
        "  Installing Linux authentication prompts for pay (polkit policy)...".dimmed()
    );

    let mut child = Command::new("pkexec")
        .args(["tee", POLKIT_POLICY_PATH])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| {
            pay_core::Error::Config(format!(
                "Failed to launch pkexec to install the pay polkit policy: {e}.\n\n\
                 Install it manually with:\n\
                 \x20 sudo tee {POLKIT_POLICY_PATH} >/dev/null <<'EOF'\n\
                 {POLKIT_POLICY}EOF"
            ))
        })?;

    {
        let mut stdin = child.stdin.take().expect("pkexec stdin");
        stdin
            .write_all(POLKIT_POLICY.as_bytes())
            .map_err(|e| pay_core::Error::Config(format!("Failed to write polkit policy: {e}")))?;
    }

    let status = child
        .wait()
        .map_err(|e| pay_core::Error::Config(format!("pkexec failed: {e}")))?;

    if status.success() {
        eprintln!("  {} Linux authentication prompts installed", "✔".green());
        Ok(())
    } else {
        Err(pay_core::Error::Config(format!(
            "Failed to install the pay polkit policy (pkexec exited with {status}).\n\n\
             Install it manually with:\n\
             \x20 sudo cp rust/config/polkit/sh.pay.unlock-keypair.policy {POLKIT_POLICY_PATH}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_mcp_entry_includes_enabled_tools() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"model = "gpt-5"

[mcp_servers.other]
command = "other"
"#,
        )
        .unwrap();

        let result = add_codex_mcp_entry(&config_path, "/usr/local/bin/pay").unwrap();

        assert!(matches!(result, McpInstallResult::Added));
        let raw = std::fs::read_to_string(&config_path).unwrap();
        assert!(raw.contains(r#"model = "gpt-5""#));
        assert!(raw.contains("enabled_tools"));
        let parsed = raw.parse::<DocumentMut>().unwrap();
        assert!(codex_pay_entry_matches(&parsed, "/usr/local/bin/pay"));
    }

    #[test]
    fn codex_mcp_entry_updates_existing_without_allowlist() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"[mcp_servers.pay]
command = "pay"
args = ["mcp"]
enabled = true
"#,
        )
        .unwrap();

        let result = add_codex_mcp_entry(&config_path, "pay").unwrap();

        assert!(matches!(result, McpInstallResult::Updated));
        let raw = std::fs::read_to_string(&config_path).unwrap();
        assert!(raw.contains("enabled_tools"));
        let parsed = raw.parse::<DocumentMut>().unwrap();
        assert!(codex_pay_entry_matches(&parsed, "pay"));
    }

    #[test]
    fn codex_mcp_entry_is_idempotent_when_allowlist_matches() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");

        assert!(matches!(
            add_codex_mcp_entry(&config_path, "pay").unwrap(),
            McpInstallResult::Added
        ));
        assert!(matches!(
            add_codex_mcp_entry(&config_path, "pay").unwrap(),
            McpInstallResult::AlreadyPresent
        ));
    }
}
