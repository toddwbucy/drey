//! Protocol v1: one local owner, bounded frames, serialized engine requests.
//! There is no TCP/HTTP implementation or consumer policy in this crate.
#![cfg(unix)]
use drey::engine::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 32 * 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Create { config: DatabaseConfig },
    Open {},
    Close {},
    Status {},
    Checkpoint {},
    Write { batch: Batch },
    Query { query: Query },
    Shutdown {},
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub id: u64,
    pub request: Request,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Reply {
    Ready(Status),
    Closed,
    Checkpoint,
    Written(Receipt),
    Rows(QueryResult),
    Shutdown,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Fault {
    pub code: String,
    pub message: String,
    pub retry_same_token: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub id: u64,
    pub result: Result<Reply, Fault>,
}

fn fault(e: drey::Error) -> Fault {
    use drey::Error::*;
    let (code, retry) = match &e {
        InvalidRequest(_)
        | InvalidNodeType(_)
        | InvalidEdgeType(_)
        | InvalidPropertyValue(_)
        | DimensionMismatch { .. }
        | UnsupportedQuery(_) => ("invalid_request", false),
        NodeNotFound(_) | EdgeNotFound(_) => ("not_found", false),
        ResourceLimit(_) => ("resource_limit", false),
        Conflict(_) | DanglingEdge(_) => ("conflict", false),
        LockConflict(_) => ("ownership_conflict", false),
        Storage(_) | Unavailable(_) => ("storage_unavailable", true),
        Codec(_) | IndexCorruption(_) | GenerationMismatch { .. } | VersionMismatch { .. } => {
            ("corrupt_or_incompatible", false)
        }
        _ => ("engine_error", false),
    };
    Fault {
        code: code.into(),
        message: e.to_string(),
        retry_same_token: retry,
    }
}
fn invalid(s: impl Into<String>) -> Fault {
    Fault {
        code: "invalid_request".into(),
        message: s.into(),
        retry_same_token: false,
    }
}
fn json_error(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}
pub fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds maximum",
        ));
    }
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(bytes)
}
pub fn read_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut len = [0; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame length",
        ));
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub struct Client {
    stream: UnixStream,
    next: u64,
}
impl Client {
    pub fn connect(socket: impl AsRef<Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self { stream, next: 1 })
    }
    /// A transport error after write is ambiguous: reconnect/open and repeat the
    /// identical durable token, never manufacture a new token for the retry.
    pub fn call(&mut self, request: Request) -> io::Result<Response> {
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("request IDs exhausted"))?;
        write_frame(
            &mut self.stream,
            &serde_json::to_vec(&Envelope {
                version: PROTOCOL_VERSION,
                id,
                request,
            })
            .map_err(json_error)?,
        )?;
        let response: Response =
            serde_json::from_slice(&read_frame(&mut self.stream)?).map_err(json_error)?;
        if response.version != PROTOCOL_VERSION || response.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response version/correlation mismatch",
            ));
        }
        Ok(response)
    }
}

struct Owner {
    path: PathBuf,
    engine: Option<Engine>,
}
impl Owner {
    fn dispatch(&mut self, request: Request) -> Result<Reply, Fault> {
        let result = (|| -> drey::Result<Reply> {
            match request {
                Request::Create { config } => {
                    if self.engine.is_some() {
                        return Err(drey::Error::Conflict("database is already open".into()));
                    }
                    let e = Engine::create(&self.path, config)?;
                    let s = e.status()?;
                    self.engine = Some(e);
                    Ok(Reply::Ready(s))
                }
                Request::Open {} => {
                    if self.engine.is_some() {
                        return Err(drey::Error::Conflict("database is already open".into()));
                    }
                    let e = Engine::open(&self.path)?;
                    let s = e.status()?;
                    self.engine = Some(e);
                    Ok(Reply::Ready(s))
                }
                Request::Close {} => {
                    self.engine = None;
                    Ok(Reply::Closed)
                }
                Request::Shutdown {} => {
                    self.engine = None;
                    Ok(Reply::Shutdown)
                }
                other => {
                    let e = self
                        .engine
                        .as_mut()
                        .ok_or_else(|| drey::Error::Unavailable("database is closed".into()))?;
                    match other {
                        Request::Status {} => Ok(Reply::Ready(e.status()?)),
                        Request::Checkpoint {} => {
                            e.checkpoint()?;
                            Ok(Reply::Checkpoint)
                        }
                        Request::Write { batch } => Ok(Reply::Written(e.write(batch)?)),
                        Request::Query { query } => Ok(Reply::Rows(e.query(query)?)),
                        _ => unreachable!(),
                    }
                }
            }
        })();
        result.map_err(fault)
    }
}

#[cfg(target_os = "linux")]
fn same_uid(stream: &UnixStream) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED writes exactly the checked fixed-size ucred to our live buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid == unsafe { libc::geteuid() })
}
#[cfg(not(target_os = "linux"))]
fn same_uid(_stream: &UnixStream) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer admission currently requires Linux",
    ))
}

/// Serve a fixed database path. Open/create are explicit requests; no client
/// can select another path. Private socket directory and owner lock prevent
/// stale-socket races and accidental duplicate service owners.
pub fn serve(database: impl AsRef<Path>, socket: impl AsRef<Path>) -> io::Result<()> {
    let socket = socket.as_ref();
    let parent = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let meta = fs::metadata(parent)?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket directory must be owned by this uid and mode 0700",
        ));
    }
    let lock_path = socket.with_extension("owner.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.try_lock()
        .map_err(|e| io::Error::other(format!("service owner lock: {e}")))?;
    if socket.exists() {
        if !fs::symlink_metadata(socket)?.file_type().is_socket() {
            return Err(io::Error::other("socket path is not a socket"));
        }
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let owner = Arc::new(Mutex::new(Owner {
        path: database.as_ref().to_path_buf(),
        engine: None,
    }));
    let stopped = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let mut connections = Vec::<(Arc<AtomicBool>, UnixStream)>::new();
    while !stopped.load(Ordering::Acquire) {
        // Drop cloned descriptors promptly so a completed handler produces
        // EOF even when no new connection arrives.
        connections.retain(|(done, _)| !done.load(Ordering::Acquire));
        match listener.accept() {
            Ok((mut stream, _)) => {
                if !same_uid(&stream)? || active.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                    continue;
                }
                let done = Arc::new(AtomicBool::new(false));
                connections.push((done.clone(), stream.try_clone()?));
                active.fetch_add(1, Ordering::AcqRel);
                let owner = owner.clone();
                let stopped = stopped.clone();
                let active = active.clone();
                std::thread::spawn(move || {
                    // Always release the slot, including if the owner panics.
                    struct ConnectionGuard(Arc<AtomicUsize>, Arc<AtomicBool>);
                    impl Drop for ConnectionGuard {
                        fn drop(&mut self) {
                            self.1.store(true, Ordering::Release);
                            self.0.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    let _guard = ConnectionGuard(active, done);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    while !stopped.load(Ordering::Acquire) {
                        let bytes = match read_frame(&mut stream) {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        let (id, result, shutdown) =
                            match serde_json::from_slice::<Envelope>(&bytes) {
                                Ok(e) if e.version == PROTOCOL_VERSION => {
                                    let shutdown = matches!(e.request, Request::Shutdown {});
                                    let result = match owner.lock() {
                                        Ok(mut o) => {
                                            if stopped.load(Ordering::Acquire) {
                                                Err(invalid("service is shutting down"))
                                            } else {
                                                let reply = o.dispatch(e.request);
                                                // Publish under the owner lock so queued
                                                // lifecycle requests cannot reopen afterward.
                                                if shutdown {
                                                    stopped.store(true, Ordering::Release);
                                                }
                                                reply
                                            }
                                        }
                                        Err(_) => Err(Fault {
                                            code: "storage_unavailable".into(),
                                            message: "owner panicked; restart required".into(),
                                            retry_same_token: true,
                                        }),
                                    };
                                    (e.id, result, shutdown)
                                }
                                Ok(e) => (
                                    e.id,
                                    Err(Fault {
                                        code: "unsupported_version".into(),
                                        message: format!("supported protocol: {PROTOCOL_VERSION}"),
                                        retry_same_token: false,
                                    }),
                                    false,
                                ),
                                Err(e) => (0, Err(invalid(e.to_string())), false),
                            };
                        // Also permit shutdown after an owner panic poisoned
                        // the mutex; recovery still requires a fresh process.
                        if shutdown {
                            stopped.store(true, Ordering::Release);
                        }
                        let response = Response {
                            version: PROTOCOL_VERSION,
                            id,
                            result,
                        };
                        let sent = serde_json::to_vec(&response)
                            .map_err(json_error)
                            .and_then(|b| write_frame(&mut stream, &b));
                        if sent.is_err() || shutdown {
                            break;
                        }
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(e) => return Err(e),
        }
    }
    // Interrupt idle/partial request frames, leaving response writes enabled
    // so the shutdown acknowledgement can finish before releasing ownership.
    for (done, stream) in &connections {
        if !done.load(Ordering::Acquire) {
            let _ = stream.shutdown(Shutdown::Read);
        }
    }
    while active.load(Ordering::Acquire) > 0 {
        std::thread::sleep(Duration::from_millis(2));
    }
    fs::remove_file(socket)?;
    drop(owner);
    drop(lock);
    Ok(())
}

use std::os::unix::fs::FileTypeExt;
