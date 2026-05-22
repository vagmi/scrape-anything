use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{SqlitePool, migrate::Migrator};
use std::path::Path;
use std::str::FromStr;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

pub async fn open(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path))
        .with_context(|| format!("parsing sqlite URL for {path}"))?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .context("opening sqlite pool")?;

    MIGRATOR.run(&pool).await.context("running migrations")?;
    Ok(pool)
}

#[allow(dead_code)]
pub fn ensure_parent_dir(path: &str) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    Ok(())
}
