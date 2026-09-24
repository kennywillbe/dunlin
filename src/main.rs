use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

const HELP: &str = "\
dunlin — self-hosted uptime and server monitoring

USAGE:
    dunlin [--config <path>]                run the server (default config: dunlin.toml)
    dunlin hash-password                    read a password on stdin, print an argon2 hash
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

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Cli {
    Help,
    Version,
    /// `None` runs the server.
    Command(Option<String>, PathBuf),
}

/// Flags and the command may come in any order; `--config` takes the next
/// argument whatever it looks like.
fn parse_args(args: &[String]) -> Result<Cli> {
    let mut config = PathBuf::from("dunlin.toml");
    let mut command = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(Cli::Help),
            "--version" | "-V" => return Ok(Cli::Version),
            "--config" | "-c" => match it.next() {
                Some(p) => config = PathBuf::from(p),
                None => bail!("--config needs a path"),
            },
            other if other.starts_with('-') => bail!("unknown option {other:?}\n\n{HELP}"),
            other => {
                if command.is_some() {
                    bail!("unexpected argument {other:?}\n\n{HELP}");
                }
                command = Some(other.to_string());
            }
        }
    }
    Ok(Cli::Command(command, config))
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, config_path) = match parse_args(&args)? {
        Cli::Help => {
            print!("{HELP}");
            return Ok(());
        }
        Cli::Version => {
            println!("dunlin {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Cli::Command(command, config) => (command, config),
    };

    match command.as_deref() {
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
        Some(other) => bail!("unknown command {other:?}\n\n{HELP}"),
        None => {
            init_tracing();
            dunlin::app::run(config_path).await
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli> {
        parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    fn cmd(c: Option<&str>, path: &str) -> Cli {
        Cli::Command(c.map(str::to_string), PathBuf::from(path))
    }

    #[test]
    fn no_arguments_runs_the_server_with_the_default_config() {
        assert_eq!(parse(&[]).unwrap(), cmd(None, "dunlin.toml"));
    }

    #[test]
    fn command_and_config_in_either_order() {
        let want = cmd(Some("check-config"), "x.toml");
        assert_eq!(
            parse(&["check-config", "--config", "x.toml"]).unwrap(),
            want
        );
        assert_eq!(
            parse(&["--config", "x.toml", "check-config"]).unwrap(),
            want
        );
        assert_eq!(parse(&["-c", "x.toml"]).unwrap(), cmd(None, "x.toml"));
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["check-config", "extra"]).is_err());
        assert_eq!(parse(&["check-config", "--help"]).unwrap(), Cli::Help);
    }
}
