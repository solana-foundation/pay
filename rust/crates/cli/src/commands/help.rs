//! Help rendering and machine-readable command catalog helpers.

use crate::components;

pub fn args_include_no_dna() -> bool {
    std::env::args_os().any(|arg| arg == std::ffi::OsStr::new("--no-dna"))
}

pub fn root_overview_help_requested() -> bool {
    let mut saw_help = false;
    let mut expect_value_for: Option<&'static str> = None;

    for arg in std::env::args_os().skip(1) {
        if expect_value_for.take().is_some() {
            continue;
        }

        let Some(arg) = arg.to_str() else {
            return false;
        };

        match arg {
            "-h" | "--help" => saw_help = true,
            "help" => return std::env::args_os().len() == 2,
            "-s" | "--sandbox" | "--mainnet" | "--local" | "--no-dna" | "-v" | "--verbose"
            | "--debugger" | "--dev" => {}
            "--yolo-upto" => expect_value_for = Some("--yolo-upto"),
            _ if arg.starts_with("--yolo-upto=") => {}
            "--account" => expect_value_for = Some("--account"),
            _ if arg.starts_with("--account=") => {}
            _ => return false,
        }
    }

    saw_help
}

pub fn print_root_overview() {
    let banner = components::pay_help_banner();
    if !banner.is_empty() {
        println!("{banner}");
        println!();
    }
    println!("{}", components::ROOT_COMMAND_SUMMARY);
}

pub fn configure(command: &mut clap::Command, show_banner: bool, is_root: bool) {
    let mut configured = command.clone().term_width(80);

    if !is_root {
        configured = configured.disable_help_subcommand(true);
    }

    if show_banner && is_root {
        let banner = components::pay_help_banner();
        if !banner.is_empty() {
            configured = configured.before_help(banner);
        }
    }

    if is_root {
        configured = configured
            .help_template(components::ROOT_HELP_TEMPLATE)
            .after_help(components::ROOT_COMMAND_SUMMARY);
    }

    *command = configured;

    for subcommand in command.get_subcommands_mut() {
        configure(subcommand, show_banner, false);
    }
}

pub fn command_catalog(command: &clap::Command) -> serde_json::Value {
    let mut flat_commands = Vec::new();
    let root_path = vec![command.get_name().to_string()];
    let commands = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| command_catalog_entry(subcommand, &root_path, &mut flat_commands))
        .collect::<Vec<_>>();

    serde_json::json!({
        "usage": command_usage(command, &[command.get_name().to_string()]),
        "hint": "Run `pay help <command>` for command-specific usage.",
        "categories": {
            "supported_pass_through": components::SUPPORTED_PASS_THROUGH_COMMANDS,
            "developers": components::DEVELOPER_COMMANDS,
            "agents": components::AGENT_COMMANDS,
            "account_management": components::ACCOUNT_MANAGEMENT_COMMANDS,
            "other": components::OTHER_COMMANDS,
        },
        "commands": commands,
        "flat_commands": flat_commands,
    })
}

fn command_catalog_entry(
    command: &clap::Command,
    parent_path: &[String],
    flat_commands: &mut Vec<String>,
) -> serde_json::Value {
    let mut path = parent_path.to_vec();
    path.push(command.get_name().to_string());
    let command_path = path.join(" ");
    flat_commands.push(command_path.clone());

    let subcommands = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| command_catalog_entry(subcommand, &path, flat_commands))
        .collect::<Vec<_>>();

    serde_json::json!({
        "name": command.get_name(),
        "command": command_path,
        "category": root_command_category(command.get_name(), parent_path),
        "aliases": command.get_all_aliases().collect::<Vec<_>>(),
        "short_flag_aliases": command
            .get_all_short_flag_aliases()
            .map(|alias| format!("-{alias}"))
            .collect::<Vec<_>>(),
        "long_flag_aliases": command
            .get_all_long_flag_aliases()
            .map(|alias| format!("--{alias}"))
            .collect::<Vec<_>>(),
        "summary": command_summary(command),
        "usage": command_usage(command, &path),
        "subcommands": subcommands,
    })
}

fn command_usage(command: &clap::Command, path: &[String]) -> String {
    let usage = command.clone().render_usage().to_string();
    let leaf_prefix = format!("Usage: {}", command.get_name());
    if let Some(rest) = usage.strip_prefix(&leaf_prefix) {
        format!("Usage: {}{rest}", path.join(" "))
    } else {
        usage
    }
}

fn command_summary(command: &clap::Command) -> Option<String> {
    command
        .get_about()
        .or_else(|| command.get_long_about())
        .map(|summary| summary.to_string())
}

fn root_command_category(command_name: &str, parent_path: &[String]) -> Option<&'static str> {
    if parent_path.len() != 1 {
        return None;
    }

    if components::SUPPORTED_PASS_THROUGH_COMMANDS.contains(&command_name) {
        Some("supported_pass_through")
    } else if components::DEVELOPER_COMMANDS.contains(&command_name) {
        Some("developers")
    } else if components::AGENT_COMMANDS.contains(&command_name) {
        Some("agents")
    } else if components::ACCOUNT_MANAGEMENT_COMMANDS.contains(&command_name) {
        Some("account_management")
    } else if components::OTHER_COMMANDS.contains(&command_name) {
        Some("other")
    } else {
        None
    }
}
