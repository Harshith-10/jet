use std::{env, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand, ValueEnum};
use jet_pack::{JavaCorrettoUpdater, PackageManager, PythonStandaloneUpdater};

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
        language: UpdateTarget,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum UpdateTarget {
    Java,
    Python,
    All,
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
    language: UpdateTarget,
    manager: &PackageManager,
) -> Result<usize, jet_pack::JetPackError> {
    match language {
        UpdateTarget::Java => manager
            .update_manifests_with_updater(&JavaCorrettoUpdater::default())
            .map(|paths| paths.len()),
        UpdateTarget::Python => manager
            .update_manifests_with_updater(&PythonStandaloneUpdater)
            .map(|paths| paths.len()),
        UpdateTarget::All => {
            let java = manager
                .update_manifests_with_updater(&JavaCorrettoUpdater::default())?
                .len();
            let python = manager
                .update_manifests_with_updater(&PythonStandaloneUpdater)?
                .len();
            Ok(java + python)
        }
    }
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
