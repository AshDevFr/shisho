use std::path::PathBuf;

use anyhow::Context;

pub async fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);

    let dek = shisho_db::keys::bootstrap_dek(&cfg)?;
    let db = shisho_db::connect(&cfg, dek.as_ref()).await?;
    shisho_db::run_migrations(&db).await?;
    tracing::info!("migrations complete");
    Ok(())
}
