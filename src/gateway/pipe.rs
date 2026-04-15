use std::io;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

const PIPE_NAME: &str = r"\\.\pipe\discord-ipc-0";

/// Create a Named Pipe server for slot 0.
/// Uses `first_pipe_instance(true)` to ensure exclusive ownership.
pub fn create() -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(true)
        .create(PIPE_NAME)
}
