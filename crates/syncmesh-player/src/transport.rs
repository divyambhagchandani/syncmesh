//! Cross-platform transport for the mpv IPC endpoint.
//!
//! mpv listens on a Unix socket on Unix and on a Windows named pipe on
//! Windows. Both expose `AsyncRead + AsyncWrite` in tokio, so we expose a
//! single trait object that the [`IpcClient`](crate::ipc::IpcClient) reads
//! and writes against. Tests use [`tokio::io::duplex`] to obtain the same
//! trait object shape without touching the filesystem.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::sleep;

use crate::error::MpvError;

/// Any `AsyncRead + AsyncWrite` pair can act as the IPC transport. `Send +
/// Unpin + 'static` is needed so we can hold it in a spawned task.
pub trait IpcTransport: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> IpcTransport for T {}

/// Pick an IPC endpoint path that mpv will accept.
///
/// Unix: a plain filesystem path under the system temp dir.
/// Windows: a path in the `\\.\pipe\` namespace — mpv only accepts pipes that
/// live there, not filesystem paths.
pub fn generate_ipc_path() -> PathBuf {
    // Uniqueness: pid + monotonic nanos, which is more than enough for local
    // concurrent tests and avoids pulling in a uuid dep.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    #[cfg(unix)]
    {
        std::env::temp_dir().join(format!("syncmesh-{pid}-{nanos}.sock"))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\syncmesh-{pid}-{nanos}"))
    }
}

/// Open a transport to the mpv IPC endpoint at `path`, retrying until either
/// the connect succeeds or `total_wait` elapses.
///
/// mpv creates the socket/pipe a short while *after* it starts, so the first
/// few connect attempts routinely fail with `NotFound`. We poll with a small
/// fixed backoff rather than exponential — the window is usually <100 ms and
/// the wait cap is <5 s.
pub async fn connect_transport(
    path: &Path,
    total_wait: Duration,
) -> Result<Box<dyn IpcTransport>, MpvError> {
    let poll = Duration::from_millis(25);
    let deadline = tokio::time::Instant::now() + total_wait;
    loop {
        let last_err = match connect_once(path).await {
            Ok(t) => return Ok(t),
            Err(e) => e,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(MpvError::Connect {
                waited_ms: u64::try_from(total_wait.as_millis()).unwrap_or(u64::MAX),
                source: last_err,
            });
        }
        sleep(poll).await;
    }
}

#[cfg(unix)]
async fn connect_once(path: &Path) -> io::Result<Box<dyn IpcTransport>> {
    let s = tokio::net::UnixStream::connect(path).await?;
    Ok(Box::new(s))
}

#[cfg(windows)]
// Kept `async` so Windows and Unix call sites share one signature.
#[allow(clippy::unused_async)]
async fn connect_once(path: &Path) -> io::Result<Box<dyn IpcTransport>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    // mpv's named pipe is opened with GENERIC_READ|GENERIC_WRITE on its
    // side; the default ClientOptions read+write access matches.
    let p = ClientOptions::new().open(path)?;
    Ok(Box::new(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_path_has_expected_shape() {
        let p = generate_ipc_path();
        let s = p.to_string_lossy();
        #[cfg(windows)]
        {
            assert!(
                s.starts_with(r"\\.\pipe\syncmesh-"),
                "windows pipe path: {s}"
            );
        }
        #[cfg(unix)]
        {
            assert!(s.contains("syncmesh-"), "unix socket path: {s}");
        }
    }

    #[test]
    fn generated_paths_are_unique() {
        let a = generate_ipc_path();
        let b = generate_ipc_path();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn connect_times_out_when_nothing_listens() {
        let p = generate_ipc_path();
        let r = connect_transport(&p, Duration::from_millis(100)).await;
        assert!(matches!(r, Err(MpvError::Connect { .. })));
    }
}
