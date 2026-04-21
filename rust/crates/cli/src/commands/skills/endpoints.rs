use owo_colors::OwoColorize;

/// List endpoints for a resource within a service.
#[derive(clap::Args)]
pub struct EndpointsCommand {
    /// Service name or FQN (e.g. "bigquery" or "solana-foundation/google/bigquery").
    pub service: String,

    /// Resource name (e.g. "jobs", "datasets").
    pub resource: String,

    /// Output as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl EndpointsCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut catalog = pay_core::skills::load_skills()?;

        // Lazy-fetch endpoints from CDN if needed
        pay_core::skills::ensure_endpoints(&mut catalog, &self.service)?;

        let result = pay_core::skills::resource_endpoints(&catalog, &self.service, &self.resource)
            .ok_or_else(|| {
                pay_core::Error::Config(format!(
                    "No endpoints found for resource `{}` in service `{}`.",
                    self.resource, self.service
                ))
            })?;

        if self.json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
            println!("{json}");
            return Ok(());
        }

        eprintln!(
            "  {}/{} — {} endpoints\n",
            result.service.bold(),
            result.resource.bold(),
            result.endpoints.len()
        );

        for ep in &result.endpoints {
            let method_colored = match ep.method.as_str() {
                "GET" => ep.method.green().to_string(),
                "POST" => ep.method.blue().to_string(),
                "PUT" | "PATCH" => ep.method.yellow().to_string(),
                "DELETE" => ep.method.red().to_string(),
                _ => ep.method.dimmed().to_string(),
            };

            let path = &ep.path;

            let metered_indicator = if ep.pricing.is_some() { "$" } else { "" };
            eprintln!(
                "  {:<7} {} {}",
                method_colored,
                path,
                metered_indicator.yellow()
            );

            if !ep.description.is_empty() {
                let desc = if ep.description.len() > 80 {
                    format!("{}...", &ep.description[..77])
                } else {
                    ep.description.clone()
                };
                eprintln!("          {}", desc.dimmed());
            }
        }

        if !result.service_url.is_empty() {
            eprintln!();
            eprintln!(
                "  {}",
                format!(
                    "Gateway: {}\nUse `pay curl <gateway><path>` to make requests.",
                    result.service_url
                )
                .dimmed()
            );
        }

        Ok(())
    }
}
