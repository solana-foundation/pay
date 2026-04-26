use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clap::Args;

/// Run Codex with 402 payment support.
///
/// Launches Codex with the pay MCP server injected automatically.
/// All arguments are passed through to the `codex` binary.
#[derive(Args)]
pub struct CodexCommand {
    /// Arguments forwarded to codex.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl CodexCommand {
    pub fn run(self, pay_bin: &str, active_account_name: Option<&str>) -> pay_core::Result<i32> {
        let instructions_file = write_instructions_file()?;
        let codex_args = build_codex_args(
            pay_bin,
            active_account_name,
            instructions_file.path(),
            &self.args,
        );

        #[cfg(windows)]
        return launch_windows(&codex_args);

        #[cfg(not(windows))]
        {
            let status = Command::new("codex")
                .args(&codex_args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| {
                    pay_core::Error::Config(format!(
                        "Failed to launch codex: {e}. Is it installed?"
                    ))
                })?;

            Ok(status.code().unwrap_or(1))
        }
    }
}

fn build_codex_args(
    pay_bin: &str,
    active_account_name: Option<&str>,
    instructions_path: &Path,
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        config_string("mcp_servers.pay.command", pay_bin),
        "-c".to_string(),
        "mcp_servers.pay.args=[\"mcp\"]".to_string(),
    ];

    // Pass config to MCP server via env.
    let mut env_parts = Vec::new();
    if let Some(source) = active_account_name {
        env_parts.push(format!("PAY_ACTIVE_ACCOUNT={}", toml_string(source)));
    }
    if let Ok(url) = std::env::var("PAY_RPC_URL") {
        env_parts.push(format!("PAY_RPC_URL={}", toml_string(&url)));
    }
    if let Ok(network) = std::env::var("PAY_NETWORK_ENFORCED") {
        env_parts.push(format!("PAY_NETWORK_ENFORCED={}", toml_string(&network)));
    }
    if let Ok(proxy) = std::env::var("PAY_DEBUGGER_PROXY") {
        env_parts.push(format!("PAY_DEBUGGER_PROXY={}", toml_string(&proxy)));
    }
    if !env_parts.is_empty() {
        args.push("-c".to_string());
        args.push(format!("mcp_servers.pay.env={{{}}}", env_parts.join(",")));
    }

    args.push("-c".to_string());
    args.push(config_string(
        "model_instructions_file",
        &instructions_path.to_string_lossy(),
    ));
    args.push("--full-auto".to_string());
    args.extend(extra_args.iter().cloned());
    args
}

fn write_instructions_file() -> pay_core::Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let mut file = tempfile::Builder::new()
        .prefix("pay_codex_instructions_")
        .suffix(".md")
        .tempfile()?;
    file.write_all(pay_core::instructions::INSTRUCTIONS.as_bytes())?;
    file.flush()?;
    Ok(file)
}

fn config_string(key: &str, value: &str) -> String {
    format!("{key}={}", toml_string(value))
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

// On Windows, npm's codex.cmd wrapper forwards %* through cmd.exe. The pay
// instructions include spaces, quotes, and <...> placeholders, which cmd can
// split into stray prompt arguments like "from". Bypass npm shims and run the
// Codex Node entrypoint directly when that layout is present.
#[cfg(windows)]
fn launch_windows(codex_args: &[String]) -> pay_core::Result<i32> {
    let (program, mut args) = windows_codex_command();
    args.extend(codex_args.iter().cloned());

    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            pay_core::Error::Config(format!("Failed to launch codex: {e}. Is it installed?"))
        })?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
fn windows_codex_command() -> (PathBuf, Vec<String>) {
    if let Some((node, codex_js)) = find_npm_codex_entrypoint() {
        return (node, vec![codex_js.to_string_lossy().to_string()]);
    }

    (PathBuf::from("codex.exe"), Vec::new())
}

#[cfg(windows)]
fn find_npm_codex_entrypoint() -> Option<(PathBuf, PathBuf)> {
    for shim in ["codex.cmd", "codex.ps1", "codex"] {
        let Some(shim_path) = find_on_path(shim) else {
            continue;
        };
        let Some(base) = shim_path.parent() else {
            continue;
        };
        for codex_js in codex_js_candidates(base) {
            if codex_js.is_file() {
                let bundled_node = base.join("node.exe");
                let node = if bundled_node.is_file() {
                    bundled_node
                } else {
                    PathBuf::from("node.exe")
                };
                return Some((node, codex_js));
            }
        }
    }

    None
}

#[cfg(windows)]
fn codex_js_candidates(base: &Path) -> [PathBuf; 2] {
    [
        base.join("node_modules")
            .join("@openai")
            .join("codex")
            .join("bin")
            .join("codex.js"),
        base.join("..")
            .join("@openai")
            .join("codex")
            .join("bin")
            .join("codex.js"),
    ]
}

#[cfg(windows)]
fn find_on_path(file_name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(file_name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_string_escapes_quotes_and_angle_examples() {
        let value = r#"Call get_skill_endpoints("<fqn>") then use "<url from results>"."#;

        assert_eq!(
            config_string("instructions", value),
            r#"instructions="Call get_skill_endpoints(\"<fqn>\") then use \"<url from results>\".""#
        );
    }

    #[test]
    fn build_args_escapes_windows_paths_as_toml() {
        let args = build_codex_args(
            r#"C:\Users\me\pay.exe"#,
            Some("default"),
            Path::new(r#"C:\Users\me\AppData\Local\Temp\pay instructions.md"#),
            &[],
        );

        assert!(args.contains(&r#"mcp_servers.pay.command="C:\\Users\\me\\pay.exe""#.to_string()));
        assert!(
            args.contains(&r#"mcp_servers.pay.env={PAY_ACTIVE_ACCOUNT="default"}"#.to_string())
        );
        assert!(args.contains(
            &r#"model_instructions_file="C:\\Users\\me\\AppData\\Local\\Temp\\pay instructions.md""#
                .to_string()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn codex_js_candidates_cover_global_and_local_npm_layouts() {
        let candidates = codex_js_candidates(Path::new(r"C:\Users\me\AppData\Roaming\npm"));

        assert_eq!(
            candidates[0],
            PathBuf::from(r"C:\Users\me\AppData\Roaming\npm")
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js")
        );
        assert_eq!(
            candidates[1],
            PathBuf::from(r"C:\Users\me\AppData\Roaming\npm")
                .join("..")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js")
        );
    }
}
