use crate::config::Access;
use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;
use std::{process::Stdio, time::Duration};
use tokio::process::{Child, Command};

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;

pub struct AccessSession {
    config: Option<Access>,
    process: Option<Child>,
    group: Option<i32>,
    started: bool,
}

impl AccessSession {
    pub fn new(config: Option<Access>) -> Self {
        Self {
            config,
            process: None,
            group: None,
            started: false,
        }
    }

    pub async fn start(&mut self, backend_url: &str) -> Result<()> {
        let Some(config) = self.config.clone() else {
            return Ok(());
        };
        self.process = Some(
            shell(&config.command)
                .spawn()
                .context("starting backend access command")?,
        );
        self.group = self
            .process
            .as_ref()
            .and_then(Child::id)
            .map(|id| id as i32);
        self.started = true;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs.max(1));
        if !config.persistent {
            let status = tokio::time::timeout_at(deadline, self.process.as_mut().unwrap().wait())
                .await
                .context("backend access command timed out")??;
            // Once a preparation command is reaped its PID can be reused. Its
            // background services belong to the configured cleanup command;
            // retaining this group ID for the whole agent session is unsafe.
            self.group = None;
            if !status.success() {
                bail!("backend access command failed with {status}");
            }
        }
        let url = reqwest::Url::parse(backend_url).context("invalid backend URL")?;
        let host = url
            .host_str()
            .context("backend URL has no hostname")?
            .trim_matches(['[', ']']);
        let port = url
            .port_or_known_default()
            .context("backend URL has no port")?;
        loop {
            if config.persistent && exited_without_reaping(self.group.unwrap())? {
                bail!("persistent access command exited before backend readiness");
            }
            // TCP readiness works with authenticated APIs that reject unauthenticated HTTP probes.
            if tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::TcpStream::connect((host, port)),
            )
            .await
            .is_ok_and(|r| r.is_ok())
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "backend did not become reachable within {} seconds",
                    config.timeout_secs
                );
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    pub async fn wait_for_failure(&mut self) -> String {
        if self.config.as_ref().is_some_and(|c| c.persistent) {
            if let Some(group) = self.group {
                return match wait_without_reaping(group).await {
                    Ok(()) => "backend access process exited".into(),
                    Err(error) => format!("backend access process failed: {error}"),
                };
            }
        }
        std::future::pending().await
    }

    pub async fn cleanup(&mut self) {
        if !self.started {
            return;
        }
        self.started = false;
        if let Some(command) = self
            .config
            .as_ref()
            .and_then(|c| c.cleanup.as_deref())
            .filter(|s| !s.trim().is_empty())
        {
            match shell(command).spawn() {
                Ok(mut child) => {
                    let group = child.id().map(|id| id as i32);
                    let result = tokio::time::timeout(
                        Duration::from_secs(10),
                        wait_without_reaping(group.unwrap()),
                    )
                    .await;
                    let completed = matches!(result, Ok(Ok(())));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => eprintln!("launchcoder: cleanup command error: {error}"),
                        Err(_) => eprintln!("launchcoder: cleanup command timed out"),
                    }
                    if let Some(group) = group {
                        signal_group(group, libc::SIGTERM);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        signal_group(group, libc::SIGKILL);
                    }
                    match child.wait().await {
                        Ok(status) if completed && !status.success() => {
                            eprintln!("launchcoder: cleanup command failed: {status}")
                        }
                        Err(error) => eprintln!("launchcoder: cleanup wait failed: {error}"),
                        _ => {}
                    }
                }
                Err(error) => eprintln!("launchcoder: could not start cleanup command: {error}"),
            }
        }
        if let Some(group) = self.group.take() {
            signal_group(group, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(100)).await;
            signal_group(group, libc::SIGKILL);
        }
        if let Some(mut child) = self.process.take() {
            let _ = child.wait().await;
        }
    }
}

fn shell(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    command
}

fn signal_group(group: i32, signal: i32) {
    // Only signal process groups explicitly created by this launcher.
    unsafe {
        libc::kill(-group, signal);
    }
}

fn exited_without_reaping(pid: i32) -> std::io::Result<bool> {
    // Keep the leader as an unreaped child until its whole group is terminated.
    // This reserves its PID and prevents a later cleanup from signaling a reused
    // process-group identifier. WNOWAIT is supported by Linux and macOS.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.si_signo != 0)
}

async fn wait_without_reaping(pid: i32) -> std::io::Result<()> {
    loop {
        if exited_without_reaping(pid)? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

impl Drop for AccessSession {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            signal_group(group, libc::SIGKILL);
        }
    }
}
