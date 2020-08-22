//! Filesystem session
//!
//! A session runs a filesystem implementation while it is being mounted to a specific mount
//! point. A session begins by mounting the filesystem and ends by unmounting it. While the
//! filesystem is mounted, the session loop receives, dispatches and replies to kernel requests
//! for filesystem operations under its mount point.

use std::ffi::OsString;
use std::io;
use std::fmt;
use std::path::{PathBuf, Path};
use libc::{EAGAIN, EINTR, ENODEV, ENOENT};
use log::{error, info};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};

use crate::channel::{self, Channel};
use crate::request::Request;
use crate::Filesystem;

/// The max size of write requests from the kernel. The absolute minimum is 4k,
/// FUSE recommends at least 128k, max 16M. The FUSE default is 16M on macOS
/// and 128k on other systems.
pub const MAX_WRITE_SIZE: usize = 16 * 1024 * 1024;

/// Size of the buffer for reading a request from the kernel. Since the kernel may send
/// up to MAX_WRITE_SIZE bytes in a write request, we use that value plus some extra space.
const BUFFER_SIZE: usize = MAX_WRITE_SIZE + 4096;

/// The session data structure
#[derive(Debug)]
pub struct Session<FS: Filesystem + Send + Sync + 'static> {
    /// Filesystem operation implementations
    pub filesystem: FS,
    /// Communication channel to the kernel driver
    ch: Channel,
    /// FUSE protocol major version
    pub proto_major: AtomicU32,
    /// FUSE protocol minor version
    pub proto_minor: AtomicU32,
    /// True if the filesystem is initialized (init operation done)
    pub initialized: AtomicBool,
    /// True if the filesystem was destroyed (destroy operation done)
    pub destroyed: AtomicBool,
}

impl<FS: Filesystem + Send + Sync + 'static> Session<FS> {
    /// Create a new session by mounting the given filesystem to the given mountpoint
    pub fn new(filesystem: FS, mountpoint: &Path, options: &[OsString]) -> io::Result<Session<FS>> {
        info!("Mounting {}", mountpoint.display());
        Channel::new(mountpoint, options).map(|ch| {
            Session {
                filesystem: filesystem,
                ch: ch,
                proto_major: AtomicU32::new(0),
                proto_minor: AtomicU32::new(0),
                initialized: AtomicBool::new(false),
                destroyed: AtomicBool::new(false),
            }
        })
    }

    /// Return path of the mounted filesystem
    pub fn mountpoint(&self) -> &Path {
        &self.ch.mountpoint()
    }

    /// Run the session loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem. This read-dispatch-loop is non-concurrent to prevent
    /// having multiple buffers (which take up much memory), but the filesystem methods
    /// may run concurrent by spawning threads.
    pub fn run(self) -> io::Result<()> {
        // Buffer for receiving requests from the kernel. Only one is allocated and
        // it is reused immediately after dispatching to conserve memory and allocations.
        let mut buffer: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
        let se = Arc::new(self);
        loop {
            // Read the next request from the given channel to kernel driver
            // The kernel driver makes sure that we get exactly one request per read
            match se.ch.receive(&mut buffer) {
                Ok(()) => match Request::new(se.ch.sender(), &buffer) {
                    // Dispatch request
                    Some(req) => {
                        let se = se.clone();
                        tokio::spawn( async move { req.dispatch(se).await });
                    },
                    // Quit loop on illegal request
                    None => break,
                },
                Err(err) => match err.raw_os_error() {
                    // Operation interrupted. Accordingly to FUSE, this is safe to retry
                    Some(ENOENT) => continue,
                    // Interrupted system call, retry
                    Some(EINTR) => continue,
                    // Explicitly try again
                    Some(EAGAIN) => continue,
                    // Filesystem was unmounted, quit the loop
                    Some(ENODEV) => break,
                    // Unhandled error
                    _ => return Err(err),
                }
            }
        }
        Ok(())
    }
}

impl<FS: Filesystem + Send + Sync + 'static> Session<FS> {
    /// Run the session loop in a background thread
    pub unsafe fn spawn(self) -> io::Result<BackgroundSession> {
        BackgroundSession::new(self)
    }
}

impl<FS: Filesystem + Send + Sync + 'static> Drop for Session<FS> {
    fn drop(&mut self) {
        info!("Unmounted {}", self.mountpoint().display());
    }
}

/// The background session data structure
pub struct BackgroundSession {
    /// Path of the mounted filesystem
    pub mountpoint: PathBuf,
    /// handle of the background session
    pub handle: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl BackgroundSession {
    /// Create a new background session for the given session by running its
    /// session loop in a background thread. If the returned handle is dropped,
    /// the filesystem is unmounted and the given session ends.
    pub unsafe fn new<FS: Filesystem + Send + Sync + 'static>(se: Session<FS>) -> io::Result<BackgroundSession> {
        let mountpoint = se.mountpoint().to_path_buf();
        let handle = tokio::spawn ( async move { se.run() } );
        Ok(BackgroundSession { mountpoint: mountpoint, handle: handle })
    }
}

impl Drop for BackgroundSession {
    fn drop(&mut self) {
        info!("Unmounting {}", self.mountpoint.display());
        // Unmounting the filesystem will eventually end the session loop,
        // drop the session and hence end the background thread.
        match channel::unmount(&self.mountpoint) {
            Ok(()) => (),
            Err(err) => error!("Failed to unmount {}: {}", self.mountpoint.display(), err),
        }
    }
}

// replace with #[derive(Debug)] if Debug ever gets implemented for
// thread_scoped::JoinGuard
impl fmt::Debug for BackgroundSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "BackgroundSession {{ mountpoint: {:?}, guard: JoinGuard<()> }}", self.mountpoint)
    }
}
