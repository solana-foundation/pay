//! Isolated provider-metadata maintenance. Never initializes settlement or Redis.

use pay_worker::resource_cleanup::ResourceCleaner;

fn dry_run(value: Option<&str>) -> Result<bool, &'static str> {
    match value {
        None | Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err("DRY_RUN must be exactly true or false"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let configured = match std::env::var("DRY_RUN") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let dry_run = dry_run(configured.as_deref())?;
    let cleaner = ResourceCleaner::for_reconciliation()?;
    let candidates = cleaner.reconcile_orphans(dry_run).await?;
    tracing::info!(dry_run, candidates, "resource reconciliation completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutations_require_explicit_false() {
        assert_eq!(dry_run(None), Ok(true));
        assert_eq!(dry_run(Some("true")), Ok(true));
        assert_eq!(dry_run(Some("false")), Ok(false));
        for invalid in ["", "0", "FALSE", "no", " false "] {
            assert!(dry_run(Some(invalid)).is_err());
        }
    }
}
