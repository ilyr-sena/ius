//! Cross-platform IPC transport.
//!
//! The daemon listens and clients connect over an [`Endpoint`], which is one
//! of:
//!   - `unix:/path/to/socket`  — unix domain socket (default on unix)
//!   - `pipe:name`             — windows named pipe `\\.\pipe\name` (default on windows)
//!   - `tcp:host:port`         — loopback TCP (opt-in on both platforms)
//!
//! [`TransportStream`] unifies the accepted stream types so the rest of the
//! daemon is fully transport-agnostic.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tracing::warn;

/// A parsed daemon endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    NamedPipe(String),       // bare pipe name, e.g. "meridian-relay"
    Tcp(std::net::SocketAddr),
}

impl Endpoint {
    /// Parse an endpoint string. Bare values (no scheme) are interpreted
    /// per-platform: a path on unix, a pipe name on windows. Bare values
    /// work on both for back-compat with `--socket-path`.
    pub fn parse(input: &str) -> Result<Self, String> {
        if let Some(rest) = input.strip_prefix("unix:") {
            return Ok(Endpoint::Unix(PathBuf::from(rest)));
        }
        if let Some(rest) = input.strip_prefix("pipe:") {
            return Self::parse_pipe(rest);
        }
        if let Some(rest) = input.strip_prefix("tcp:") {
            return Self::parse_tcp(rest);
        }

        // Bare value — platform default scheme.
        #[cfg(unix)]
        {
            Ok(Endpoint::Unix(PathBuf::from(input)))
        }
        #[cfg(windows)]
        {
            Self::parse_pipe(input)
        }
    }

    fn parse_pipe(name: &str) -> Result<Self, String> {
        if name.is_empty()
            || name.len() > 256
            || name.contains(['/', '\\'])
            || name.contains("..")
            || name.contains(|c: char| c.is_control())
        {
            return Err(format!("invalid pipe name: '{name}'"));
        }
        Ok(Endpoint::NamedPipe(name.to_string()))
    }

    fn parse_tcp(addr: &str) -> Result<Self, String> {
        let addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| format!("invalid tcp endpoint '{addr}': {e}"))?;
        if !addr.ip().is_loopback() {
            return Err("TCP endpoint must be loopback (the IPC protocol is unauthenticated)".into());
        }
        Ok(Endpoint::Tcp(addr))
    }

    pub fn display_string(&self) -> String {
        match self {
            Endpoint::Unix(p) => format!("unix:{}", p.display()),
            Endpoint::NamedPipe(n) => format!("pipe:{n}"),
            Endpoint::Tcp(a) => format!("tcp:{a}"),
        }
    }

    /// Connect to this endpoint (client side).
    pub async fn connect(&self) -> io::Result<TransportStream> {
        match self {
            Endpoint::Unix(path) => {
                #[cfg(unix)]
                {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok(TransportStream::Unix(stream))
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Err(io::Error::new(io::ErrorKind::Unsupported, "unix sockets not supported on this platform"))
                }
            }
            Endpoint::Tcp(addr) => {
                let stream = TcpStream::connect(addr).await?;
                let _ = stream.set_nodelay(true);
                Ok(TransportStream::Tcp(stream))
            }
            Endpoint::NamedPipe(name) => {
                #[cfg(windows)]
                {
                    let full = format!(r"\\.\pipe\{name}");
                    // ClientOptions::open is synchronous; it errors with
                    // ERROR_PIPE_BUSY if all server instances are connected.
                    // Retry briefly so clients can slip in between instances.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    loop {
                        match tokio::net::windows::named_pipe::ClientOptions::new().open(&full) {
                            Ok(client) => break Ok(TransportStream::Pipe(Box::new(client))),
                            Err(e)
                                if e.raw_os_error()
                                    == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32)
                                && std::time::Instant::now() < deadline =>
                            {
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            }
                            Err(e) => break Err(e),
                        }
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    Err(io::Error::new(io::ErrorKind::Unsupported, "named pipes are windows-only"))
                }
            }
        }
    }
}

/// The listener side: accepts connections from an endpoint.
pub enum TransportListener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    Tcp(TcpListener),
    #[cfg(windows)]
    Pipe { name: String, sddl: String },
}

impl TransportListener {
    pub async fn bind(endpoint: &Endpoint, security_sddl: &str) -> io::Result<Self> {
        match endpoint {
            Endpoint::Unix(path) => {
                #[cfg(unix)]
                {
                    let listener = tokio::net::UnixListener::bind(path)?;
                    Ok(TransportListener::Unix(listener))
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Err(io::Error::new(io::ErrorKind::Unsupported, "unix sockets unsupported"))
                }
            }
            Endpoint::Tcp(addr) => {
                let listener = TcpListener::bind(addr).await?;
                Ok(TransportListener::Tcp(listener))
            }
            Endpoint::NamedPipe(name) => {
                #[cfg(windows)]
                {
                    // Create the first instance eagerly so bind-time errors
                    // (SDDL parse failure, already-exists) surface now.
                    let _first = create_pipe_instance(name, Some(security_sddl))?;
                    Ok(TransportListener::Pipe {
                        name: name.clone(),
                        sddl: security_sddl.to_string(),
                    })
                }
                #[cfg(not(windows))]
                {
                    let _ = (name, security_sddl);
                    Err(io::Error::new(io::ErrorKind::Unsupported, "named pipes are windows-only"))
                }
            }
        }
    }

    /// Accept one client, producing a transport-agnostic stream.
    /// For named pipes, accepts the buffered first instance and immediately
    /// prepares the next one.
    pub async fn accept(&mut self) -> io::Result<(TransportStream, crate::platform::PeerIdentity)> {
        match self {
            #[cfg(unix)]
            TransportListener::Unix(l) => {
                let (stream, _) = l.accept().await?;
                let peer = crate::platform::peer_identity_of_unix(&stream);
                Ok((TransportStream::Unix(stream), peer))
            }
            TransportListener::Tcp(l) => {
                let (stream, addr) = l.accept().await?;
                if !addr.ip().is_loopback() {
                    warn!("rejecting non-loopback TCP client {addr}");
                    drop(stream);
                    // Not a fatal error: the caller loops accept again.
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "non-loopback"));
                }
                let _ = stream.set_nodelay(true);
                Ok((TransportStream::Tcp(stream), crate::platform::anonymous_peer()))
            }
            #[cfg(windows)]
            TransportListener::Pipe { name, sddl } => {
                // The previous accept left a fresh instance buffered inside
                // `pending`. On first call we create one on the spot.
                let server = create_pipe_instance(name, Some(sddl))?;
                server.connect().await?;
                let peer = crate::platform::peer_identity_of_pipe(&server);
                Ok((TransportStream::Pipe(Box::new(server)), peer))
            }
        }
    }
}

/// Both named-pipe client and server ends implement the same I/O traits;
/// unify behind a trait so one variant can hold either side.
#[cfg(windows)]
pub trait PipeIo: AsyncRead + AsyncWrite + Unpin + Send {}
#[cfg(windows)]
impl PipeIo for tokio::net::windows::named_pipe::NamedPipeServer {}
#[cfg(windows)]
impl PipeIo for tokio::net::windows::named_pipe::NamedPipeClient {}

/// The unified duplex stream handed to connection handlers.
pub enum TransportStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    Tcp(TcpStream),
    #[cfg(windows)]
    Pipe(Box<dyn PipeIo>),
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            TransportStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
            TransportStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            TransportStream::Pipe(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            TransportStream::Unix(s) => Pin::new(s).poll_write(cx, data),
            TransportStream::Tcp(s) => Pin::new(s).poll_write(cx, data),
            #[cfg(windows)]
            TransportStream::Pipe(s) => Pin::new(&mut **s).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            TransportStream::Unix(s) => Pin::new(s).poll_flush(cx),
            TransportStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            TransportStream::Pipe(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            TransportStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
            TransportStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            TransportStream::Pipe(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}

/// Create a named-pipe instance with the given SDDL applied at creation time
/// (no race window).
#[cfg(windows)]
fn create_pipe_instance(
    name: &str,
    sddl: Option<&str>,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

    let full = format!(r"\\.\pipe\{name}");
    let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();

    let (sd, _size) = match sddl {
        Some(s) => Some(unsafe { crate::platform::windows::sddl_to_security_descriptor(s) }?),
        None => None,
    }
    .map(|(s, z)| (Some(s), z))
    .unwrap_or((None, 0));

    let sd_ptr = sd.map(|s| s).unwrap_or(std::ptr::null_mut());

    let mut sec_attr = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd_ptr as _,
        bInheritHandle: 0,
    };

    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            if sd_ptr.is_null() { std::ptr::null() } else { &mut sec_attr },
        )
    };

    if !sd_ptr.is_null() {
        unsafe { crate::platform::windows::free_security_descriptor(sd_ptr) };
    }

    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: we own this fresh pipe handle.
    Ok(unsafe { tokio::net::windows::named_pipe::NamedPipeServer::from_raw_handle(handle as _)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_unix_scheme() {
        let ep = Endpoint::parse("unix:/tmp/foo.sock").unwrap();
        assert!(matches!(ep, Endpoint::Unix(p) if p == PathBuf::from("/tmp/foo.sock")));
    }

    #[test]
    fn test_parse_pipe_scheme() {
        let ep = Endpoint::parse("pipe:meridian-relay").unwrap();
        assert_eq!(ep, Endpoint::NamedPipe("meridian-relay".into()));
    }

    #[test]
    fn test_parse_tcp_loopback_ok() {
        let ep = Endpoint::parse("tcp:127.0.0.1:27015").unwrap();
        assert_eq!(
            ep,
            Endpoint::Tcp("127.0.0.1:27015".parse().unwrap())
        );
    }

    #[test]
    fn test_parse_tcp_rejects_non_loopback() {
        assert!(Endpoint::parse("tcp:0.0.0.0:27015").is_err());
        assert!(Endpoint::parse("tcp:192.168.1.5:27015").is_err());
        assert!(Endpoint::parse("tcp:[::1]:27015").is_ok());
    }

    #[test]
    fn test_parse_pipe_rejects_traversal() {
        assert!(Endpoint::parse("pipe:../evil").is_err());
        assert!(Endpoint::parse("pipe:a/b").is_err());
        assert!(Endpoint::parse("pipe:a\\b").is_err());
        assert!(Endpoint::parse("pipe:").is_err());
    }

    #[test]
    fn test_bare_value_platform_scheme() {
        let ep = Endpoint::parse("meridian-relay").unwrap();
        #[cfg(unix)]
        assert!(matches!(ep, Endpoint::Unix(_)));
        #[cfg(windows)]
        assert!(matches!(ep, Endpoint::NamedPipe(_)));
    }

    #[test]
    fn test_display_roundtrip() {
        for s in ["unix:/tmp/a.sock", "pipe:foo", "tcp:127.0.0.1:1"] {
            let ep = Endpoint::parse(s).unwrap();
            assert_eq!(Endpoint::parse(&ep.display_string()).unwrap(), ep);
        }
    }
}
