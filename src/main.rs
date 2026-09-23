use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

const HELP: &str = "\
dunlin — self-hosted uptime and server monitoring

USAGE:
    dunlin [--config <path>]      run the server (default config: dunlin.toml)
    dunlin hash-password          read a password on stdin, print an argon2 hash
    dunlin check-config [--config <path>]   validate a config and exit
    dunlin --version
    dunlin --help
";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") || args.is_empty() {
        print!("{HELP}");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("dunlin {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let config_path = config_path(&args)?;

    match args.first().map(String::as_str) {
        Some("hash-password") => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("reading password from stdin")?;
            let password = input.trim_end_matches(['\n', '\r']);
            let hash = dunlin::auth::hash_password(password)?;
            println!("{hash}");
            Ok(())
        }
        Some("check-config") => {
            dunlin::config::load(&config_path)?;
            println!("{} is valid", config_path.display());
            Ok(())
        }
        Some(other) if other.starts_with('-') => {
            init_tracing();
            dunlin::app::run(config_path).await
        }
        Some(other) => bail!("unknown command {other:?}\n\n{HELP}"),
        None => {
            init_tracing();
            dunlin::app::run(config_path).await
        }
    }
}

fn config_path(args: &[String]) -> Result<PathBuf> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--config" || arg == "-c" {
            if let Some(p) = it.next() {
                return Ok(PathBuf::from(p));
            }
            bail!("--config needs a path");
        }
    }
    Ok(PathBuf::from("dunlin.toml"))
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
