//! `pay policy use <name>` — set the default policy.

use pay_core::policy::PolicyStore;

#[derive(clap::Args)]
pub struct UseCommand {
    /// Policy name. Pass an empty string `""` (or `--clear`) to remove the
    /// default and revert to no-policy-by-default behavior.
    pub name: Option<String>,

    /// Clear the default instead of setting one.
    #[arg(long, conflicts_with = "name")]
    pub clear: bool,
}

impl UseCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = pay_core::policy::FilePolicyStore::default_path();
        let mut file = PolicyStore::load_policies(&store)?;

        if self.clear {
            file.default = None;
            PolicyStore::save_policies(&store, &file)?;
            crate::components::print_notice(
                crate::components::NoticeLevel::Success,
                "Cleared default policy",
                "Paid requests will run without policy enforcement unless --policy or per-account binding is set.",
            );
            return Ok(());
        }

        let name = self.name.ok_or_else(|| {
            pay_core::Error::Config("provide a policy name or pass --clear".to_string())
        })?;
        file.set_default(&name).ok_or_else(|| {
            pay_core::Error::Config(format!("policy `{name}` not found"))
        })?;
        PolicyStore::save_policies(&store, &file)?;
        crate::components::print_notice(
            crate::components::NoticeLevel::Success,
            &format!("Default policy is now `{name}`"),
            "Paid requests will use this policy unless --policy <other> overrides.",
        );
        Ok(())
    }
}
