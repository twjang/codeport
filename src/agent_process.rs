//! Own an agent process group without breaking interactive terminal input.
use std::{io, os::unix::process::CommandExt, process::ExitStatus, time::Duration};
use tokio::process::Child;

pub struct AgentProcess {
    child: Child,
    group: libc::pid_t,
    _terminal: Option<ForegroundTerminal>,
}

struct ForegroundTerminal {
    fd: libc::c_int,
    previous: libc::pid_t,
}

impl ForegroundTerminal {
    fn current() -> Option<Self> {
        // Only hand off a terminal that this launcher actually owns. Background
        // launches and redirected stdin must retain their existing job control.
        let previous = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        (previous > 0 && previous == unsafe { libc::getpgrp() }).then_some(Self {
            fd: libc::STDIN_FILENO,
            previous,
        })
    }
}

/// tcsetpgrp from the now-background launcher must not stop it with SIGTTOU.
/// Blocking only this thread avoids changing signal dispositions process-wide.
fn foreground(fd: libc::c_int, group: libc::pid_t) -> io::Result<()> {
    unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let code = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        let result = libc::tcsetpgrp(fd, group);
        let error = io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if result == -1 {
            Err(error)
        } else {
            Ok(())
        }
    }
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        let _ = foreground(self.fd, self.previous);
    }
}

impl AgentProcess {
    pub fn spawn(mut command: std::process::Command) -> io::Result<Self> {
        let terminal = ForegroundTerminal::current();
        let terminal_fd = terminal.as_ref().map(|terminal| terminal.fd);
        // pre_exec executes only async-signal-safe libc operations. Giving the
        // child the terminal here, before exec, prevents its first read racing
        // the parent's foreground handoff.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(fd) = terminal_fd {
                    foreground(fd, libc::getpid())?;
                }
                Ok(())
            });
        }
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let child = command.spawn()?;
        let group = child.id().expect("newly spawned child has PID") as libc::pid_t;
        Ok(Self {
            child,
            group,
            _terminal: terminal,
        })
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if self.group > 0 {
            loop {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.group as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result == -1 {
                    return Err(io::Error::last_os_error());
                }
                if info.si_signo != 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Reserve the leader PID until after terminating its group, so a
            // reused process-group ID can never target an unrelated process.
            self.signal_group(libc::SIGKILL);
            self.group = 0;
        }
        self.child.wait().await
    }

    fn signal_group(&self, signal: libc::c_int) {
        // group is exactly the positive PID returned by our own successful
        // spawn. Never signal the launcher's group, zero, or a guessed PID.
        if self.group > 0 {
            unsafe {
                libc::kill(-self.group, signal);
            }
        }
    }

    pub async fn shutdown(&mut self, signal: libc::c_int) {
        self.signal_group(signal);
        let _ = tokio::time::timeout(Duration::from_millis(300), self.wait()).await;
        // The group can outlive its leader; wait() kills it before reaping.
        // If the grace period expired, the leader still reserves the group ID.
        self.signal_group(libc::SIGKILL);
        self.group = 0;
        let _ = self.child.wait().await;
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        // Covers cancelled futures, startup errors and normal leader exit with
        // leftover tool children. Tokio's Child drop handles leader reaping.
        self.signal_group(libc::SIGKILL);
    }
}
