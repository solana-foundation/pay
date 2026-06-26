use owo_colors::OwoColorize;

use super::boxed;

/// Show a service's endpoints — all of them, or filtered to one resource.
#[derive(clap::Args)]
pub struct ShowCommand {
    /// Service name or FQN (e.g. "bigquery" or "solana-foundation/google/bigquery").
    pub service: String,

    /// Resource name to filter by (e.g. "jobs", "datasets"). Omit to list every
    /// endpoint for the service.
    pub resource: Option<String>,

    /// Output as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl ShowCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut catalog = pay_core::skills::blocking::load_skills()?;

        // Lazy-fetch endpoints from CDN if needed
        pay_core::skills::blocking::ensure_endpoints(&mut catalog, &self.service)?;

        let result = match &self.resource {
            Some(resource) => {
                pay_core::skills::resource_endpoints(&catalog, &self.service, resource).ok_or_else(
                    || {
                        pay_core::Error::Config(format!(
                            "No endpoints found for resource `{}` in service `{}`.",
                            resource, self.service
                        ))
                    },
                )?
            }
            None => {
                pay_core::skills::service_endpoints(&catalog, &self.service).ok_or_else(|| {
                    pay_core::Error::Config(format!(
                        "Service `{}` not found in catalog. Try `pay skills search <query>` to discover providers.",
                        self.service
                    ))
                })?
            }
        };

        if self.json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
            println!("{json}");
            return Ok(());
        }

        // Header — same framed look as `pay skills ls` / `pay skills search`:
        // bold title, then dimmed fqn and resource context.
        let title = if result.meta.title.is_empty() {
            result.service.as_str()
        } else {
            result.meta.title.as_str()
        };
        let subtitle = if result.resource.is_empty() {
            format!("fqn: {}", result.service)
        } else {
            format!("fqn: {}  ·  resource: {}", result.service, result.resource)
        };
        eprintln!();
        eprintln!("  {}", title.bold());
        eprintln!("  {}", subtitle.dimmed());

        // Endpoints as a bordered table: METHOD + price, path, description —
        // identical rendering to `pay skills search`.
        let rows: Vec<boxed::EndpointRow> = result
            .endpoints
            .iter()
            .map(|ep| boxed::EndpointRow {
                method: &ep.method,
                path: &ep.path,
                description: &ep.description,
                pricing: ep.pricing.as_ref(),
                metered: ep.pricing.is_some(),
            })
            .collect();
        eprintln!("{}", boxed::render_endpoint_table(&rows));
        eprintln!(
            "  {}",
            format!("{} endpoints", result.endpoints.len()).dimmed()
        );

        if !result.meta.service_url.is_empty() {
            eprintln!();
            eprintln!(
                "  {}",
                format!(
                    "Gateway: {}\n\n  Use `pay curl <gateway><path>` to make requests.",
                    result.meta.service_url
                )
                .dimmed()
            );
        }

        Ok(())
    }
}
