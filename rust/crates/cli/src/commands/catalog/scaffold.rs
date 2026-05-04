//! `pay catalog scaffold` — generate a starter `PAY.md` for a new provider.
//!
//! Fetches the OpenAPI document at `<openapi_url>`, derives `title` and
//! `description` from `info.{title,description}`, infers `service_url` from
//! `servers[0].url` (or by stripping the OpenAPI suffix), and writes a `PAY.md`
//! file in the current directory with the same frontmatter shape used by
//! providers in `pay-skills/providers/`.
//!
//! Fields that need human curation (`use_case`, `category`) are emitted as
//! `TODO` placeholders so the author reviews them before opening a PR.
//!
//! No network or filesystem state is mutated beyond writing the output file —
//! the OpenAPI doc itself is only read.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use owo_colors::OwoColorize;
use reqwest::blocking::Client;
use serde_json::Value;

const FETCH_TIMEOUT_SECS: u64 = 30;

/// Scaffold a starter `PAY.md` for a new provider from its OpenAPI document.
#[derive(clap::Args)]
pub struct ScaffoldCommand {
    /// Fully-qualified provider name, e.g. `quicknode/rpc`. The leaf segment
    /// becomes the `name:` field; the full FQN is used as a comment hint.
    pub fqn: String,

    /// URL to the OpenAPI document (JSON or YAML).
    pub openapi_url: String,

    /// Output directory (the file is always written as `<dir>/<fqn>/PAY.md`).
    #[arg(long, default_value = ".")]
    pub output_dir: PathBuf,

    /// Overwrite the output file if it already exists.
    #[arg(long)]
    pub force: bool,
}

impl ScaffoldCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let leaf = self
            .fqn
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| pay_core::Error::Config(format!("invalid fqn `{}`", self.fqn)))?;

        let target_dir = self.output_dir.join(&self.fqn);
        let output = target_dir.join("PAY.md");
        if output.exists() && !self.force {
            return Err(pay_core::Error::Config(format!(
                "{} already exists; pass --force to overwrite",
                output.display()
            )));
        }
        fs::create_dir_all(&target_dir)
            .map_err(|e| pay_core::Error::Config(format!("mkdir {}: {e}", target_dir.display())))?;

        eprintln!("Fetching OpenAPI from {}", self.openapi_url.bold());
        let body = fetch_openapi(&self.openapi_url)?;
        let doc: Value = parse_openapi(&body)?;

        let info = doc.get("info");
        let title = info
            .and_then(|v| v.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| leaf.to_string());

        let description_full = info
            .and_then(|v| v.get("description"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let description = description_full.as_deref().map(first_sentence);

        let service_url = doc
            .get("servers")
            .and_then(Value::as_array)
            .and_then(|servers| servers.first())
            .and_then(|s| s.get("url"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('/'))
            .map(String::from)
            .unwrap_or_else(|| derive_service_url(&self.openapi_url));

        let content = render_pay_md(RenderArgs {
            fqn: &self.fqn,
            name: leaf,
            title: &title,
            description: description.as_deref(),
            service_url: &service_url,
            openapi_url: &self.openapi_url,
            body: description_full.as_deref(),
        });

        fs::write(&output, content)
            .map_err(|e| pay_core::Error::Config(format!("write {}: {e}", output.display())))?;

        eprintln!(
            "Wrote {} ({} -> {})",
            output.display().to_string().bold(),
            self.fqn.dimmed(),
            service_url.dimmed()
        );
        eprintln!(
            "{}",
            "Review the TODO fields (use_case, category) before publishing.".dimmed()
        );

        Ok(())
    }
}

fn fetch_openapi(url: &str) -> pay_core::Result<String> {
    let client = Client::builder()
        .user_agent(format!(
            "pay-catalog-scaffold/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| pay_core::Error::Config(format!("http client: {e}")))?;

    let resp = client
        .get(url)
        .send()
        .map_err(|e| pay_core::Error::Config(format!("fetch {url}: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| pay_core::Error::Config(format!("read {url}: {e}")))?;
    if !status.is_success() {
        return Err(pay_core::Error::Config(format!(
            "fetch {url} returned {status}"
        )));
    }
    Ok(body)
}

fn parse_openapi(body: &str) -> pay_core::Result<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return Ok(v);
    }
    serde_yml::from_str::<Value>(body)
        .map_err(|e| pay_core::Error::Config(format!("parse OpenAPI doc (not JSON or YAML): {e}")))
}

/// Strip a trailing `/openapi.json`, `/openapi.yaml`, etc. from the doc URL to
/// approximate the upstream service base. Falls back to scheme://host.
fn derive_service_url(openapi_url: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "/openapi.json",
        "/openapi.yaml",
        "/openapi.yml",
        "/swagger.json",
        "/swagger.yaml",
    ];
    for suffix in SUFFIXES {
        if let Some(stripped) = openapi_url.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    if let Some((scheme_host, _)) = openapi_url.split_once("://").and_then(|(scheme, rest)| {
        rest.split_once('/')
            .map(|(host, path)| (format!("{scheme}://{host}"), path))
    }) {
        return scheme_host;
    }
    openapi_url.to_string()
}

/// Pull the first sentence (up to ~255 chars) for use as a one-line description.
/// Skips leading markdown heading lines (`# Foo`) since those make poor
/// descriptions — many OpenAPI `info.description` fields embed a full README.
fn first_sentence(text: &str) -> String {
    let body = text
        .lines()
        .find(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .unwrap_or("");
    let trimmed = body.trim();
    let mut end = trimmed.len();
    if let Some(idx) = trimmed.find(". ") {
        end = idx + 1;
    }
    let mut out = trimmed[..end].trim().to_string();
    if out.len() > 250 {
        out.truncate(250);
        out.push('…');
    }
    out
}

struct RenderArgs<'a> {
    fqn: &'a str,
    name: &'a str,
    title: &'a str,
    description: Option<&'a str>,
    service_url: &'a str,
    openapi_url: &'a str,
    body: Option<&'a str>,
}

fn render_pay_md(args: RenderArgs<'_>) -> String {
    let body = args
        .body
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            format!(
                "TODO: write a short overview of what {} offers and when an agent should reach for it.",
                args.title
            )
        });

    let description_line = match args.description {
        Some(desc) => format!("description: {}", yaml_quote(desc)),
        None => format!("description: TODO  # one-sentence pitch for {}", args.title),
    };

    format!(
        "---\n\
         name: {name}\n\
         title: {title}\n\
         {description_line}\n\
         use_case: TODO  # describe when an agent should pick {fqn} over alternatives.\n\
         category: TODO  # one of: ai_ml, analytics, cloud, compute, data, devtools, finance, identity, iot, maps, media, messaging, other, productivity, search, security, storage, translation\n\
         service_url: {service_url}\n\
         openapi:\n  url: {openapi_url}\n\
         ---\n\
         \n\
         {body}\n\
         \n\
         ## Spend-aware usage\n\
         \n\
         - TODO: list patterns that minimize paid calls (e.g. prefer narrow lookups\n  over broad searches; reuse identifiers; cap result limits).\n",
        name = args.name,
        title = yaml_quote(args.title),
        fqn = args.fqn,
        service_url = args.service_url,
        openapi_url = args.openapi_url,
        body = body,
    )
}

/// Wrap a value in double quotes, escaping `"` and `\` so the result is a valid
/// YAML double-quoted scalar. Used for free-form fields like title/description
/// where the upstream text may contain colons or other YAML metacharacters.
fn yaml_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_service_url_strips_openapi_suffix() {
        assert_eq!(
            derive_service_url("https://x402.quicknode.com/openapi.json"),
            "https://x402.quicknode.com"
        );
        assert_eq!(
            derive_service_url("https://api.example.com/v1/openapi.yaml"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn derive_service_url_falls_back_to_host() {
        assert_eq!(
            derive_service_url("https://api.example.com/v1/spec"),
            "https://api.example.com"
        );
    }

    #[test]
    fn first_sentence_truncates_at_period() {
        assert_eq!(
            first_sentence("Foo bar baz. More text here."),
            "Foo bar baz."
        );
    }

    #[test]
    fn first_sentence_truncates_at_newline() {
        assert_eq!(first_sentence("Foo bar\nMore text"), "Foo bar");
    }

    #[test]
    fn first_sentence_skips_markdown_headings() {
        let text = "# Some Title\n\nThe real description sentence. More stuff here.";
        assert_eq!(first_sentence(text), "The real description sentence.");
    }

    #[test]
    fn yaml_quote_escapes_quotes_and_backslashes() {
        assert_eq!(yaml_quote("a \"b\" \\c"), "\"a \\\"b\\\" \\\\c\"");
    }

    #[test]
    fn render_pay_md_includes_frontmatter_and_body() {
        let out = render_pay_md(RenderArgs {
            fqn: "quicknode/rpc",
            name: "rpc",
            title: "QuickNode",
            description: Some("JSON-RPC for many chains."),
            service_url: "https://x402.quicknode.com",
            openapi_url: "https://x402.quicknode.com/openapi.json",
            body: Some("Multi-chain RPC with x402 micropayments."),
        });
        assert!(out.starts_with("---\n"));
        assert!(out.contains("name: rpc"));
        assert!(out.contains("title: \"QuickNode\""));
        assert!(out.contains("description: \"JSON-RPC for many chains.\""));
        assert!(out.contains(
            "use_case: TODO  # describe when an agent should pick quicknode/rpc over alternatives."
        ));
        assert!(out.contains("category: TODO"));
        assert!(out.contains("openapi:\n  url: https://x402.quicknode.com/openapi.json"));
        assert!(out.contains("Multi-chain RPC with x402 micropayments."));
        assert!(out.contains("## Spend-aware usage"));
    }

    #[test]
    fn render_pay_md_uses_todo_placeholder_when_description_missing() {
        let out = render_pay_md(RenderArgs {
            fqn: "syra/PAY",
            name: "PAY",
            title: "Syra",
            description: None,
            service_url: "https://example.com",
            openapi_url: "https://example.com/openapi.json",
            body: None,
        });
        assert!(out.contains("description: TODO  # one-sentence pitch for Syra"));
    }
}
