mod access;
mod agent_process;
mod agents;
mod bridge;
mod config;
mod protocol;
mod tui;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "Launch coding agents with your own LLM backend")]
struct Cli {
    /// Open interactive configuration (also accepts -cfg)
    #[arg(long)]
    cfg: bool,
    /// Use an alternate credential file
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Override this agent's default backend
    #[arg(long)]
    backend: Option<String>,
    /// Override the configured model
    #[arg(long)]
    model: Option<String>,
    /// Installed coding agent: pi, opencode, codex, or claude
    agent: Option<String>,
    /// Arguments forwarded to the coding agent after --
    #[arg(last = true)]
    args: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    // clap supports conventional long options; keep the explicitly promised -cfg alias.
    let mut argv: Vec<_> = std::env::args_os().collect();
    for arg in argv.iter_mut().skip(1) {
        if arg == "--" {
            break;
        }
        if arg == "-cfg" {
            *arg = "--cfg".into();
        }
    }
    let cli = Cli::parse_from(argv);
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("launchcoder: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<u8> {
    if cli.cfg {
        tui::run(cli.config.as_deref())?;
        return Ok(0);
    }
    let agent = cli
        .agent
        .context("specify an agent (pi, opencode, codex, claude), or run launchcoder -cfg")?;
    let agent = if agent == "claude-code" {
        "claude".to_owned()
    } else {
        agent
    };
    agents::protocol(&agent)?;
    let config = match cli.config {
        Some(path) => config::Config::load_from(&path)?,
        None => config::Config::load()?,
    };
    let binding = config.agents.get(&agent);
    let backend_name = cli
        .backend
        .as_deref()
        .or_else(|| binding.map(|b| b.backend.as_str()))
        .context("no backend selected; run launchcoder -cfg or supply --backend NAME")?;
    let backend = config
        .backends
        .get(backend_name)
        .with_context(|| format!("backend {backend_name:?} does not exist; run launchcoder -cfg"))?
        .clone();
    let model = cli
        .model
        .as_deref()
        .or_else(|| {
            binding
                .filter(|b| b.backend == backend_name)
                .and_then(|b| b.model.as_deref())
        })
        .or(backend.model.as_deref())
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let mut access = access::AccessSession::new(backend.access.clone());
    // Install signal handlers before any access command is started.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let result = launch(
        &agent,
        &cli.args,
        backend,
        model,
        &mut access,
        &mut terminate,
        &mut interrupt,
    )
    .await;
    access.cleanup().await;
    result
}

async fn launch(
    agent: &str,
    args: &[String],
    backend: config::Backend,
    model: Option<String>,
    access: &mut access::AccessSession,
    terminate: &mut tokio::signal::unix::Signal,
    interrupt: &mut tokio::signal::unix::Signal,
) -> Result<u8> {
    tokio::select! {
        result = access.start(&backend.url) => result?,
        _ = terminate.recv() => return Ok(143),
        _ = interrupt.recv() => return Ok(130),
    }
    let upstream_protocol = backend.protocol;
    let bridge = bridge::Bridge::start(backend, model.clone()).await?;
    let args = if agent == "opencode" {
        agents::opencode_args(args).await?
    } else {
        args.to_vec()
    };
    let mut launch = agents::prepare(
        agent,
        &bridge.base_url,
        &bridge.token,
        model.as_deref(),
        &args,
        upstream_protocol,
    )?;
    let mut child =
        agent_process::AgentProcess::spawn(launch.take_command()).with_context(|| {
            format!("could not launch {agent}; install the agent and ensure it is on PATH")
        })?;
    let status = tokio::select! {
        status = child.wait() => status?,
        error = access.wait_for_failure() => {
            child.shutdown(libc::SIGTERM).await;
            bail!("{error}");
        }
        _ = terminate.recv() => {
            child.shutdown(libc::SIGTERM).await;
            return Ok(143);
        }
        _ = interrupt.recv() => {
            child.shutdown(libc::SIGINT).await;
            return Ok(130);
        }
    };
    drop(child);
    // Keep temporary agent configuration and the proxy alive until the child exits.
    drop(launch);
    drop(bridge);
    use std::os::unix::process::ExitStatusExt;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
        .clamp(0, 255) as u8)
}
