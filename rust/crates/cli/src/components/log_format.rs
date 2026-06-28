//! A `tracing` event formatter that renders log lines in the CLI's notice
//! style — a colored vertical rail (`│`) by level, the message, and dimmed
//! fields — so the running server's logs match the ASCII-art startup output
//! instead of the raw `timestamp LEVEL target: msg …` format.
//!
//! OTel metric carriers (`monotonic_counter.*` / `histogram.*` / `gauge.*` /
//! `metric`) are dropped — they're machine signals, noise in a human log.

use std::fmt;

use owo_colors::OwoColorize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Notice-styled event formatter (colored `│` rail + message + dimmed fields).
pub struct NoticeFormat;

impl<S, N> FormatEvent<S, N> for NoticeFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let rail = match *event.metadata().level() {
            Level::ERROR => "│".red().bold().to_string(),
            Level::WARN => "│".yellow().bold().to_string(),
            Level::INFO => "│".blue().bold().to_string(),
            Level::DEBUG | Level::TRACE => "│".dimmed().to_string(),
        };

        let mut visitor = NoticeVisitor::default();
        event.record(&mut visitor);

        write!(writer, "{rail} {}", visitor.message)?;
        if !visitor.fields.is_empty() {
            write!(writer, " {}", visitor.fields.dimmed())?;
        }
        writeln!(writer)
    }
}

/// Collects the `message` field plus non-metric structured fields.
#[derive(Default)]
struct NoticeVisitor {
    message: String,
    fields: String,
}

impl Visit for NoticeVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();
        // Drop OTel metric carriers and the `log`-crate bridge's location
        // fields (pingora logs via `log`) — both are noise in a human log.
        if name.starts_with("monotonic_counter.")
            || name.starts_with("histogram.")
            || name.starts_with("gauge.")
            || name.starts_with("log.")
            || name == "metric"
        {
            return;
        }
        if name == "message" {
            // The message is recorded as `fmt::Arguments`, whose Debug is the
            // formatted text (no surrounding quotes).
            self.message = format!("{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            self.fields.push_str(&format!("{name}={value:?}"));
        }
    }
}
