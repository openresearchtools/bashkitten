use anyhow::Result;
use bashkitten::paths::AppPaths;
use clap::Parser;

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    session: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    bashkitten::worker::run_worker(paths, arguments.session).await
}
