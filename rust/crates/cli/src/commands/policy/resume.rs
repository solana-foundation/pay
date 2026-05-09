//! `pay policy resume [name]` — un-pause a policy.

use pay_core::policy::PolicyStore;

use super::{load_policy_or_error, resolve_target_name};

#[derive(clap::Args)]
pub struct ResumeCommand {
    /// Policy name. Defaults to the configured default.
    pub name: Option<String>,
}

impl ResumeCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = pay_core::policy::FilePolicyStore::default_path();
        let mut file = PolicyStore::load_policies(&store)?;
        let target = resolve_target_name(self.name.as_deref(), &file)?;
        let (_, mut policy) = load_policy_or_error(&store, &target)?;
        if !policy.paused {
            crate::components::print_notice(
                crate::components::NoticeLevel::Info,
                &format!("Policy `{target}` was not paused"),
                "No change.",
            );
            return Ok(());
        }
        policy.paused = false;
        file.upsert(policy);
        PolicyStore::save_policies(&store, &file)?;
        crate::components::print_notice(
            crate::components::NoticeLevel::Success,
            &format!("Resumed policy `{target}`"),
            "Future paid requests will be evaluated against the policy rules.",
        );
        Ok(())
    }
}
