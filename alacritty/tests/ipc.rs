#![cfg(unix)]

use std::fs;
use std::io::{Error, ErrorKind, Read, Result as IoResult, Seek, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Allow the client's ten-second reply deadline plus process startup time.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

fn client(socket: &Path, args: &[&str]) -> Output {
    client_with_binary(Path::new(env!("CARGO_BIN_EXE_alacritty")), socket, args, TEST_TIMEOUT)
        .unwrap()
}

fn client_with_binary(
    binary: &Path,
    socket: &Path,
    args: &[&str],
    timeout: Duration,
) -> IoResult<Output> {
    let mut command = Command::new(binary);
    command.args(["msg", "--socket"]).arg(socket).arg("create-window").args(args);
    command_output(&mut command, timeout)
}

fn command_output(command: &mut Command, timeout: Duration) -> IoResult<Output> {
    // Files avoid filling a capture pipe while polling for the child's exit.
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = Process(
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?)
            .spawn()?,
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(Error::new(ErrorKind::TimedOut, "IPC test client did not exit"));
        }
        thread::sleep(Duration::from_millis(10));
    };

    stdout.rewind()?;
    stderr.rewind()?;
    let mut output = Output { status, stdout: Vec::new(), stderr: Vec::new() };
    stdout.read_to_end(&mut output.stdout)?;
    stderr.read_to_end(&mut output.stderr)?;
    Ok(output)
}

fn accept(listener: &UnixListener, timeout: Duration) -> IoResult<UnixStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Accepted sockets inherit nonblocking mode on some platforms.
                stream.set_nonblocking(false)?;
                return Ok(stream);
            },
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => (),
            Err(err) => return Err(err),
        }
        if Instant::now() >= deadline {
            return Err(Error::new(ErrorKind::TimedOut, "Mock daemon did not accept a connection"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn mock_daemon(reply: &[u8], args: &[&str]) -> (Output, Value) {
    try_mock_daemon(reply, args, TEST_TIMEOUT).unwrap()
}

fn try_mock_daemon(reply: &[u8], args: &[&str], timeout: Duration) -> IoResult<(Output, Value)> {
    let directory = TempDir::new()?;
    let socket = directory.path().join("ipc.sock");
    let listener = UnixListener::bind(&socket)?;
    let reply = reply.to_vec();
    let daemon = thread::spawn(move || {
        let mut stream = accept(&listener, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut request = String::new();
        stream.read_to_string(&mut request)?;
        stream.write_all(&reply)?;
        serde_json::from_str(&request).map_err(Error::other)
    });
    let output =
        client_with_binary(Path::new(env!("CARGO_BIN_EXE_alacritty")), &socket, args, timeout);
    let request = daemon.join().unwrap();
    Ok((output?, request?))
}

#[test]
fn mock_daemon_client_rejection_is_bounded() {
    let err =
        try_mock_daemon(b"", &["--unknown-test-option"], Duration::from_millis(200)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TimedOut);
}

#[test]
fn client_wait_is_bounded() {
    let err =
        command_output(Command::new("sleep").arg("1"), Duration::from_millis(20)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TimedOut);
}

#[test]
fn print_pid_stdout() {
    let (output, request) = mock_daemon(br#"{"CreateWindow":{"Ok":1234}}"#, &["--print-pid"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"1234\n");
    assert!(output.stderr.is_empty());
    assert_eq!(request["CreateWindow"]["request_pid"], true);
}

#[test]
fn creation_error_is_stderr_and_failure() {
    let (output, _) =
        mock_daemon(br#"{"CreateWindow":{"Err":"PTY spawn failed"}}"#, &["--print-pid"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PTY spawn failed"));
}

#[test]
fn old_daemon_without_reply() {
    let (output, _) = mock_daemon(b"", &["--print-pid"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("may not support --print-pid"));
    assert!(stderr.contains("may already have been created"));
}

#[test]
fn unflagged_client_remains_fire_and_forget() {
    let (output, request) = mock_daemon(b"", &[]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(request["CreateWindow"].get("request_pid").is_none());
}

/// Terminate and reap only the process owned by this test, including on failure.
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(directory: &Path) -> (Process, std::path::PathBuf) {
    start_daemon_with_binary(directory, Path::new(env!("CARGO_BIN_EXE_alacritty")))
}

fn start_daemon_with_binary(directory: &Path, binary: &Path) -> (Process, std::path::PathBuf) {
    let socket = directory.join("ipc.sock");
    let config = directory.join("alacritty.toml");
    fs::write(&config, "[general]\nlive_config_reload = false\n").unwrap();
    let mut daemon = Process(
        Command::new(binary)
            .args(["--daemon", "--socket"])
            .arg(&socket)
            .arg("--config-file")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(daemon.0.try_wait().unwrap().is_none(), "daemon exited before creating socket");
        assert!(Instant::now() < deadline, "daemon did not create socket");
        thread::sleep(Duration::from_millis(10));
    }
    (daemon, socket)
}

#[test]
#[ignore = "requires an isolated graphical display"]
fn daemon_pid_replies() {
    let directory = TempDir::new().unwrap();
    let (_daemon, socket) = start_daemon(directory.path());

    // Exercise initial and subsequent windows with a child that exits immediately, without hold.
    for index in 0..2 {
        let pid_file = directory.path().join(format!("child-{index}.pid"));
        let output = client(
            &socket,
            &[
                "--print-pid",
                "-e",
                "sh",
                "-c",
                "echo $$ > \"$1\"",
                "sh",
                pid_file.to_str().unwrap(),
            ],
        );
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stderr.is_empty());
        let pid = String::from_utf8(output.stdout).unwrap();
        assert!(pid.trim().parse::<u32>().unwrap() > 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fs::read_to_string(&pid_file).ok().as_deref() == Some(&pid) {
                break;
            }
            assert!(Instant::now() < deadline, "reply PID did not match the spawned child");
            thread::sleep(Duration::from_millis(10));
        }
    }

    let failure = client(&socket, &["--print-pid", "-e", "/nonexistent/alacritty-test-command"]);
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("Failed to spawn command"));

    // A failed subsequent window must leave the daemon usable.
    let output = client(&socket, &["--print-pid", "--hold", "-e", "true"]);
    assert!(output.status.success());

    let output = client(&socket, &["--hold", "-e", "true"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
#[ignore = "requires an isolated graphical display"]
fn daemon_initial_spawn_failure_replies_before_exit() {
    let directory = TempDir::new().unwrap();
    let (mut daemon, socket) = start_daemon(directory.path());
    let output = client(&socket, &["--print-pid", "-e", "/nonexistent/alacritty-test-command"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Failed to spawn command"));

    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "initial creation failure did not exit the daemon");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires an isolated graphical display"]
fn disconnected_client_does_not_prevent_creation() {
    let directory = TempDir::new().unwrap();
    let (_daemon, socket) = start_daemon(directory.path());
    let marker = directory.path().join("created");
    let request = serde_json::json!({
        "CreateWindow": {
            "terminal_options": {
                "hold": false,
                "command": ["sh", "-c", "echo created > \"$1\"", "sh", marker],
            },
            "window_identity": {},
            "option": [],
            "request_pid": true,
        },
    });
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
    // Disable replies before sending so disconnection cannot race with a successful reply.
    stream.shutdown(Shutdown::Read).unwrap();
    stream.write_all(serde_json::to_string(&request).unwrap().as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    drop(stream);

    let deadline = Instant::now() + TEST_TIMEOUT;
    while fs::read_to_string(&marker).ok().as_deref() != Some("created\n") {
        assert!(Instant::now() < deadline, "disconnected request did not create its child");
        thread::sleep(Duration::from_millis(10));
    }
    let output = client(&socket, &["--print-pid", "-e", "true"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
#[ignore = "requires an isolated graphical display"]
fn oversized_spawn_error_returns_valid_reply() {
    let directory = TempDir::new().unwrap();
    let (_daemon, socket) = start_daemon(directory.path());
    let command = "x".repeat(4096);
    let output = client(&socket, &["--print-pid", "-e", &command]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("error details exceed the IPC reply limit")
    );
}

#[test]
#[ignore = "requires an isolated graphical display and ALACRITTY_TEST_OLD_BINARY"]
fn old_binary_compatibility() {
    let old_binary = std::env::var_os("ALACRITTY_TEST_OLD_BINARY").unwrap();
    let directory = TempDir::new().unwrap();
    let (_daemon, socket) = start_daemon_with_binary(directory.path(), Path::new(&old_binary));
    let marker = directory.path().join("created");
    let output = client(
        &socket,
        &[
            "--print-pid",
            "--hold",
            "-e",
            "sh",
            "-c",
            "echo created >> \"$1\"",
            "sh",
            marker.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("may not support --print-pid"));

    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(&marker).ok().as_deref() != Some("created\n") {
        assert!(Instant::now() < deadline, "old daemon did not create the requested child");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), "created\n");

    let output = client(&socket, &["--hold", "-e", "true"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());

    let new_directory = TempDir::new().unwrap();
    let (_new_daemon, new_socket) = start_daemon(new_directory.path());
    let output = client_with_binary(
        Path::new(&old_binary),
        &new_socket,
        &["--hold", "-e", "true"],
        TEST_TIMEOUT,
    )
    .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    // The queued old-client request must leave the new daemon able to serve PID replies.
    let output = client(&new_socket, &["--print-pid", "--hold", "-e", "true"]);
    assert!(output.status.success());
}
