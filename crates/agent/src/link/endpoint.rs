//! Where the local links listen, who may connect, and how peers are identified.
//!
//! * Windows: named pipes. The service pipe admits SYSTEM and Administrators only (the
//!   session host runs as SYSTEM); a UI pipe admits SYSTEM and interactive users. Every
//!   accepted client must be this same executable (`GetNamedPipeClientProcessId` →
//!   `QueryFullProcessImageNameW`).
//! * Unix (macOS): a socket in a root-owned directory. Session hosts run as the signed-in user,
//!   so the socket itself is world-connectable and the peer is identified by `LOCAL_PEERPID` →
//!   `proc_pidpath` instead, which must again be this executable.
//!
//! `REMOTE_AGENT_SERVICE_LINK` overrides the service endpoint (tests, side-by-side dev builds).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};

/// A connected link stream.
pub trait LinkStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> LinkStream for T {}

pub type BoxStream = Box<dyn LinkStream>;

/// Who may connect to an endpoint (before the executable check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// LocalSystem and Administrators (Windows). On Unix: anyone who can reach the socket.
    Privileged,
    /// LocalSystem and interactive users (Windows UI pipe).
    InteractiveUsers,
}

/// What the OS tells us about the process on the other end.
#[derive(Debug, Clone, Default)]
pub struct PeerInfo {
    pub pid: Option<u32>,
    pub exe: Option<PathBuf>,
    /// Windows session id of the peer process.
    pub session: Option<u32>,
}

/// The service link endpoint.
pub fn service_endpoint() -> String {
    if let Some(v) = std::env::var_os("REMOTE_AGENT_SERVICE_LINK") {
        return v.to_string_lossy().into_owned();
    }
    #[cfg(windows)]
    {
        r"\\.\pipe\RemoteAgent.service".to_string()
    }
    #[cfg(not(windows))]
    {
        "/var/run/remote-agent/service.sock".to_string()
    }
}

/// The UI link endpoint of the session host in Windows session `session`.
pub fn ui_endpoint(session: u32) -> String {
    if let Some(v) = std::env::var_os("REMOTE_AGENT_UI_LINK") {
        return v.to_string_lossy().into_owned();
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\RemoteAgent.ui.{session}")
    }
    #[cfg(not(windows))]
    {
        format!("/tmp/remote-agent-ui-{session}.sock")
    }
}

/// Whether `peer` runs this same executable. Paths are compared after canonicalisation
/// (and case-insensitively on Windows).
pub fn peer_is_self(peer: &PeerInfo) -> bool {
    let Some(theirs) = peer.exe.as_ref() else {
        return false;
    };
    let Ok(ours) = std::env::current_exe() else {
        return false;
    };
    let canon = |p: &PathBuf| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
    let (a, b) = (canon(&ours), canon(theirs));
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Reject peers that are not this executable. Debug builds can opt out for tests that drive
/// the link from another binary (`REMOTE_AGENT_LINK_TRUST_ANY=1`).
pub fn check_peer(peer: &PeerInfo) -> Result<()> {
    if cfg!(debug_assertions) && std::env::var_os("REMOTE_AGENT_LINK_TRUST_ANY").is_some() {
        return Ok(());
    }
    if peer_is_self(peer) {
        return Ok(());
    }
    bail!(
        "refusing link peer pid {:?} ({}): not this executable",
        peer.pid,
        peer.exe
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown image".into())
    )
}

pub use imp::{connect, Listener};

// ─── Unix ────────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        pub async fn bind(name: &str, _access: Access) -> Result<Self> {
            let path = PathBuf::from(name);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            // A stale socket from a previous run (crash, kill -9) blocks bind.
            let _ = std::fs::remove_file(&path);
            let inner = UnixListener::bind(&path)
                .with_context(|| format!("binding link socket {}", path.display()))?;
            {
                use std::os::unix::fs::PermissionsExt;
                // Hosts run as the signed-in user; the executable check below is the gate.
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                    .with_context(|| format!("chmod {}", path.display()))?;
            }
            Ok(Self { inner, path })
        }

        pub async fn accept(&mut self) -> Result<(BoxStream, PeerInfo)> {
            let (stream, _) = self.inner.accept().await.context("accepting link")?;
            let peer = peer_info(&stream);
            Ok((Box::new(stream), peer))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub async fn connect(name: &str) -> Result<BoxStream> {
        let s = UnixStream::connect(name)
            .await
            .with_context(|| format!("connecting to {name}"))?;
        Ok(Box::new(s))
    }

    #[cfg(target_os = "macos")]
    fn peer_info(stream: &UnixStream) -> PeerInfo {
        use std::os::fd::AsRawFd;
        // <sys/un.h>: SOL_LOCAL = 0, LOCAL_PEERPID = 0x002.
        const SOL_LOCAL: libc::c_int = 0;
        const LOCAL_PEERPID: libc::c_int = 0x002;
        let fd = stream.as_raw_fd();
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid`/`len` are valid out-parameters sized for the option.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                SOL_LOCAL,
                LOCAL_PEERPID,
                &mut pid as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 || pid <= 0 {
            return PeerInfo::default();
        }
        let mut buf = vec![0u8; 4096];
        // SAFETY: the buffer is writable for its full length.
        let n = unsafe {
            libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32)
        };
        let exe = (n > 0).then(|| {
            buf.truncate(n as usize);
            PathBuf::from(String::from_utf8_lossy(&buf).into_owned())
        });
        PeerInfo {
            pid: Some(pid as u32),
            exe,
            session: None,
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn peer_info(stream: &UnixStream) -> PeerInfo {
        let pid = stream
            .peer_cred()
            .ok()
            .and_then(|c| c.pid())
            .map(|p| p as u32);
        let exe = pid.and_then(|p| std::fs::read_link(format!("/proc/{p}/exe")).ok());
        PeerInfo {
            pid,
            exe,
            session: None,
        }
    }
}

// ─── Windows ─────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    /// Generic all to SYSTEM and Administrators; protected (no inheritance).
    const SDDL_PRIVILEGED: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
    /// Plus read/write for interactive users (the UI process of the signed-in user).
    const SDDL_INTERACTIVE: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

    pub struct Listener {
        name: String,
        next: NamedPipeServer,
        sd: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: the security descriptor is an immutable heap block owned by the listener.
    unsafe impl Send for Listener {}

    impl Listener {
        pub async fn bind(name: &str, access: Access) -> Result<Self> {
            let sddl = HSTRING::from(match access {
                Access::Privileged => SDDL_PRIVILEGED,
                Access::InteractiveUsers => SDDL_INTERACTIVE,
            });
            let mut sd = PSECURITY_DESCRIPTOR::default();
            // SAFETY: valid SDDL string; the descriptor is freed with LocalFree in Drop.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    &sddl,
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )
                .context("building the link pipe security descriptor")?;
            }
            let next = create(name, sd, true)?;
            Ok(Self {
                name: name.to_string(),
                next,
                sd,
            })
        }

        pub async fn accept(&mut self) -> Result<(BoxStream, PeerInfo)> {
            self.next.connect().await.context("accepting link")?;
            // Have the next instance ready before handing this one out.
            let fresh = create(&self.name, self.sd, false)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            let peer = peer_info(HANDLE(connected.as_raw_handle()));
            Ok((Box::new(connected), peer))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            if !self.sd.0.is_null() {
                // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.sd.0)));
                }
            }
        }
    }

    fn create(name: &str, sd: PSECURITY_DESCRIPTOR, first: bool) -> Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mut opts = ServerOptions::new();
        // `first_pipe_instance` refuses to start when another process already owns the name
        // (a second service, or a squatter that created the pipe first).
        opts.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `sa` and the descriptor it points to outlive the call.
        unsafe { opts.create_with_security_attributes_raw(name, &mut sa as *mut _ as *mut _) }
            .with_context(|| format!("creating link pipe {name}"))
    }

    pub async fn connect(name: &str) -> Result<BoxStream> {
        // ERROR_PIPE_BUSY: every instance is taken for a moment; retry briefly.
        const ERROR_PIPE_BUSY: i32 = 231;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(name) {
                Ok(c) => return Ok(Box::new(c)),
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e).with_context(|| format!("connecting to {name}")),
            }
        }
    }

    fn peer_info(pipe: HANDLE) -> PeerInfo {
        use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let mut pid = 0u32;
        // SAFETY: `pipe` is a connected server end; `pid` is a valid out-parameter.
        if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.is_err() || pid == 0 {
            return PeerInfo::default();
        }
        let mut session = 0u32;
        // SAFETY: plain query with a valid out-parameter.
        let session = unsafe { ProcessIdToSessionId(pid, &mut session) }
            .ok()
            .map(|_| session);
        // SAFETY: the process handle is closed below; the buffer is sized as advertised.
        let exe = unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .ok()
                .and_then(|h| {
                    let mut buf = vec![0u16; 32768];
                    let mut len = buf.len() as u32;
                    let r = QueryFullProcessImageNameW(
                        h,
                        PROCESS_NAME_WIN32,
                        windows::core::PWSTR(buf.as_mut_ptr()),
                        &mut len,
                    );
                    let _ = CloseHandle(h);
                    r.ok()
                        .map(|_| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
                })
        };
        PeerInfo {
            pid: Some(pid),
            exe,
            session,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::link::frame::{read_frame, write_frame};

    #[tokio::test]
    async fn unix_link_accepts_this_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.sock").to_string_lossy().into_owned();
        let mut listener = Listener::bind(&path, Access::Privileged).await.unwrap();
        let client = tokio::spawn({
            let path = path.clone();
            async move {
                let mut s = connect(&path).await.unwrap();
                write_frame(&mut s, &"hi").await.unwrap();
                let reply: String = read_frame(&mut s).await.unwrap().unwrap();
                reply
            }
        });
        let (mut s, peer) = listener.accept().await.unwrap();
        // Same process on both ends: the executable check must pass.
        check_peer(&peer).unwrap();
        let hi: String = read_frame(&mut s).await.unwrap().unwrap();
        assert_eq!(hi, "hi");
        write_frame(&mut s, &"hello").await.unwrap();
        assert_eq!(client.await.unwrap(), "hello");
    }

    #[test]
    fn foreign_executables_are_refused() {
        let peer = PeerInfo {
            pid: Some(1),
            exe: Some(PathBuf::from("/usr/bin/true")),
            session: None,
        };
        assert!(!peer_is_self(&peer));
        assert!(!peer_is_self(&PeerInfo::default()));
    }
}
