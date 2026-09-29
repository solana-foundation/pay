//! Per-model token pricing lives in `pay_core::pricing`; `pay gate inference`
//! and `sell_inference` share it.

pub use pay_core::pricing::{PricingConfig, TokenRate};
