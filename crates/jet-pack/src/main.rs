use std::{env, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand, ValueEnum};
use jet_pack::{PackageManager, get_updaters};

#[derive(Debug, Parser)]
#[command(name = "jet-pack", about = "Jet package and manifest manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, env = "JET_RUNTIME_DIR")]
    runtime_dir: Option<PathBuf>,

    #[arg(long, env = "JET_RUNTIME_MANIFEST_DIR")]
    manifest_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Update {
        #[arg(value_enum)]
        language: CliUpdateTarget,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliUpdateTarget {
    Java,
    Python,
    Zig,
    All,
}

impl From<CliUpdateTarget> for jet_pack::UpdateTarget {
    fn from(t: CliUpdateTarget) -> Self {
        match t {
            CliUpdateTarget::Java => Self::Java,
            CliUpdateTarget::Python => Self::Python,
            CliUpdateTarget::Zig => Self::Zig,
            CliUpdateTarget::All => Self::All,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime_dir = cli.runtime_dir.unwrap_or_else(default_runtime_dir);
    let manifest_dir = cli
        .manifest_dir
        .unwrap_or_else(|| runtime_dir.join("manifests"));

    let manager = PackageManager::new(runtime_dir, manifest_dir);

    let result = match cli.command {
        Command::Update { language } => run_update(language, &manager),
    };

    match result {
        Ok(count) => {
            println!("Updated {count} manifest(s)");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("update failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_update(
    language: CliUpdateTarget,
    manager: &PackageManager,
) -> Result<usize, jet_pack::JetPackError> {
    let updaters = get_updaters(language.into());
    let mut total = 0;
    for updater in &updaters {
        total += manager
            .update_manifests_with_updater(updater.as_ref())?
            .len();
    }
    Ok(total)
}

fn default_runtime_dir() -> PathBuf {
    if let Ok(path) = env::var("JET_RUNTIME_DIR") {
        return PathBuf::from(path);
    }

    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".jet").join("runtimes");
    }

    PathBuf::from("/var/lib/jet/runtimes")
}
