//! Reusable display components for CLI output (links, notices, etc.).

pub mod link;
pub mod notice;

pub use link::{link, link_with_arrow};
pub use notice::{NoticeLevel, notice};
