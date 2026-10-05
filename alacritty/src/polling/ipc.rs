//! Alacritty socket IPC.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Read, Result as IoResult, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs};

use log::{error, warn};
use std::result::Result;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::cli::{IpcCreateWindow, Options, SocketMessage};
use crate::event::{Event, EventType};

/// Environment variable name for the IPC socket path.
const ALACRITTY_SOCKET_ENV: &str = "ALACRITTY_SOCKET";

/// Maximum time to wait for a requested child PID.
///
/// Allow time for initial graphics setup while bounding waits on an unresponsive daemon. Expiring
/// this deadline does not cancel window creation.
const PID_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum size of a create-window reply, including an error message.
///
/// A PID needs only a few bytes. Keep diagnostics small for best-effort nonblocking delivery on the
/// window event loop; unusually long errors are logged in full and replaced with a short reply.
const MAX_PID_REPLY_SIZE: usize = 1024;

/// IPC socket listener.
pub struct IpcListener {
    pub socket: UnixListener,

    event_proxy: EventLoopProxy<Event>,
    data: String,
}

impl IpcListener {
    pub fn new(
        options: &Options,
        event_proxy: EventLoopProxy<Event>,
        path: &Path,
    ) -> Result<Self, IoError> {
        // Create unix socket in nonblocking mode.
        let socket = UnixListener::bind(path)?;
        socket.set_nonblocking(true)?;

        // Register socket path as environment variable for `alacritty msg`.
        unsafe { env::set_var(ALACRITTY_SOCKET_ENV, path.as_os_str()) };
        if options.daemon {
            println!("ALACRITTY_SOCKET={}; export ALACRITTY_SOCKET", path.display());
        }

        Ok(Self { event_proxy, socket, data: Default::default() })
    }

    /// Process the next IPC message.
    pub fn process_message(&mut self) -> Result<(), IoError> {
        let (stream, _) = self.socket.accept()?;

        self.data.clear();
        let mut reader = BufReader::new(&stream);

        match reader.read_line(&mut self.data) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => (),
        };

        let message: SocketMessage = match serde_json::from_str(&self.data) {
            Ok(message) => message,
            Err(err) => {
                warn!("Failed to parse IPC message: {err}");
                return Ok(());
            },
        };

        // Handle IPC events.
        match message {
            SocketMessage::CreateWindow(options) => {
                let event = Event::new(create_window_event(options, stream)?, None);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::Config(ipc_config) => {
                let window_id =
                    ipc_config.window_id.and_then(|id| u64::try_from(id).ok()).map(WindowId::from);
                let event = Event::new(EventType::IpcConfig(ipc_config), window_id);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::GetConfig(config) => {
                let window_id =
                    config.window_id.and_then(|id| u64::try_from(id).ok()).map(WindowId::from);
                let event = Event::new(EventType::IpcGetConfig(Arc::new(stream)), window_id);
                let _ = self.event_proxy.send_event(event);
            },
        }

        Ok(())
    }
}

/// Retain the reply socket only when explicitly requested.
fn create_window_event(options: IpcCreateWindow, stream: UnixStream) -> IoResult<EventType> {
    if options.request_pid {
        // A client which stops reading must not block the window event loop.
        // Failed writes are logged and the socket is dropped without retrying creation.
        stream.set_nonblocking(true)?;
        Ok(EventType::IpcCreateWindow(options.window_options, Arc::new(stream)))
    } else {
        Ok(EventType::CreateWindow(options.window_options))
    }
}

/// Send a message to the active Alacritty socket.
pub fn send_message(socket: Option<PathBuf>, message: SocketMessage) -> IoResult<()> {
    let mut socket = find_socket(socket)?;

    if matches!(&message, SocketMessage::CreateWindow(options) if options.request_pid) {
        socket.set_write_timeout(Some(PID_REPLY_TIMEOUT))?;
    }

    // Write message to socket.
    let message_json = serde_json::to_string(&message)?;
    socket.write_all(message_json.as_bytes())?;
    let _ = socket.flush();

    // Shutdown write end, to allow reading.
    socket.shutdown(Shutdown::Write)?;

    // Get matching IPC reply.
    handle_reply(&socket, &message)?;

    Ok(())
}

/// Process IPC responses.
fn handle_reply(stream: &UnixStream, message: &SocketMessage) -> IoResult<()> {
    if matches!(message, SocketMessage::CreateWindow(options) if options.request_pid) {
        let pid = read_pid_reply(stream, PID_REPLY_TIMEOUT)?;
        println!("{pid}");
        return Ok(());
    }

    // Read reply, returning early if there is none.
    let mut buffer = String::new();
    let mut reader = BufReader::new(stream);
    if let Ok(0) | Err(_) = reader.read_line(&mut buffer) {
        return Ok(());
    }

    // Parse IPC reply.
    let reply: SocketReply = serde_json::from_str(&buffer)
        .map_err(|err| IoError::other(format!("Invalid IPC format: {err}")))?;

    // Ensure reply matches request.
    match (message, &reply) {
        // Write requested config to STDOUT.
        (SocketMessage::GetConfig(..), SocketReply::GetConfig(config)) => {
            println!("{config}");
            Ok(())
        },
        // Ignore requests without reply.
        _ => Ok(()),
    }
}

/// Read a requested PID with a deadline, even if the peer sends a partial reply.
fn read_pid_reply(mut stream: &UnixStream, timeout: Duration) -> IoResult<u32> {
    let deadline = Instant::now() + timeout;
    let mut buffer = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(pid_reply_timeout());
        }
        stream.set_read_timeout(Some(remaining))?;

        let mut chunk = [0; 1024];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(len) => {
                buffer.extend_from_slice(&chunk[..len]);
                if buffer.len() > MAX_PID_REPLY_SIZE {
                    return Err(IoError::new(
                        ErrorKind::InvalidData,
                        "Create-window reply too large",
                    ));
                }
                if buffer.contains(&b'\n') {
                    break;
                }
            },
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(pid_reply_timeout());
            },
            Err(err) => return Err(err),
        }
    }

    if buffer.is_empty() {
        return Err(IoError::new(
            ErrorKind::UnexpectedEof,
            "No create-window PID reply; the running Alacritty may not support --print-pid. \
             The window may already have been created; do not retry automatically",
        ));
    }

    let reply = serde_json::from_slice(&buffer).map_err(|err| {
        IoError::new(ErrorKind::InvalidData, format!("Invalid IPC format: {err}"))
    })?;
    match reply {
        SocketReply::CreateWindow(Ok(pid)) if pid != 0 => Ok(pid),
        SocketReply::CreateWindow(Err(err)) => Err(IoError::other(err)),
        _ => Err(IoError::new(ErrorKind::InvalidData, "Invalid create-window PID reply")),
    }
}

fn pid_reply_timeout() -> IoError {
    IoError::new(
        ErrorKind::TimedOut,
        "Timed out waiting for create-window PID reply. \
         The window may already have been created; do not retry automatically",
    )
}

/// Send IPC message reply.
pub fn send_reply(stream: &UnixStream, message: SocketReply) {
    if let Err(err) = send_reply_fallible(stream, message) {
        error!("Failed to send IPC reply: {err}");
    }
}

/// Send IPC message reply, returning possible errors.
fn send_reply_fallible(mut stream: &UnixStream, message: SocketReply) -> IoResult<()> {
    let mut json = serde_json::to_string(&message).map_err(IoError::other)?;
    if let SocketReply::CreateWindow(Err(err)) = &message {
        if json.len() > MAX_PID_REPLY_SIZE {
            error!("Window creation failed: {err}");
            let reply = SocketReply::CreateWindow(Err(String::from(
                "Window creation failed; error details exceed the IPC reply limit. \
                 See Alacritty's log",
            )));
            json = serde_json::to_string(&reply).map_err(IoError::other)?;
        }
    }
    stream.write_all(json.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Directory for the IPC socket file.
#[cfg(not(target_os = "macos"))]
pub fn socket_dir() -> PathBuf {
    xdg::BaseDirectories::with_prefix("alacritty")
        .get_runtime_directory()
        .map(ToOwned::to_owned)
        .ok()
        .and_then(|path| fs::create_dir_all(&path).map(|_| path).ok())
        .unwrap_or_else(env::temp_dir)
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
pub fn socket_dir() -> PathBuf {
    env::temp_dir()
}

/// Find the IPC socket path.
fn find_socket(socket_path: Option<PathBuf>) -> IoResult<UnixStream> {
    // Handle --socket CLI override.
    if let Some(socket_path) = socket_path {
        // Ensure we inform the user about an invalid path.
        return UnixStream::connect(&socket_path).map_err(|err| {
            let message = format!("invalid socket path {socket_path:?}");
            IoError::new(err.kind(), message)
        });
    }

    // Handle environment variable.
    if let Ok(path) = env::var(ALACRITTY_SOCKET_ENV) {
        let socket_path = PathBuf::from(path);
        if let Ok(socket) = UnixStream::connect(socket_path) {
            return Ok(socket);
        }
    }

    // Search for sockets files.
    for entry in fs::read_dir(socket_dir())?.filter_map(|entry| entry.ok()) {
        let path = entry.path();

        // Skip files that aren't Alacritty sockets.
        let socket_prefix = socket_prefix();
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|file| file.starts_with(&socket_prefix) && file.ends_with(".sock"))
            .is_none()
        {
            continue;
        }

        // Attempt to connect to the socket.
        match UnixStream::connect(&path) {
            Ok(socket) => return Ok(socket),
            // Delete orphan sockets.
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs::remove_file(&path);
            },
            // Ignore other errors like permission issues.
            Err(_) => (),
        }
    }

    Err(IoError::new(ErrorKind::NotFound, "no socket found"))
}

/// File prefix matching all available sockets.
///
/// This prefix will include display server information to allow for environments with multiple
/// display servers running for the same user.
#[cfg(not(target_os = "macos"))]
pub fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY").or_else(|_| env::var("DISPLAY")).unwrap_or_default();
    format!("Alacritty-{}", display.replace('/', "-"))
}

/// File prefix matching all available sockets.
#[cfg(target_os = "macos")]
pub fn socket_prefix() -> String {
    String::from("Alacritty")
}

/// IPC socket replies.
#[derive(Serialize, Deserialize, Debug)]
pub enum SocketReply {
    GetConfig(String),
    CreateWindow(Result<u32, String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_reply_bytes(bytes: &[u8]) -> IoResult<u32> {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || server.write_all(bytes).unwrap());
            read_pid_reply(&client, Duration::from_secs(1))
        })
    }

    #[test]
    fn pid_reply_success_and_creation_failure() {
        // Literal wire fixtures protect the externally tagged reply format.
        assert_eq!(read_reply_bytes(br#"{"CreateWindow":{"Ok":1234}}"#).unwrap(), 1234);
        let err = read_reply_bytes(br#"{"CreateWindow":{"Err":"PTY spawn failed"}}"#).unwrap_err();
        assert_eq!(err.to_string(), "PTY spawn failed");

        let (client, server) = UnixStream::pair().unwrap();
        send_reply_fallible(&server, SocketReply::CreateWindow(Ok(1234))).unwrap();
        drop(server);
        assert_eq!(read_pid_reply(&client, Duration::from_secs(1)).unwrap(), 1234);
    }

    #[test]
    fn old_daemon_eof_is_not_success() {
        let err = read_reply_bytes(b"").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("may not support --print-pid"));
        assert!(err.to_string().contains("may already have been created"));
    }

    #[test]
    fn invalid_pid_replies() {
        for reply in [
            &b"not JSON"[..],
            br#"{"CreateWindow":{"Ok":0}}"#,
            br#"{"CreateWindow":{"Ok":-1}}"#,
            br#"{"CreateWindow":{"Ok":4294967296}}"#,
            br#"{"GetConfig":"{}"}"#,
            br#"{"CreateWindow":{"Ok":1234}}{"CreateWindow":{"Ok":5678}}"#,
        ] {
            assert_eq!(read_reply_bytes(reply).unwrap_err().kind(), ErrorKind::InvalidData);
        }
    }

    #[test]
    fn reply_wait_is_bounded() {
        for partial in [&b""[..], &b"{\"CreateWindow\":"[..]] {
            let (client, mut server) = UnixStream::pair().unwrap();
            server.write_all(partial).unwrap();
            let err = read_pid_reply(&client, Duration::from_millis(20)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::TimedOut);
            assert!(err.to_string().contains("do not retry automatically"));
        }
    }

    #[test]
    fn unflagged_request_drops_socket_without_reply() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let event = create_window_event(IpcCreateWindow::default(), server).unwrap();
        assert!(matches!(event, EventType::CreateWindow(_)));
        // Holding the queued event must not hold the unflagged client's socket.
        client.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn flagged_request_retains_socket_until_reply() {
        let (client, server) = UnixStream::pair().unwrap();
        let options = IpcCreateWindow { request_pid: true, ..Default::default() };
        let event = create_window_event(options, server).unwrap();
        let EventType::IpcCreateWindow(_, stream) = event else { panic!() };
        send_reply_fallible(&stream, SocketReply::CreateWindow(Ok(1234))).unwrap();
        drop(stream);
        assert_eq!(read_pid_reply(&client, Duration::from_secs(1)).unwrap(), 1234);
    }

    #[test]
    fn disconnected_client_still_queues_creation() {
        let (client, server) = UnixStream::pair().unwrap();
        drop(client);
        let options = IpcCreateWindow { request_pid: true, ..Default::default() };
        let event = create_window_event(options, server).unwrap();
        let EventType::IpcCreateWindow(_, stream) = event else { panic!() };
        assert!(send_reply_fallible(&stream, SocketReply::CreateWindow(Ok(1234))).is_err());
    }

    #[test]
    fn reply_size_is_bounded() {
        let reply = vec![b' '; MAX_PID_REPLY_SIZE + 1];
        assert_eq!(read_reply_bytes(&reply).unwrap_err().kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn nonreading_client_cannot_block_reply() {
        let (_client, server) = UnixStream::pair().unwrap();
        let options = IpcCreateWindow { request_pid: true, ..Default::default() };
        let event = create_window_event(options, server).unwrap();
        let EventType::IpcCreateWindow(_, stream) = event else { panic!() };
        // Fill the socket before sending a bounded reply.
        let mut writer = &*stream;
        loop {
            match writer.write(&[0; 4096]) {
                Ok(_) => (),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    assert_eq!(err.kind(), ErrorKind::WouldBlock);
                    break;
                },
            }
        }
        let reply = SocketReply::CreateWindow(Ok(1234));
        assert_eq!(send_reply_fallible(&stream, reply).unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn creation_error_at_reply_limit_is_preserved() {
        let error = "x".repeat(MAX_PID_REPLY_SIZE - br#"{"CreateWindow":{"Err":""}}"#.len());
        let (client, server) = UnixStream::pair().unwrap();
        send_reply_fallible(&server, SocketReply::CreateWindow(Err(error.clone()))).unwrap();
        drop(server);
        assert_eq!(read_pid_reply(&client, Duration::from_secs(1)).unwrap_err().to_string(), error);
    }

    #[test]
    fn oversized_creation_errors_return_valid_replies() {
        // Measure the encoded reply, accounting for both UTF-8 and JSON escaping.
        for error in ["x".repeat(MAX_PID_REPLY_SIZE), "é\"".repeat(MAX_PID_REPLY_SIZE / 4)] {
            let (client, server) = UnixStream::pair().unwrap();
            send_reply_fallible(&server, SocketReply::CreateWindow(Err(error))).unwrap();
            drop(server);
            let err = read_pid_reply(&client, Duration::from_secs(1)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Other);
            assert!(err.to_string().contains("error details exceed the IPC reply limit"));
        }
    }
}
