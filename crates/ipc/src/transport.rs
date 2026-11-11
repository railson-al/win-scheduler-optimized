//! Platform transport: a named pipe on Windows (production) and a Unix socket
//! elsewhere (development/CI). Both expose the same `Listener` / `Stream` /
//! `connect` surface, so the rest of the crate never sees the difference.
//!
//! Security in this phase (see the plan's D4):
//! * Unix: the socket lives in a directory created with mode 0700, so access is
//!   restricted from the moment the socket exists (no chmod-after-bind window).
//! * Windows: the pipe admits only SYSTEM and Administrators (an explicit DACL on
//!   every instance, see `pipe_security`) and rejects remote clients. The client
//!   also checks that the server is the installed service (see `connect`).

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    /// A live daemon already owns the endpoint (so a second one must not start).
    #[error("another ScriptRunner daemon is already listening on {0} (or another process holds that name)")]
    AlreadyRunning(String),
    /// Nothing is listening; the daemon is not running.
    #[error("the ScriptRunner daemon is not running (nothing is listening on {0})")]
    NotRunning(String),
    /// Something owns the endpoint, but it is not the ScriptRunner service.
    #[error("refusing to talk to the process listening on {0}: it is not the ScriptRunner service")]
    ServerNotVerified(String),
    /// The daemon is there but this user may not talk to it.
    #[error(
        "access to the ScriptRunner daemon ({0}) was denied; run this command as Administrator \
         (on Linux, as the user that started the daemon)"
    )]
    AccessDenied(String),
    #[error("ipc transport error: {0}")]
    Io(#[from] io::Error),
}

/// Where the daemon listens: a socket path on Unix, a pipe name on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    address: PathBuf,
    /// Create and lock down (0700) the parent directory before binding. Only
    /// done for endpoints we derive ourselves, never for caller-supplied paths,
    /// so we cannot chmod a shared directory such as `/tmp`.
    secure_parent: bool,
}

impl Endpoint {
    /// The standard endpoint for a daemon using `data_dir`.
    ///
    /// Unix: `<data_dir>/ipc/scriptrunner.sock`. Windows: the fixed pipe
    /// `\\.\pipe\scriptrunner`, so there is at most one daemon per machine.
    pub fn for_data_dir(data_dir: &Path) -> Self {
        #[cfg(unix)]
        {
            Self {
                address: data_dir.join("ipc").join("scriptrunner.sock"),
                secure_parent: true,
            }
        }
        #[cfg(windows)]
        {
            let _ = data_dir;
            Self::custom(r"\\.\pipe\scriptrunner")
        }
    }

    /// An explicit socket path (Unix) or pipe name (Windows). The caller is
    /// responsible for the parent directory's permissions.
    pub fn custom(address: impl Into<PathBuf>) -> Self {
        Self {
            address: address.into(),
            secure_parent: false,
        }
    }

    pub fn address(&self) -> &Path {
        &self.address
    }

    fn describe(&self) -> String {
        self.address.display().to_string()
    }
}

pub use imp::{Listener, Stream};

/// Connects to a running daemon. Fails with `TransportError::NotRunning` when
/// nothing is listening.
pub async fn connect(endpoint: &Endpoint) -> Result<Stream, TransportError> {
    imp::connect(endpoint).await
}

#[cfg(unix)]
mod imp {
    use std::fs::{DirBuilder, File, OpenOptions, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};

    use super::{Endpoint, TransportError};

    pub type Stream = UnixStream;

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
        /// Holds the exclusive `flock` for as long as this daemon listens. The
        /// kernel releases it if the process dies, so a crash never leaves the
        /// endpoint locked.
        _lock: File,
    }

    impl Listener {
        /// Starts listening.
        ///
        /// Ownership of the endpoint is decided by an exclusive `flock` on
        /// `<socket>.lock`, taken before anything else: a second daemon fails
        /// with `TransportError::AlreadyRunning`, and two daemons starting at
        /// once cannot both win. Because the lock is held, a socket file already
        /// at the path belongs to a dead daemon and is replaced. (Probing the
        /// socket instead would race: both candidates could see it as stale and
        /// one would unlink the other's live socket.)
        pub async fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
            let path = endpoint.address();
            if endpoint.secure_parent
                && let Some(dir) = path.parent()
            {
                DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
                std::fs::set_permissions(dir, Permissions::from_mode(0o700))?;
            }

            let lock = take_lock(endpoint)?;
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
                // Never delete something that is not ours to delete.
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    )
                    .into());
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            let inner = UnixListener::bind(path)?;
            Ok(Self {
                inner,
                path: path.to_path_buf(),
                _lock: lock,
            })
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            self.inner.accept().await.map(|(stream, _)| stream)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Runs while `_lock` is still held, so no other daemon can have bound a
            // socket here in the meantime. Best effort: a leftover file is replaced by the next `bind`.
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn take_lock(endpoint: &Endpoint) -> Result<File, TransportError> {
        let mut lock_path = endpoint.address().as_os_str().to_owned();
        lock_path.push(".lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&lock_path)?;
        // SAFETY: `flock` only reads the descriptor, which `file` keeps open and valid for this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            return Err(match err.kind() {
                io::ErrorKind::WouldBlock => TransportError::AlreadyRunning(endpoint.describe()),
                _ => err.into(),
            });
        }
        Ok(file)
    }

    pub async fn connect(endpoint: &Endpoint) -> Result<Stream, TransportError> {
        UnixStream::connect(endpoint.address())
            .await
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                    TransportError::NotRunning(endpoint.describe())
                }
                io::ErrorKind::PermissionDenied => TransportError::AccessDenied(endpoint.describe()),
                _ => err.into(),
            })
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;

    use super::{Endpoint, TransportError};
    use crate::pipe_security::PipeSecurity;
    use crate::service_check::{self, ServiceProcess};

    /// `ERROR_PIPE_BUSY`: every server instance is currently serving a client.
    const ERROR_PIPE_BUSY: i32 = 231;
    const BUSY_RETRY: Duration = Duration::from_millis(50);
    const BUSY_ATTEMPTS: u32 = 100;

    pub enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    impl AsyncRead for Stream {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Server(pipe) => Pin::new(pipe).poll_read(cx, buf),
                Self::Client(pipe) => Pin::new(pipe).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Self::Server(pipe) => Pin::new(pipe).poll_write(cx, buf),
                Self::Client(pipe) => Pin::new(pipe).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Server(pipe) => Pin::new(pipe).poll_flush(cx),
                Self::Client(pipe) => Pin::new(pipe).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Server(pipe) => Pin::new(pipe).poll_shutdown(cx),
                Self::Client(pipe) => Pin::new(pipe).poll_shutdown(cx),
            }
        }
    }

    pub struct Listener {
        endpoint: Endpoint,
        /// The instance clients connect to next; always created ahead of time so
        /// there is never a moment with no instance (clients would get NotFound).
        next: NamedPipeServer,
        /// Applied to *every* instance, not only the first: a later instance with the
        /// default DACL would be a hole next to the locked-down one.
        security: PipeSecurity,
    }

    impl Listener {
        /// `first_pipe_instance(true)` makes creation fail when any instance of
        /// this pipe already exists, so a second daemon cannot start. The pipe is
        /// created with an explicit DACL admitting only SYSTEM and Administrators.
        pub async fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
            let mut security = PipeSecurity::admins_only()?;
            // SAFETY: the pointer comes from `security`, a valid SECURITY_ATTRIBUTES that
            // outlives the call; `create` copies the descriptor.
            let next = unsafe {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(endpoint.address(), security.as_attributes_ptr())
            }
            .map_err(|err| match err.kind() {
                io::ErrorKind::PermissionDenied => TransportError::AlreadyRunning(endpoint.describe()),
                _ => err.into(),
            })?;
            Ok(Self {
                endpoint: endpoint.clone(),
                next,
                security,
            })
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            self.next.connect().await?;
            // Create the replacement *before* handing over the connected one.
            // SAFETY: as in `bind`; `self.security` is alive for the whole call.
            let fresh = unsafe {
                ServerOptions::new()
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(self.endpoint.address(), self.security.as_attributes_ptr())
            }?;
            Ok(Stream::Server(std::mem::replace(&mut self.next, fresh)))
        }
    }

    /// When the ScriptRunner service is installed, the pipe's server must be that
    /// service's process. Installed but not running means whatever owns the pipe
    /// name is an impostor, so the client refuses rather than send it anything.
    /// With no service installed (development, `scriptrunner daemon`) there is
    /// nothing to compare against, so the check is skipped.
    ///
    /// The client keeps tokio's default `SECURITY_IDENTIFICATION`, so even a
    /// server that slipped through could not impersonate the caller.
    pub async fn connect(endpoint: &Endpoint) -> Result<Stream, TransportError> {
        let expected_server = match service_check::query()? {
            ServiceProcess::NotInstalled => None,
            ServiceProcess::NotRunning => return Err(TransportError::NotRunning(endpoint.describe())),
            ServiceProcess::Running(pid) => Some(pid),
        };
        for _ in 0..BUSY_ATTEMPTS {
            match ClientOptions::new().open(endpoint.address()) {
                Ok(client) => {
                    if let Some(expected) = expected_server {
                        let mut server_pid = 0u32;
                        // SAFETY: the handle is this client's own open pipe handle, and the out-pointer is a valid local.
                        let got =
                            unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut server_pid) };
                        if got == 0 {
                            return Err(io::Error::last_os_error().into());
                        }
                        if server_pid != expected {
                            return Err(TransportError::ServerNotVerified(endpoint.describe()));
                        }
                    }
                    return Ok(Stream::Client(client));
                }
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => tokio::time::sleep(BUSY_RETRY).await,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Err(TransportError::NotRunning(endpoint.describe()));
                }
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(TransportError::AccessDenied(endpoint.describe()));
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "the daemon's pipe stayed busy").into())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[cfg(unix)]
    fn endpoint(dir: &tempfile::TempDir) -> Endpoint {
        Endpoint::for_data_dir(dir.path())
    }

    #[cfg(windows)]
    fn endpoint(_dir: &tempfile::TempDir) -> Endpoint {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Endpoint::custom(format!(
            r"\\.\pipe\scriptrunner-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn bytes_flow_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        let mut listener = Listener::bind(&endpoint).await.unwrap();

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            stream.write_all(&buf).await.unwrap();
            // Keep the listener alive until the echo is written.
            listener
        });

        let mut client = connect(&endpoint).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn serves_several_clients_in_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        let mut listener = Listener::bind(&endpoint).await.unwrap();

        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let mut stream = listener.accept().await.unwrap();
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                stream.write_all(&byte).await.unwrap();
            }
            listener
        });

        for n in 0..3u8 {
            let mut client = connect(&endpoint).await.unwrap();
            client.write_all(&[n]).await.unwrap();
            let mut back = [0u8; 1];
            client.read_exact(&mut back).await.unwrap();
            assert_eq!(back[0], n);
        }
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn a_second_listener_is_refused_while_the_first_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        let _first = Listener::bind(&endpoint).await.unwrap();
        assert!(matches!(
            Listener::bind(&endpoint).await,
            Err(TransportError::AlreadyRunning(_))
        ));
    }

    #[tokio::test]
    async fn connecting_without_a_daemon_says_so() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            connect(&endpoint(&dir)).await,
            Err(TransportError::NotRunning(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stale_socket_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        // A std listener leaves its socket file behind when dropped, like a crashed daemon.
        std::fs::create_dir_all(endpoint.address().parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(endpoint.address()).unwrap());
        assert!(endpoint.address().exists());

        let _listener = Listener::bind(&endpoint).await.unwrap();
        assert!(connect(&endpoint).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn racing_daemons_cannot_both_win_even_over_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        std::fs::create_dir_all(endpoint.address().parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(endpoint.address()).unwrap());

        let attempts: Vec<_> = (0..8)
            .map(|_| {
                let endpoint = endpoint.clone();
                tokio::spawn(async move { Listener::bind(&endpoint).await })
            })
            .collect();
        let mut winners = 0;
        let mut kept = Vec::new();
        for attempt in attempts {
            match attempt.await.unwrap() {
                Ok(listener) => {
                    winners += 1;
                    kept.push(listener);
                }
                Err(err) => assert!(matches!(err, TransportError::AlreadyRunning(_)), "got {err:?}"),
            }
        }
        assert_eq!(winners, 1);
        assert!(
            connect(&endpoint).await.is_ok(),
            "the winner's socket must stay reachable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn something_that_is_not_a_socket_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        std::fs::create_dir_all(endpoint.address().parent().unwrap()).unwrap();
        std::fs::write(endpoint.address(), b"precious").unwrap();

        assert!(matches!(Listener::bind(&endpoint).await, Err(TransportError::Io(_))));
        assert_eq!(std::fs::read(endpoint.address()).unwrap(), b"precious");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_socket_directory_is_private_and_cleaned_up() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&dir);
        let listener = Listener::bind(&endpoint).await.unwrap();

        let mode = std::fs::metadata(endpoint.address().parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        drop(listener);
        assert!(!endpoint.address().exists());
    }
}
