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

        // Headline: Title-Cased message + the request location (`subdomain/path`)
        // when present, e.g. `Payment Settlement Failed - localhost:1402/api/v1/x`.
        // Raw structured fields are intentionally omitted from the human log —
        // they're carried in the OTLP/JSON layers for machines.
        write!(writer, "{rail} {}", titleize(&visitor.message))?;
        if let Some(location) = visitor.location() {
            write!(writer, " - {}", location.dimmed())?;
        }
        writeln!(writer)?;

        // The error detail, if any, on its own line beneath the headline.
        if let Some(error) = &visitor.error {
            writeln!(writer, "{rail} {}", error.dimmed())?;
        }
        Ok(())
    }
}

/// Collects the `message`, the request location (`subdomain`/`path`), the
/// `error` detail, and any remaining non-metric structured fields.
#[derive(Default)]
struct NoticeVisitor {
    message: String,
    subdomain: Option<String>,
    path: Option<String>,
    error: Option<String>,
}

impl NoticeVisitor {
    /// `subdomain/path` breadcrumb for the headline, if a path was logged.
    fn location(&self) -> Option<String> {
        let path = self.path.as_ref()?;
        let path = path.trim_start_matches('/');
        Some(match &self.subdomain {
            Some(sub) => format!("{sub}/{path}"),
            None => path.to_string(),
        })
    }
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
        // The message is recorded as `fmt::Arguments` (Debug == the formatted
        // text); structured values may arrive quoted, so unquote the ones we
        // surface specially.
        let rendered = format!("{value:?}");
        match name {
            "message" => self.message = rendered,
            "subdomain" => self.subdomain = Some(unquote(&rendered)),
            "path" => self.path = Some(unquote(&rendered)),
            "error" => self.error = Some(unquote(&rendered)),
            // Other structured fields are dropped from the human log.
            _ => {}
        }
    }
}

/// Title-Case a message: uppercase the first character of each word, leaving the
/// rest untouched (so `Http402Gate`-style casing survives).
fn titleize(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip one layer of surrounding double quotes from a Debug-rendered value.
fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}
