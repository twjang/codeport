//! Exercise the actual executable with a fake agent, including signals and cleanup.
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn fixture(
    agent_script: &str,
    access_script: &str,
    persistent: bool,
) -> (tempfile::TempDir, Command, std::net::TcpListener) {
    let directory = tempfile::tempdir().unwrap();
    let agent = directory.path().join("codex");
    fs::write(&agent, format!("#!/bin/sh\n{agent_script}\n")).unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = directory.path().join("credential.json");
    fs::write(&config, serde_json::to_vec(&json!({
        "backends":{"test":{"url":format!("http://{}/v1", listener.local_addr().unwrap()),"protocol":"responses","access":{
            "command":access_script,"persistent":persistent,"timeout_secs":5,
            "cleanup":format!("printf cleaned > {}",quote(&directory.path().join("cleanup")))
        }}},"agents":{"codex":{"backend":"test"}}
    })).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_launchcoder"));
    command
        .arg("--config")
        .arg(&config)
        .arg("codex")
        .env(
            "PATH",
            format!(
                "{}:{}",
                directory.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    (directory, command, listener)
}

#[test]
fn preserves_agent_status_and_cleans_access() {
    let (directory, mut command, _listener) = fixture("exit 23", "sleep 60", true);
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("cleanup")).unwrap(),
        "cleaned"
    );
    assert_eq!(
        fs::metadata(directory.path().join("credential.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn sigterm_during_preparation_runs_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("started");
    let script = format!("printf started > {}; sleep 60", quote(&marker));
    let (directory, mut command, _listener) = fixture("exit 0", &script, false);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "access command never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(143));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("launcher did not stop after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("cleanup")).unwrap(),
        "cleaned"
    );
}

#[test]
fn cfg_alias_and_forwarding_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_launchcoder"))
        .args(["-cfg", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--backend"));
}

#[test]
fn sigterm_stops_agent_tool_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let heartbeat = temp.path().join("heartbeat");
    let script = format!(
        "(trap '' TERM; while :; do printf tick >> {}; sleep 0.03; done) &\nwait",
        quote(&heartbeat)
    );
    let (directory, mut command, _listener) = fixture(&script, "true", false);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !heartbeat.exists() {
        assert!(Instant::now() < deadline, "agent tool never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(143));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("launcher did not stop after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let stopped = fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        fs::metadata(&heartbeat).unwrap().len(),
        stopped,
        "TERM-ignoring tool descendant survived launcher exit"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("cleanup")).unwrap(),
        "cleaned"
    );
}

#[test]
fn interactive_agent_can_read_foreground_terminal() {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    let (_directory, mut command, _listener) = fixture(
        "printf 'agent-ready\\n'; read answer; printf 'agent-read:%s\\n' \"$answer\"",
        "true",
        false,
    );
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let mut master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(slave);
    use std::os::fd::AsRawFd;
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    let mut output = Vec::new();
    let mut sent = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut buffer = [0_u8; 1024];
        while let Ok(count) = master.read(&mut buffer) {
            if count == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..count]);
        }
        if !sent && String::from_utf8_lossy(&output).contains("agent-ready") {
            master.write_all(b"hello-terminal\n").unwrap();
            sent = true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{status}: {}",
                String::from_utf8_lossy(&output)
            );
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            drop(master);
            let _ = child.kill();
            panic!(
                "interactive agent terminal timeout: {}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        String::from_utf8_lossy(&output).contains("agent-read:hello-terminal"),
        "{}",
        String::from_utf8_lossy(&output)
    );
}
