//! Unix domain socket transport from the CUPTI-injected target process to
//! the agent. Length-prefixed protobuf `Batch` messages.
//!
//! Connection policy: the injector runs inside the target CUDA process and
//! may be loaded *before* the agent has bound its socket (e.g. user runs
//! `perfweave run --` while `perfweave start` is still booting, or the
//! agent briefly restarts). We therefore retry the initial connect with
//! exponential backoff for a bounded window, then give up quietly. Every
//! retry takes the latest env value, so a late-set `PERFWEAVE_CUPTI_SOCK`
//! is honoured. After a successful connect, reconnect on write failure is
//! handled one level up (`ship()` in lib.rs).

use std::io;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub struct AgentConn {
    // Written to from `ship()` once the CUPTI callback chain is wired.
    #[allow(dead_code)]
    pub stream: UnixStream,
}

/// How long we wait for the agent to come up before giving up on init.
/// Covers "start perfweave and the CUDA program in the same script" races
/// and one agent restart cycle. 10s is generous; loads rarely cuInit this
/// quickly after a cold boot.
const INIT_BUDGET: Duration = Duration::from_secs(10);
const MIN_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_millis(500);

pub fn connect() -> io::Result<AgentConn> {
    let deadline = Instant::now() + INIT_BUDGET;
    let mut backoff = MIN_BACKOFF;
    let mut last_err: Option<io::Error> = None;
    loop {
        let path = std::env::var("PERFWEAVE_CUPTI_SOCK")
            .unwrap_or_else(|_| "/tmp/perfweave.cupti.sock".to_string());
        match UnixStream::connect(&path) {
            Ok(stream) => {
                stream.set_nonblocking(false)?;
                return Ok(AgentConn { stream });
            }
            Err(e) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(last_err.unwrap_or(e));
                }
                last_err = Some(e);
                let sleep = backoff.min(deadline.saturating_duration_since(now));
                std::thread::sleep(sleep);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Try to reconnect without blocking past `budget`. Used by `ship()` when
/// the socket has been torn down (agent restart).
pub fn reconnect(budget: Duration) -> io::Result<AgentConn> {
    let deadline = Instant::now() + budget;
    let mut backoff = MIN_BACKOFF;
    let mut last_err: Option<io::Error> = None;
    loop {
        let path = std::env::var("PERFWEAVE_CUPTI_SOCK")
            .unwrap_or_else(|_| "/tmp/perfweave.cupti.sock".to_string());
        match UnixStream::connect(&path) {
            Ok(stream) => {
                stream.set_nonblocking(false)?;
                return Ok(AgentConn { stream });
            }
            Err(e) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(last_err.unwrap_or(e));
                }
                last_err = Some(e);
                let sleep = backoff.min(deadline.saturating_duration_since(now));
                std::thread::sleep(sleep);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}
