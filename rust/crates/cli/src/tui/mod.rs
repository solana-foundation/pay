//! Interactive TUI for configuring a payment session.
//!
//! Shown before making requests when automatic payment is not pre-approved.
//! Lets the user set a spending cap and session duration — all 402
//! challenges within that budget/time are then paid automatically.

mod claude;
pub mod inference;
mod session;
mod term;
mod theme;
mod topup;
mod widgets;

pub use claude::{ClaudeProviderChoice, ClaudeProviderSelection, select_claude_provider};
pub use inference::{InferenceTuiArgs, run_inference_tui};
pub use session::{SessionSetup, setup_session};
pub use topup::{TopupCompletion, run_topup_flow};
