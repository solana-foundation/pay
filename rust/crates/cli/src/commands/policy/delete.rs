//! `pay policy delete <name>` — remove a policy and its tracked spend.

use dialoguer::Confirm;
use pay_core::policy::PolicyStore;

#[derive(clap::Args)]
pub struct DeleteCommand {
    pub name: String,

    /// Skip the confirmation prompt.
    #[arg(long, alias = "force")]
    pub yes: bool,
}

impl DeleteCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = pay_core::policy::FilePolicyStore::default_path();
        let mut file = PolicyStore::load_policies(&store)?;
        if file.get(&self.name).is_none() {
            return Err(pay_core::Error::Config(format!(
                "policy `{}` not found",
                self.name
            )));
        }

        if !self.yes && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            let confirmed = Confirm::new()
                .with_prompt(format!("Delete policy `{}`? This cannot be undone.", self.name))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("prompt error: {e}")))?;
            if !confirmed {
                eprintln!("Aborted.");
                return Ok(());
            }
        }

        file.remove(&self.name);
        PolicyStore::save_policies(&store, &file)?;

        let mut state = PolicyStore::load_state(&store)?;
        state.forget(&self.name);
        PolicyStore::save_state(&store, &state)?;

        crate::components::print_notice(
            crate::components::NoticeLevel::Success,
            &format!("Deleted policy `{}`", self.name),
            "Tracked spend cleared.",
        );
        Ok(())
    }
}
