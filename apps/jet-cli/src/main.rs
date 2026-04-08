use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use console::{Emoji, Style, style};
use futures::future::join_all;
use indicatif::{ProgressBar, ProgressStyle};
use jet_core::{FileRequest, JetConfig, JobRequest, JobResult};
use jet_pack::{PackageManager, get_updaters};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(name = "jet-cli", version, about = "Jet CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Exec(ExecArgs),
    Benchmark(BenchmarkArgs),
    Runtimes(RuntimesCommand),
    Server(ServerCommand),
}

#[derive(Args, Debug, Clone)]
struct ExecArgs {
    language: String,

    #[arg(short = 'f', long = "file", required = true)]
    files: Vec<PathBuf>,

    #[arg(short = 'v', long = "version", default_value = "latest")]
    version: String,

    #[arg(long = "stdin")]
    stdin_file: Option<PathBuf>,

    #[arg(long = "server", default_value = "http://127.0.0.1:4000")]
    server: String,

    #[arg(long = "poll-interval", value_parser = parse_duration, default_value = "200ms")]
    poll_interval: Duration,

    #[arg(long = "poll-timeout", value_parser = parse_duration, default_value = "60s")]
    poll_timeout: Duration,

    #[arg(long = "run-timeout-ms")]
    run_timeout: Option<u64>,

    #[arg(long = "compile-timeout-ms")]
    compile_timeout: Option<u64>,

    #[arg(long = "run-memory-bytes")]
    run_memory_limit: Option<u64>,

    #[arg(long = "compile-memory-bytes")]
    compile_memory_limit: Option<u64>,

    #[arg(long = "run-output-bytes")]
    run_output_limit: Option<u64>,

    #[arg(long = "compile-output-bytes")]
    compile_output_limit: Option<u64>,
}

#[derive(Args, Debug, Clone)]
struct BenchmarkArgs {
    #[arg(help = "Language to benchmark")]
    language: String,

    #[arg(
        short = 'v',
        long = "version",
        default_value = "latest",
        help = "Language version (resolves to latest patch)"
    )]
    version: String,

    #[arg(
        short = 'f',
        long = "file",
        required = true,
        help = "Source files to execute"
    )]
    files: Vec<PathBuf>,

    #[arg(
        short = 'c',
        long = "concurrency",
        default_value_t = 5,
        help = "Number of concurrent workers"
    )]
    concurrency: usize,

    #[arg(
        short = 'n',
        long = "requests",
        default_value_t = 100,
        help = "Total number of requests to perform"
    )]
    requests: usize,

    #[arg(short = 'd', long = "delay", value_parser = parse_duration, default_value = "500ms", help = "Delay between requests for each worker")]
    delay: Duration,

    #[arg(long = "stdin", help = "Input file for stdin")]
    stdin_file: Option<PathBuf>,

    #[arg(
        long = "server",
        default_value = "http://127.0.0.1:4000",
        help = "Jet server URL"
    )]
    server: String,

    #[arg(long = "poll-interval", value_parser = parse_duration, default_value = "500ms", help = "Interval to poll job status")]
    poll_interval: Duration,

    #[arg(long = "poll-timeout", value_parser = parse_duration, default_value = "60s", help = "Timeout for single job completion")]
    poll_timeout: Duration,

    #[arg(long = "run-timeout-ms")]
    run_timeout: Option<u64>,

    #[arg(long = "compile-timeout-ms")]
    compile_timeout: Option<u64>,

    #[arg(long = "run-memory-bytes")]
    run_memory_limit: Option<u64>,

    #[arg(long = "compile-memory-bytes")]
    compile_memory_limit: Option<u64>,

    #[arg(long = "run-output-bytes")]
    run_output_limit: Option<u64>,

    #[arg(long = "compile-output-bytes")]
    compile_output_limit: Option<u64>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum UpdateTarget {
    Java,
    Python,
    Zig,
    All,
}

impl From<UpdateTarget> for jet_pack::UpdateTarget {
    fn from(t: UpdateTarget) -> Self {
        match t {
            UpdateTarget::Java => Self::Java,
            UpdateTarget::Python => Self::Python,
            UpdateTarget::Zig => Self::Zig,
            UpdateTarget::All => Self::All,
        }
    }
}

#[derive(Subcommand, Debug)]
enum RuntimesSubcommands {
    List,
    Install {
        language: String,
        version: String,
        #[arg(long = "arch", default_value_t = default_arch())]
        arch: String,
    },
    Uninstall {
        language: String,
        version: String,
    },
    Update {
        #[arg(value_enum, default_value = "all")]
        language: UpdateTarget,
    },
    Clean,
}

#[derive(Args, Debug)]
struct RuntimesCommand {
    #[command(subcommand)]
    command: RuntimesSubcommands,
}

#[derive(Subcommand, Debug)]
enum ServerSubcommands {
    Run {
        #[arg(long = "release", default_value_t = false)]
        release: bool,
    },
    GenerateSystemd {
        #[arg(long = "output", default_value = "jet-server.service")]
        output: PathBuf,
        #[arg(long = "user", default_value = "harshu")]
        user: String,
    },
}

#[derive(Args, Debug)]
struct ServerCommand {
    #[command(subcommand)]
    command: ServerSubcommands,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SubmitJobResponse {
    job_id: String,
    status: String,
    resolved_version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct JobStateRecord {
    job_id: String,
    status: String,
    language: String,
    version: String,
    result: Option<JobResult>,
    error: Option<String>,
}

/// Detect the host CPU architecture and normalise it to the key used in
/// runtime manifests (`"x86_64"` or `"aarch64"`).
fn default_arch() -> String {
    match std::env::consts::ARCH {
        "amd64" | "x86_64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        other => other,
    }
    .to_string()
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Exec(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to initialize async runtime")?;
            rt.block_on(execute_command(args))
        }
        Commands::Benchmark(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to initialize async runtime")?;
            rt.block_on(benchmark_command(args))
        }
        Commands::Runtimes(cmd) => runtimes_command(cmd),
        Commands::Server(cmd) => server_command(cmd),
    }
}

async fn execute_command(args: ExecArgs) -> Result<()> {
    let config = JetConfig::load()?;
    let manager = PackageManager::new(
        config.runtime_install_dir.clone(),
        config.runtimes_manifest_dir.clone(),
    );
    let resolver = manager.build_resolver()?;
    let resolved_version = resolver
        .resolve(&args.language, &args.version)?
        .ok_or_else(|| {
            anyhow!(
                "unsupported version: {} for {}",
                args.version,
                args.language
            )
        })?;

    let mut request = build_job_request(
        &args.language,
        &resolved_version,
        &args.files,
        args.stdin_file.as_deref(),
        &CommonLimits {
            run_timeout: args.run_timeout,
            compile_timeout: args.compile_timeout,
            run_memory_limit: args.run_memory_limit,
            compile_memory_limit: args.compile_memory_limit,
            run_output_limit: args.run_output_limit,
            compile_output_limit: args.compile_output_limit,
        },
    )?;
    request.version = Some(resolved_version.clone());

    let client = reqwest::Client::new();
    let submit = submit_job(&client, &args.server, &request).await?;

    println!("job submitted: {}", submit.job_id);
    println!("resolved version: {}", submit.resolved_version);

    let finished = poll_job_until_done(
        &client,
        &args.server,
        &submit.job_id,
        args.poll_interval,
        args.poll_timeout,
    )
    .await?;

    print_job_state(&finished);
    Ok(())
}

async fn benchmark_command(args: BenchmarkArgs) -> Result<()> {
    if args.concurrency == 0 {
        bail!("concurrency must be >= 1");
    }
    if args.requests == 0 {
        bail!("requests must be >= 1");
    }

    let config = JetConfig::load()?;
    let manager = PackageManager::new(
        config.runtime_install_dir.clone(),
        config.runtimes_manifest_dir.clone(),
    );

    println!();
    let spinner = make_spinner();
    spinner.set_message(format!("{}Resolving version…", LOOKING_GLASS));

    let resolver = manager.build_resolver()?;
    let resolved_version = resolver
        .resolve(&args.language, &args.version)?
        .ok_or_else(|| {
            spinner.finish_and_clear();
            anyhow!(
                "unsupported version: {} for {}",
                args.version,
                args.language
            )
        })?;

    spinner.finish_with_message(format!(
        "{}Benchmark target: {}:{} ({})",
        CHECKMARK,
        style(&args.language).green(),
        style(&resolved_version).yellow(),
        style(format!(
            "concurrency={}, requests={}",
            args.concurrency, args.requests
        ))
        .dim()
    ));

    let mut request_template = build_job_request(
        &args.language,
        &resolved_version,
        &args.files,
        args.stdin_file.as_deref(),
        &CommonLimits {
            run_timeout: args.run_timeout,
            compile_timeout: args.compile_timeout,
            run_memory_limit: args.run_memory_limit,
            compile_memory_limit: args.compile_memory_limit,
            run_output_limit: args.run_output_limit,
            compile_output_limit: args.compile_output_limit,
        },
    )?;
    request_template.version = Some(resolved_version.clone());

    let pb = ProgressBar::new(args.requests as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {msg}\n  {wide_bar:.green/white.dim} {pos}/{len} ({percent}%, {remaining} left)"
        )
        .unwrap()
        .progress_chars("━━╺━"),
    );
    pb.set_message(format!("{}Running benchmark…", GAUGE));

    let start = Instant::now();
    let next_index = Arc::new(AtomicUsize::new(0));
    let success_counter = Arc::new(AtomicUsize::new(0));
    let failure_counter = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let first_error = Arc::new(Mutex::new(None));

    let mut tasks = Vec::new();
    for worker_id in 0..args.concurrency {
        let counter = next_index.clone();
        let success = success_counter.clone();
        let failure = failure_counter.clone();
        let latencies = latencies.clone();
        let first_err = first_error.clone();
        let server = args.server.clone();
        let poll_interval = args.poll_interval;
        let poll_timeout = args.poll_timeout;
        let delay = args.delay;
        let template = request_template.clone();
        let pb = pb.clone();

        tasks.push(tokio::spawn(async move {
            // Each worker uses a unique X-Forwarded-For IP so the server's
            // per-IP rate limiter gives each worker its own bucket instead of
            // throttling them all under 127.0.0.1.
            let mut headers = reqwest::header::HeaderMap::new();
            let fake_ip = format!("10.0.{}.{}", worker_id / 256, worker_id % 256);
            headers.insert(
                "X-Forwarded-For",
                reqwest::header::HeaderValue::from_str(&fake_ip).unwrap(),
            );
            let client = reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .unwrap();

            loop {
                let i = counter.fetch_add(1, Ordering::SeqCst);
                if i >= args.requests {
                    break;
                }

                let per_req_start = Instant::now();
                let out = async {
                    let submit = submit_job(&client, &server, &template).await?;
                    let state = poll_job_until_done(
                        &client,
                        &server,
                        &submit.job_id,
                        poll_interval,
                        poll_timeout,
                    )
                    .await?;
                    Ok::<JobStateRecord, anyhow::Error>(state)
                }
                .await;

                match out {
                    Ok(state) => {
                        if state.status == "completed" {
                            success.fetch_add(1, Ordering::SeqCst);
                        } else {
                            failure.fetch_add(1, Ordering::SeqCst);
                            let mut fe = first_err.lock().await;
                            if fe.is_none() {
                                *fe = Some(format!(
                                    "Job {} failed with status: {}",
                                    state.job_id, state.status
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        failure.fetch_add(1, Ordering::SeqCst);
                        let mut fe = first_err.lock().await;
                        if fe.is_none() {
                            *fe = Some(format!("{}", e));
                        }
                    }
                }

                latencies.lock().await.push(per_req_start.elapsed());
                pb.inc(1);
                tokio::time::sleep(delay).await;
            }
        }));
    }

    for outcome in join_all(tasks).await {
        outcome.map_err(|e| anyhow!("benchmark worker task failed: {e}"))?;
    }

    let elapsed = start.elapsed();
    pb.finish_with_message(format!(
        "{}Benchmark finished in {:.2}s",
        CHECKMARK,
        elapsed.as_secs_f64()
    ));

    let successes = success_counter.load(Ordering::SeqCst);
    let failures = failure_counter.load(Ordering::SeqCst);
    let mut samples = latencies.lock().await.clone();
    samples.sort();

    let avg_latency = if samples.is_empty() {
        Duration::ZERO
    } else {
        samples.iter().copied().sum::<Duration>() / (samples.len() as u32)
    };
    let p95_latency = percentile(&samples, 0.95).unwrap_or(Duration::ZERO);
    let throughput = if elapsed.as_secs_f64() > 0.0 {
        (successes + failures) as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    println!("  {}", style("─".repeat(48)).dim());
    println!(
        "  {} Successes: {}",
        style("●").green(),
        style(successes).bold()
    );
    println!(
        "  {} Failures:  {}",
        style("●").red(),
        style(failures).bold()
    );
    println!("  {}", style("─".repeat(48)).dim());
    println!("  Throughput:  {:.2} req/s", style(throughput).cyan());
    println!(
        "  Avg Latency: {:.3}s",
        style(avg_latency.as_secs_f64()).yellow()
    );
    println!(
        "  P95 Latency: {:.3}s",
        style(p95_latency.as_secs_f64()).yellow()
    );

    if failures > 0 {
        if let Some(err) = first_error.lock().await.as_ref() {
            println!("\n  {} First error: {}", CROSS, style(err).red());
        }
    }
    println!();

    Ok(())
}

// ── Emoji & Style Constants ──────────────────────────────────────────────────
static PACKAGE: Emoji<'_, '_> = Emoji("📦 ", "");
static LOOKING_GLASS: Emoji<'_, '_> = Emoji("🔍 ", "");
static DOWNLOAD: Emoji<'_, '_> = Emoji("⬇️  ", "");
static SPARKLE: Emoji<'_, '_> = Emoji("\u{2728} ", "");
static CHECKMARK: Emoji<'_, '_> = Emoji("✅ ", "[OK] ");
static CROSS: Emoji<'_, '_> = Emoji("❌ ", "[ERR] ");
static REFRESH: Emoji<'_, '_> = Emoji("🔄 ", "");
static GAUGE: Emoji<'_, '_> = Emoji("⏱️  ", "");

const REQUIRED_SERVER_ENV_KEYS: [&str; 2] = [
    "JET_RATE_LIMIT_HMAC_KEY_ID",
    "JET_RATE_LIMIT_HMAC_SECRET",
];

fn make_spinner() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn make_download_bar() -> ProgressBar {
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {msg}\n  {wide_bar:.green/white.dim} {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})"
        )
        .unwrap()
        .progress_chars("━━╺━")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✔"]),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn runtimes_command(cmd: RuntimesCommand) -> Result<()> {
    let config = JetConfig::load()?;
    let manager = PackageManager::new(
        config.runtime_install_dir.clone(),
        config.runtimes_manifest_dir.clone(),
    );

    match cmd.command {
        RuntimesSubcommands::List => runtimes_list(&manager, &config),
        RuntimesSubcommands::Install {
            language,
            version,
            arch,
        } => runtimes_install(&manager, &language, &version, &arch),
        RuntimesSubcommands::Uninstall { language, version } => {
            runtimes_uninstall(&manager, &language, &version)
        }
        RuntimesSubcommands::Update { language } => runtimes_update(&manager, language),
        RuntimesSubcommands::Clean => runtimes_clean(&manager),
    }
}

fn runtimes_list(manager: &PackageManager, config: &JetConfig) -> Result<()> {
    let header = Style::new().bold().cyan();
    let dim = Style::new().dim();
    let lang_style = Style::new().bold().green();
    let ver_style = Style::new().yellow();

    let manifests = manager.scan_manifests()?;
    if manifests.is_empty() {
        println!(
            "{}No manifests found in {}\n  Run {} to add manifests.",
            dim.apply_to("  "),
            style(config.runtimes_manifest_dir.display()).underlined(),
            style("jet-cli runtimes update --source <name>=<url>").bold()
        );
        return Ok(());
    }

    println!(
        "\n  {} {}\n",
        PACKAGE,
        header.apply_to(format!("Available Runtimes ({})", manifests.len()))
    );

    for m in &manifests {
        let archs: Vec<_> = m.runtimes.keys().cloned().collect();
        let installed_path = manager
            .runtime_dir
            .join(&m.language)
            .join(&m.version)
            .join("root");
        let is_installed = installed_path.exists();

        let dot = if is_installed {
            style("●").green()
        } else {
            style("●").red()
        };

        println!(
            "  {} {:<12} {:<14} {}",
            dot,
            lang_style.apply_to(&m.language),
            ver_style.apply_to(&m.version),
            dim.apply_to(format!("[{}]", archs.join(", ")))
        );
    }
    println!();
    Ok(())
}

fn runtimes_install(
    manager: &PackageManager,
    language: &str,
    version: &str,
    arch: &str,
) -> Result<()> {
    let start = Instant::now();

    // ── Header ─────────────────────────────────────────────────
    println!();
    println!(
        "  {} Installing {} {}",
        PACKAGE,
        style(format!("{}:{}", language, version)).bold().cyan(),
        style(format!("({})", arch)).dim()
    );
    println!("  {}", style("─".repeat(48)).dim());

    // ── Step 1: Resolve manifest ────────────────────────────────
    let spinner = make_spinner();
    spinner.set_message(format!("{}Scanning manifests…", LOOKING_GLASS));

    let resolved = manager.resolve_manifest(language, version).map_err(|e| {
        spinner.finish_with_message(format!(
            "{}Manifest not found for {}:{}",
            CROSS, language, version
        ));
        anyhow::Error::from(e)
    })?;

    spinner.finish_with_message(format!(
        "{}Resolved {} to {} {}",
        CHECKMARK,
        style(version).dim(),
        style(language).green(),
        style(&resolved.resolved_version).yellow()
    ));

    // ── Step 2: Download + install (hooks run automatically) ────
    let download_bar = make_download_bar();
    download_bar.set_message(format!(
        "{}Downloading {} {} archive…",
        DOWNLOAD,
        style(language).green(),
        style(&resolved.resolved_version).yellow()
    ));

    let extract_spinner = make_spinner();
    extract_spinner.set_draw_target(indicatif::ProgressDrawTarget::hidden());

    let lang_owned = language.to_string();
    let ver_owned = resolved.resolved_version.clone();
    let installed =
        manager.install_runtime_with_progress(&resolved.manifest, arch, &download_bar, || {
            download_bar.finish_with_message(format!("{}Download complete", CHECKMARK));
            extract_spinner.set_draw_target(indicatif::ProgressDrawTarget::stderr());
            extract_spinner.set_message(format!(
                "{}Extracting {} {}…",
                PACKAGE,
                style(&lang_owned).green(),
                style(&ver_owned).yellow()
            ));
        });

    match installed {
        Ok(path) => {
            extract_spinner.finish_with_message(format!("{}Extraction complete", CHECKMARK));

            let elapsed = start.elapsed();

            // ── Success summary ──────────────────────────────────
            println!("  {}", style("─".repeat(48)).dim());
            println!(
                "  {}Runtime installed successfully in {:.1}s",
                SPARKLE,
                elapsed.as_secs_f64()
            );
            println!(
                "  {}  {}",
                style("→").cyan(),
                style(path.display()).underlined().dim()
            );
            println!();
            Ok(())
        }
        Err(e) => {
            extract_spinner.finish_and_clear();
            download_bar.abandon_with_message(format!("{}Installation failed", CROSS));
            println!("\n  {} {}\n", CROSS, style(format!("{e}")).red());
            Err(e.into())
        }
    }
}

fn runtimes_uninstall(manager: &PackageManager, language: &str, version: &str) -> Result<()> {
    let spinner = make_spinner();
    spinner.set_message(format!(
        "{}Resolving {} {}…",
        LOOKING_GLASS,
        style(language).green(),
        style(version).yellow()
    ));

    let resolved = manager.resolve_manifest(language, version).map_err(|e| {
        spinner.finish_with_message(format!(
            "{}Could not resolve {}:{}",
            CROSS, language, version
        ));
        anyhow::Error::from(e)
    })?;

    spinner.set_message(format!(
        "{}Uninstalling {} {}…",
        LOOKING_GLASS,
        style(&resolved.canonical_language).green(),
        style(&resolved.resolved_version).yellow()
    ));

    match manager.uninstall_runtime(&resolved.canonical_language, &resolved.resolved_version) {
        Ok(true) => {
            spinner.finish_with_message(format!(
                "{}Uninstalled {} {}",
                CHECKMARK,
                style(&resolved.canonical_language).green(),
                style(&resolved.resolved_version).yellow()
            ));
            Ok(())
        }
        Ok(false) => {
            spinner.finish_with_message(format!(
                "{}Runtime {} {} is not installed",
                CROSS,
                style(&resolved.canonical_language).yellow(),
                style(&resolved.resolved_version).yellow()
            ));
            Ok(())
        }
        Err(e) => {
            spinner.finish_with_message(format!(
                "{}Failed to uninstall {} {}: {}",
                CROSS,
                style(&resolved.canonical_language).yellow(),
                style(&resolved.resolved_version).yellow(),
                style(&e).red()
            ));
            Err(e.into())
        }
    }
}

fn runtimes_clean(manager: &PackageManager) -> Result<()> {
    let spinner = make_spinner();
    spinner.set_message(format!("{}Cleaning download cache…", LOOKING_GLASS));

    match manager.clean_downloads() {
        Ok(count) => {
            spinner.finish_with_message(format!(
                "{}Removed {} cached archive(s)",
                CHECKMARK,
                style(count).bold()
            ));
            Ok(())
        }
        Err(e) => {
            spinner.finish_with_message(format!(
                "{}Failed to clean downloads: {}",
                CROSS,
                style(&e).red()
            ));
            Err(e.into())
        }
    }
}

fn runtimes_update(manager: &PackageManager, target: UpdateTarget) -> Result<()> {
    println!(
        "\n  {} {}\n",
        REFRESH,
        style("Updating manifests…").bold().cyan()
    );

    let mut total = 0usize;
    let updaters = get_updaters(target.into());

    for updater in &updaters {
        total += run_updater(manager, updater.as_ref());
    }

    println!(
        "\n  {} {}\n",
        SPARKLE,
        style(format!("Updated {total} manifest(s) total")).bold()
    );
    Ok(())
}

fn run_updater(manager: &PackageManager, updater: &dyn jet_pack::RuntimeUpdater) -> usize {
    let spinner = make_spinner();
    spinner.set_message(format!(
        "{}Fetching latest {} versions…",
        LOOKING_GLASS,
        style(updater.language()).green()
    ));

    match manager.update_manifests_with_updater(updater) {
        Ok(paths) => {
            let count = paths.len();
            spinner.finish_with_message(format!(
                "{}Updated {} {} manifest(s)",
                CHECKMARK,
                style(count).bold(),
                style(updater.language()).green()
            ));
            count
        }
        Err(e) => {
            spinner.finish_with_message(format!(
                "{}Failed to update {}: {}",
                CROSS,
                style(updater.language()).yellow(),
                style(format!("{e}")).red()
            ));
            0
        }
    }
}

fn parse_dotenv(contents: &str) -> Result<HashMap<String, String>> {
    let mut vars = HashMap::new();

    for (idx, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let (raw_key, raw_value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid .env line {}: expected KEY=VALUE", idx + 1))?;

        let key = raw_key.trim();
        if key.is_empty() {
            bail!("invalid .env line {}: empty key", idx + 1);
        }

        let mut value = raw_value.trim().to_string();
        if value.len() >= 2 {
            let first = value.as_bytes()[0] as char;
            let last = value.as_bytes()[value.len() - 1] as char;
            if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
                value = value[1..value.len() - 1].to_string();
            }
        }

        vars.insert(key.to_string(), value);
    }

    Ok(vars)
}

fn resolve_required_server_env(dotenv_path: &Path) -> Result<Vec<(String, String)>> {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();

    for key in REQUIRED_SERVER_ENV_KEYS {
        if let Ok(existing) = std::env::var(key) {
            if !existing.trim().is_empty() {
                resolved.push((key.to_string(), existing));
                continue;
            }
        }
        missing.push(key);
    }

    if missing.is_empty() {
        return Ok(resolved);
    }

    let dotenv_contents = fs::read_to_string(dotenv_path).with_context(|| {
        format!(
            "missing required server env keys ({}) and failed to read {}",
            missing.join(", "),
            dotenv_path.display()
        )
    })?;
    let parsed = parse_dotenv(&dotenv_contents)?;

    for key in missing {
        if let Some(value) = parsed.get(key) {
            if !value.trim().is_empty() {
                resolved.push((key.to_string(), value.clone()));
            }
        }
    }

    let unresolved: Vec<&str> = REQUIRED_SERVER_ENV_KEYS
        .iter()
        .copied()
        .filter(|required| !resolved.iter().any(|(k, _)| k == required))
        .collect();

    if !unresolved.is_empty() {
        bail!(
            "missing required server env keys: {} (set them in shell env or {})",
            unresolved.join(", "),
            dotenv_path.display()
        );
    }

    Ok(resolved)
}

fn server_command(cmd: ServerCommand) -> Result<()> {
    match cmd.command {
        ServerSubcommands::Run { release } => {
            let dotenv_path = PathBuf::from(".env");
            let injected = resolve_required_server_env(&dotenv_path)?;

            let mut command = Command::new("cargo");
            command.args(["run", "-p", "jet-server"]);
            for (key, value) in &injected {
                command.env(key, value);
            }
            if release {
                command.arg("--release");
            }

            let status = command.status().context("failed to launch jet-server")?;
            if !status.success() {
                bail!("jet-server exited with non-zero status");
            }
            Ok(())
        }
        ServerSubcommands::GenerateSystemd { output, user } => {
            let cwd = std::env::current_dir().context("failed to detect current directory")?;
            let service = format!(
                "[Unit]\nDescription=Jet Server\nAfter=network.target\n\n[Service]\nType=simple\nUser={user}\nWorkingDirectory={}\nExecStart=/usr/bin/env cargo run -p jet-server --release\nRestart=always\nRestartSec=3\n\n[Install]\nWantedBy=multi-user.target\n",
                cwd.display()
            );
            fs::write(&output, service)
                .with_context(|| format!("failed to write {}", output.display()))?;
            println!("systemd unit generated at {}", output.display());
            Ok(())
        }
    }
}

async fn submit_job(
    client: &reqwest::Client,
    server: &str,
    request: &JobRequest,
) -> Result<SubmitJobResponse> {
    let url = format!("{}/jobs", server.trim_end_matches('/'));
    let response = client
        .post(url)
        .json(request)
        .send()
        .await
        .context("failed to send submit request")?;

    let status = response.status();
    if status != StatusCode::ACCEPTED {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unavailable>".to_string());
        bail!("submit failed ({status}): {body}");
    }

    let parsed = response
        .json::<SubmitJobResponse>()
        .await
        .context("failed to parse submit response")?;
    Ok(parsed)
}

async fn poll_job_until_done(
    client: &reqwest::Client,
    server: &str,
    job_id: &str,
    poll_interval: Duration,
    poll_timeout: Duration,
) -> Result<JobStateRecord> {
    let url = format!("{}/jobs/{}", server.trim_end_matches('/'), job_id);
    let deadline = Instant::now() + poll_timeout;

    loop {
        if Instant::now() > deadline {
            bail!("timed out while waiting for job {}", job_id);
        }

        let response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to poll job state for {}", job_id))?;
        let status = response.status();

        if status == StatusCode::NOT_FOUND {
            tokio::time::sleep(poll_interval).await;
            continue;
        }

        // Rate-limited: back off instead of failing.
        if status == StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(poll_interval * 2).await;
            continue;
        }

        if status != StatusCode::OK {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unavailable>".to_string());
            bail!("poll failed ({status}): {body}");
        }

        let state = response
            .json::<JobStateRecord>()
            .await
            .context("failed to parse job state")?;

        match state.status.as_str() {
            "queued" | "running" => {
                tokio::time::sleep(poll_interval).await;
            }
            _ => return Ok(state),
        }
    }
}

fn print_job_state(state: &JobStateRecord) {
    println!("job_id: {}", state.job_id);
    println!("status: {}", state.status);
    println!("language: {}", state.language);
    println!("version: {}", state.version);

    if let Some(error) = &state.error {
        println!("error: {}", error);
    }

    if let Some(result) = &state.result {
        println!("\n=== Result ===");
        if let Some(compile) = &result.compile {
            println!("[compile]\n{}", compile);
        }
        if let Some(run) = &result.run {
            println!("[run]\n{}", run);
        }
        if let Some(testcases) = &result.testcases {
            println!("[testcases] total={}", testcases.len());
            for tc in testcases {
                println!("- {} passed={}", tc.id, tc.passed);
                if !tc.actual_output.trim().is_empty() {
                    println!("  output: {}", tc.actual_output.trim());
                }
                println!(
                    "  metrics: status={:?} exec={:?}ms cpu={:?}ms mem={:?}B",
                    tc.run_details.status,
                    tc.run_details.execution_time,
                    tc.run_details.cpu_time,
                    tc.run_details.memory_usage
                );
            }
        }
    }
}

#[derive(Debug, Clone)]
struct CommonLimits {
    run_timeout: Option<u64>,
    compile_timeout: Option<u64>,
    run_memory_limit: Option<u64>,
    compile_memory_limit: Option<u64>,
    run_output_limit: Option<u64>,
    compile_output_limit: Option<u64>,
}

fn build_job_request(
    language: &str,
    version: &str,
    files: &[PathBuf],
    stdin_file: Option<&Path>,
    limits: &CommonLimits,
) -> Result<JobRequest> {
    if files.is_empty() {
        bail!("at least one --file is required");
    }

    let mut job_files = Vec::with_capacity(files.len());
    for path in files {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read file {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .map(|v| v.to_string());

        job_files.push(FileRequest {
            name,
            content,
            encoding: None,
        });
    }

    let stdin = if let Some(path) = stdin_file {
        Some(
            fs::read_to_string(path)
                .with_context(|| format!("failed to read stdin file {}", path.display()))?,
        )
    } else {
        None
    };

    Ok(JobRequest {
        job_id: None,
        language: language.to_string(),
        version: Some(version.to_string()),
        files: job_files,
        testcases: None,
        args: None,
        stdin,
        run_timeout: limits.run_timeout,
        compile_timeout: limits.compile_timeout,
        run_memory_limit: limits.run_memory_limit,
        compile_memory_limit: limits.compile_memory_limit,
        run_output_limit: limits.run_output_limit,
        compile_output_limit: limits.compile_output_limit,
    })
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    humantime::parse_duration(input).map_err(|e| e.to_string())
}

fn percentile(samples: &[Duration], p: f64) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples.get(idx).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_duration_supports_fractional_seconds() {
        let d = parse_duration("0.5s").expect("duration should parse");
        assert_eq!(d, Duration::from_millis(500));
    }

    #[test]
    fn builds_job_request_from_files_and_stdin() {
        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("main.py");
        let stdin = dir.path().join("input.txt");
        fs::write(&file, "print(input())").expect("write file");
        fs::write(&stdin, "hello").expect("write stdin");

        let req = build_job_request(
            "python",
            "3.14",
            std::slice::from_ref(&file),
            Some(&stdin),
            &CommonLimits {
                run_timeout: Some(1000),
                compile_timeout: None,
                run_memory_limit: None,
                compile_memory_limit: None,
                run_output_limit: None,
                compile_output_limit: None,
            },
        )
        .expect("request should build");

        assert_eq!(req.language, "python");
        assert_eq!(req.version.as_deref(), Some("3.14"));
        assert_eq!(req.files.len(), 1);
        assert_eq!(req.files[0].name.as_deref(), Some("main.py"));
        assert_eq!(req.stdin.as_deref(), Some("hello"));
        assert_eq!(req.run_timeout, Some(1000));
    }

    #[test]
    fn percentile_returns_expected_value() {
        let samples = vec![
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
        ];
        let p95 = percentile(&samples, 0.95).expect("p95");
        assert_eq!(p95, Duration::from_millis(50));
    }

    #[test]
    fn parse_dotenv_supports_plain_and_quoted_values() {
        let parsed = parse_dotenv(
            r#"
            # comment
            JET_RATE_LIMIT_HMAC_KEY_ID=key-1
            export JET_RATE_LIMIT_HMAC_SECRET="super-secret"
            "#,
        )
        .expect("dotenv should parse");

        assert_eq!(
            parsed.get("JET_RATE_LIMIT_HMAC_KEY_ID").map(String::as_str),
            Some("key-1")
        );
        assert_eq!(
            parsed.get("JET_RATE_LIMIT_HMAC_SECRET").map(String::as_str),
            Some("super-secret")
        );
    }
}
