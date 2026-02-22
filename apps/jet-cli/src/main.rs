use std::{
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
use futures::future::join_all;
use jet_core::{FileRequest, JetConfig, JobRequest, JobResult};
use jet_pack::{ManifestSource, PackageManager};
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

    #[arg(long = "server", default_value = "http://127.0.0.1:3000")]
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
    language: String,

    #[arg(short = 'f', long = "file", required = true)]
    files: Vec<PathBuf>,

    #[arg(short = 'v', long = "version", default_value = "latest")]
    version: String,

    #[arg(short = 'c', long = "concurrency", default_value_t = 5)]
    concurrency: usize,

    #[arg(short = 'n', long = "requests", default_value_t = 100)]
    requests: usize,

    #[arg(short = 'd', long = "delay", value_parser = parse_duration, default_value = "500ms")]
    delay: Duration,

    #[arg(long = "stdin")]
    stdin_file: Option<PathBuf>,

    #[arg(long = "server", default_value = "http://127.0.0.1:3000")]
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

#[derive(Subcommand, Debug)]
enum RuntimesSubcommands {
    List,
    Install {
        language: String,
        version: String,
        #[arg(long = "arch", default_value = "x86_64")]
        arch: String,
    },
    Update {
        #[arg(long = "source", value_parser = parse_manifest_source)]
        sources: Vec<ManifestSource>,
    },
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
    let request = build_job_request(
        &args.language,
        &args.version,
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

    let request_template = build_job_request(
        &args.language,
        &args.version,
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

    println!(
        "benchmark start: concurrency={} requests={} delay={:?}",
        args.concurrency, args.requests, args.delay
    );

    let start = Instant::now();
    let next_index = Arc::new(AtomicUsize::new(0));
    let success_counter = Arc::new(AtomicUsize::new(0));
    let failure_counter = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(Mutex::new(Vec::<Duration>::new()));

    let mut tasks = Vec::new();
    for _ in 0..args.concurrency {
        let counter = next_index.clone();
        let success = success_counter.clone();
        let failure = failure_counter.clone();
        let latencies = latencies.clone();
        let server = args.server.clone();
        let poll_interval = args.poll_interval;
        let poll_timeout = args.poll_timeout;
        let delay = args.delay;
        let template = request_template.clone();

        tasks.push(tokio::spawn(async move {
            let client = reqwest::Client::new();
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
                        }
                    }
                    Err(_) => {
                        failure.fetch_add(1, Ordering::SeqCst);
                    }
                }

                latencies.lock().await.push(per_req_start.elapsed());
                tokio::time::sleep(delay).await;
            }
        }));
    }

    for outcome in join_all(tasks).await {
        outcome.map_err(|e| anyhow!("benchmark worker task failed: {e}"))?;
    }

    let elapsed = start.elapsed();
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

    println!("benchmark finished");
    println!("  total requests: {}", successes + failures);
    println!("  successes: {}", successes);
    println!("  failures: {}", failures);
    println!("  elapsed: {:.3}s", elapsed.as_secs_f64());
    println!("  throughput: {:.2} req/s", throughput);
    println!("  avg latency: {:.3}s", avg_latency.as_secs_f64());
    println!("  p95 latency: {:.3}s", p95_latency.as_secs_f64());

    Ok(())
}

fn runtimes_command(cmd: RuntimesCommand) -> Result<()> {
    let config = JetConfig::load()?;
    let manager = PackageManager::new(
        config.runtime_install_dir.clone(),
        config.runtimes_manifest_dir.clone(),
    );

    match cmd.command {
        RuntimesSubcommands::List => {
            let manifests = manager.scan_manifests()?;
            if manifests.is_empty() {
                println!(
                    "no manifests found in {}",
                    config.runtimes_manifest_dir.display()
                );
                return Ok(());
            }
            for m in manifests {
                let archs: Vec<_> = m.runtimes.keys().cloned().collect();
                println!("{} {} [{}]", m.language, m.version, archs.join(", "));
            }
            Ok(())
        }
        RuntimesSubcommands::Install {
            language,
            version,
            arch,
        } => {
            let manifests = manager.scan_manifests()?;
            let manifest = manifests
                .into_iter()
                .find(|m| m.language == language && m.version == version)
                .ok_or_else(|| anyhow!("manifest not found for {language}:{version}"))?;

            let installed = manager.install_runtime(&manifest, &arch)?;
            println!("installed runtime at {}", installed.display());
            Ok(())
        }
        RuntimesSubcommands::Update { sources } => {
            if sources.is_empty() {
                bail!("at least one --source file_name=url is required");
            }
            let updated = manager.update_manifests(&sources)?;
            for path in updated {
                println!("updated manifest: {}", path.display());
            }
            Ok(())
        }
    }
}

fn server_command(cmd: ServerCommand) -> Result<()> {
    match cmd.command {
        ServerSubcommands::Run { release } => {
            let mut command = Command::new("cargo");
            command.args(["run", "-p", "jet-server"]);
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

fn parse_manifest_source(input: &str) -> Result<ManifestSource, String> {
    let (file_name, url) = input
        .split_once('=')
        .ok_or_else(|| "expected format: file_name=url".to_string())?;

    if file_name.trim().is_empty() || url.trim().is_empty() {
        return Err("expected format: file_name=url".to_string());
    }

    Ok(ManifestSource {
        file_name: file_name.trim().to_string(),
        url: url.trim().to_string(),
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
    fn parse_manifest_source_requires_key_value() {
        let err = parse_manifest_source("bad-format").expect_err("should fail");
        assert!(err.contains("expected format"));
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
}
