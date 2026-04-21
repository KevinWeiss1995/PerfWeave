//! Unix domain socket transport from the CUPTI-injected target process to
//! the agent. Length-prefixed protobuf `Batch` messages.

use std::io;
use std::os::unix::net::UnixStream;

pub struct AgentConn {
    // Written to from `ship()` once the CUPTI callback chain is wired.
    #[allow(dead_code)]
    pub stream: UnixStream,
}

pub fn connect() -> io::Result<AgentConn> {
    let path = std::env::var("PERFWEAVE_CUPTI_SOCK")
        .unwrap_or_else(|_| "/tmp/perfweave.cupti.sock".to_string());
    let stream = UnixStream::connect(path)?;
    stream.set_nonblocking(false)?;
    Ok(AgentConn { stream })
}
