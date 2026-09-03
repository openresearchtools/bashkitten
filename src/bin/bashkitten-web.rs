use anyhow::Result;
use bashkitten::config::AppConfig;
use bashkitten::paths::AppPaths;

#[tokio::main]
async fn main() -> Result<()> {
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    let config = AppConfig::load(&paths)?;
    bashkitten::web::serve(paths, config).await
}
