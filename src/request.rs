//! Filesystem operation request
//!
//! A request represents information about a filesystem operation the kernel driver wants us to
//! perform.
//!
//! TODO: This module is meant to go away soon in favor of `ll::Request`.

use std::convert::TryFrom;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use libc::{EIO, ENOSYS, EPROTO};
use fuse_abi::*;
use fuse_abi::consts::*;
use log::{debug, error, warn};
use std::sync::Arc;

use crate::channel::ChannelSender;
use crate::ll;
use crate::reply::{Reply, ReplyRaw, ReplyEmpty, ReplyDirectory};
use crate::session::{MAX_WRITE_SIZE, Session};
use crate::Filesystem;

/// We generally support async reads
#[cfg(not(target_os = "macos"))]
const INIT_FLAGS: u32 = FUSE_ASYNC_READ;
// TODO: Add FUSE_EXPORT_SUPPORT and FUSE_BIG_WRITES (requires ABI 7.10)

/// On macOS, we additionally support case insensitiveness, volume renames and xtimes
/// TODO: we should eventually let the filesystem implementation decide which flags to set
#[cfg(target_os = "macos")]
const INIT_FLAGS: u32 = FUSE_ASYNC_READ | FUSE_CASE_INSENSITIVE | FUSE_VOL_RENAME | FUSE_XTIMES;
// TODO: Add FUSE_EXPORT_SUPPORT and FUSE_BIG_WRITES (requires ABI 7.10)

/// Request data structure
#[derive(Debug)]
pub struct Request {
    /// Channel sender for sending the reply
    ch: ChannelSender,
    /// Request raw data
    //data: &'a [u8],
    /// Parsed request
    request: ll::Request,
}

impl Request {
    /// Create a new request from the given data
    pub fn new(ch: ChannelSender, data: &[u8]) -> Option<Request> {
        let request = match ll::Request::try_from(data) {
            Ok(request) => request,
            Err(err) => {
                // FIXME: Reply with ENOSYS?
                error!("{}", err);
                return None;
            }
        };

        Some(Self {ch, request})
    }

    /// Dispatch request to the given filesystem.
    /// This calls the appropriate filesystem operation method for the
    /// request and sends back the returned reply to the kernel
    pub async fn dispatch<FS: Filesystem + Send + Sync + 'static>(self, se: &mut Session<FS>) {
        let req = &self;
        debug!("{}", req.request);

        match req.request.operation() {
            // Filesystem initialization
            ll::Operation::Init { arg } => {
                let reply: ReplyRaw<fuse_init_out> = req.reply();
                // We don't support ABI versions before 7.6
                if arg.major < 7 || (arg.major == 7 && arg.minor < 6) {
                    error!("Unsupported FUSE ABI version {}.{}", arg.major, arg.minor);
                    reply.error(EPROTO);
                    return;
                }
                // Remember ABI version supported by kernel
                se.proto_major = arg.major;
                se.proto_minor = arg.minor;
                // Call filesystem init method and give it a chance to return an error
                let res = se.filesystem.init(req).await;
                if let Err(err) = res {
                    reply.error(err);
                    return;
                }
                // Reply with our desired version and settings. If the kernel supports a
                // larger major version, it'll re-send a matching init message. If it
                // supports only lower major versions, we replied with an error above.
                let init = fuse_init_out {
                    major: FUSE_KERNEL_VERSION,
                    minor: FUSE_KERNEL_MINOR_VERSION,
                    max_readahead: arg.max_readahead,       // accept any readahead size
                    flags: arg.flags & INIT_FLAGS,          // use features given in INIT_FLAGS and reported as capable
                    unused: 0,
                    max_write: MAX_WRITE_SIZE as u32,       // use a max write size that fits into the session's buffer
                };
                debug!("INIT response: ABI {}.{}, flags {:#x}, max readahead {}, max write {}", init.major, init.minor, init.flags, init.max_readahead, init.max_write);
                se.initialized = true;
                reply.ok(&init);
            }
            // Any operation is invalid before initialization
            _ if !se.initialized => {
                warn!("Ignoring FUSE operation before init: {}", req.request);
                req.reply::<ReplyEmpty>().error(EIO);
            }
            // Filesystem destroyed
            ll::Operation::Destroy => {
                se.filesystem.destroy(req).await;
                se.destroyed = true;
                req.reply::<ReplyEmpty>().ok();
            }
            // Any operation is invalid after destroy
            _ if se.destroyed => {
                warn!("Ignoring FUSE operation after destroy: {}", req.request);
                req.reply::<ReplyEmpty>().error(EIO);
            }

            ll::Operation::Interrupt { .. } => {
                // TODO: handle FUSE_INTERRUPT
                req.reply::<ReplyEmpty>().error(ENOSYS);
            }

            _ => { 
                let filesystem = se.filesystem.clone();
                tokio::spawn(async move {
                    self.dispatch_other(filesystem).await;
                });
            }
        }
    }

    /// Dispatch request to the given filesystem except Init and Destroy
    pub async fn dispatch_other<FS: Filesystem+Send+Sync>(self, filesystem: Arc<FS>) {
        let req = &self;
        match req.request.operation() {
            ll::Operation::Lookup { name } => {
                filesystem.lookup(req, req.request.nodeid(), &name, req.reply()).await;
            }
            ll::Operation::Forget { arg } => {
                filesystem.forget(req, req.request.nodeid(), arg.nlookup).await; // no reply
            }
            ll::Operation::GetAttr => {
                filesystem.getattr(req, req.request.nodeid(), req.reply()).await;
            }
            ll::Operation::SetAttr { arg } => {
                let mode = match arg.valid & FATTR_MODE {
                    0 => None,
                    _ => Some(arg.mode),
                };
                let uid = match arg.valid & FATTR_UID {
                    0 => None,
                    _ => Some(arg.uid),
                };
                let gid = match arg.valid & FATTR_GID {
                    0 => None,
                    _ => Some(arg.gid),
                };
                let size = match arg.valid & FATTR_SIZE {
                    0 => None,
                    _ => Some(arg.size),
                };
                let atime = match arg.valid & FATTR_ATIME {
                    0 => None,
                    _ => Some(UNIX_EPOCH + Duration::new(arg.atime, arg.atimensec)),
                };
                let mtime = match arg.valid & FATTR_MTIME {
                    0 => None,
                    _ => Some(UNIX_EPOCH + Duration::new(arg.mtime, arg.mtimensec)),
                };
                let fh = match arg.valid & FATTR_FH {
                    0 => None,
                    _ => Some(arg.fh),
                };
                #[cfg(target_os = "macos")]
                #[inline]
                fn get_macos_setattr(arg: &fuse_setattr_in) -> (Option<SystemTime>, Option<SystemTime>, Option<SystemTime>, Option<u32>) {
                    let crtime = match arg.valid & FATTR_CRTIME {
                        0 => None,
                        _ => Some(UNIX_EPOCH + Duration::new(arg.crtime, arg.crtimensec)),
                    };
                    let chgtime = match arg.valid & FATTR_CHGTIME {
                        0 => None,
                        _ => Some(UNIX_EPOCH + Duration::new(arg.chgtime, arg.chgtimensec)),
                    };
                    let bkuptime = match arg.valid & FATTR_BKUPTIME {
                        0 => None,
                        _ => Some(UNIX_EPOCH + Duration::new(arg.bkuptime, arg.bkuptimensec)),
                    };
                    let flags = match arg.valid & FATTR_FLAGS {
                        0 => None,
                        _ => Some(arg.flags),
                    };
                    (crtime, chgtime, bkuptime, flags)
                }
                #[cfg(not(target_os = "macos"))]
                #[inline]
                fn get_macos_setattr(_arg: &fuse_setattr_in) -> (Option<SystemTime>, Option<SystemTime>, Option<SystemTime>, Option<u32>) {
                    (None, None, None, None)
                }
                let (crtime, chgtime, bkuptime, flags) = get_macos_setattr(arg);
                filesystem.setattr(req, req.request.nodeid(), mode, uid, gid, size, atime, mtime, fh, crtime, chgtime, bkuptime, flags, req.reply()).await;
            }
            ll::Operation::ReadLink => {
                filesystem.readlink(req, req.request.nodeid(), req.reply()).await;
            }
            ll::Operation::MkNod { arg, name } => {
                filesystem.mknod(req, req.request.nodeid(), &name, arg.mode, arg.rdev, req.reply()).await;
            }
            ll::Operation::MkDir { arg, name } => {
                filesystem.mkdir(req, req.request.nodeid(), &name, arg.mode, req.reply()).await;
            }
            ll::Operation::Unlink { name } => {
                filesystem.unlink(req, req.request.nodeid(), &name, req.reply()).await;
            }
            ll::Operation::RmDir { name } => {
                filesystem.rmdir(req, req.request.nodeid(), &name, req.reply()).await;
            }
            ll::Operation::SymLink { name, link } => {
                filesystem.symlink(req, req.request.nodeid(), &name, &Path::new(link), req.reply()).await;
            }
            ll::Operation::Rename { arg, name, newname } => {
                filesystem.rename(req, req.request.nodeid(), &name, arg.newdir, &newname, req.reply()).await;
            }
            ll::Operation::Link { arg, name } => {
                filesystem.link(req, arg.oldnodeid, req.request.nodeid(), &name, req.reply()).await;
            }
            ll::Operation::Open { arg } => {
                filesystem.open(req, req.request.nodeid(), arg.flags, req.reply()).await;
            }
            ll::Operation::Read { arg } => {
                filesystem.read(req, req.request.nodeid(), arg.fh, arg.offset as i64, arg.size, req.reply()).await;
            }
            ll::Operation::Write { arg, data } => {
                assert!(data.len() == arg.size as usize);
                filesystem.write(req, req.request.nodeid(), arg.fh, arg.offset as i64, data, arg.write_flags, req.reply()).await;
            }
            ll::Operation::Flush { arg } => {
                filesystem.flush(req, req.request.nodeid(), arg.fh, arg.lock_owner, req.reply()).await;
            }
            ll::Operation::Release { arg } => {
                let flush = match arg.release_flags & FUSE_RELEASE_FLUSH {
                    0 => false,
                    _ => true,
                };
                filesystem.release(req, req.request.nodeid(), arg.fh, arg.flags, arg.lock_owner, flush, req.reply()).await;
            }
            ll::Operation::FSync { arg } => {
                let datasync = match arg.fsync_flags & 1 {
                    0 => false,
                    _ => true,
                };
                filesystem.fsync(req, req.request.nodeid(), arg.fh, datasync, req.reply()).await;
            }
            ll::Operation::OpenDir { arg } => {
                filesystem.opendir(req, req.request.nodeid(), arg.flags, req.reply()).await;
            }
            ll::Operation::ReadDir { arg } => {
                filesystem.readdir(req, req.request.nodeid(), arg.fh, arg.offset as i64, ReplyDirectory::new(req.request.unique(), req.ch, arg.size as usize)).await;
            }
            ll::Operation::ReleaseDir { arg } => {
                filesystem.releasedir(req, req.request.nodeid(), arg.fh, arg.flags, req.reply()).await;
            }
            ll::Operation::FSyncDir { arg } => {
                let datasync = match arg.fsync_flags & 1 {
                    0 => false,
                    _ => true,
                };
                filesystem.fsyncdir(req, req.request.nodeid(), arg.fh, datasync, req.reply()).await;
            }
            ll::Operation::StatFs => {
                filesystem.statfs(req, req.request.nodeid(), req.reply()).await;
            }
            ll::Operation::SetXAttr { arg, name, value } => {
                assert!(value.len() == arg.size as usize);
                #[cfg(target_os = "macos")]
                #[inline]
                fn get_position (arg: &fuse_setxattr_in) -> u32 { arg.position }
                #[cfg(not(target_os = "macos"))]
                #[inline]
                fn get_position (_arg: &fuse_setxattr_in) -> u32 { 0 }
                filesystem.setxattr(req, req.request.nodeid(), name, value, arg.flags, get_position(arg), req.reply()).await;
            }
            ll::Operation::GetXAttr { arg, name } => {
                filesystem.getxattr(req, req.request.nodeid(), name, arg.size, req.reply()).await;
            }
            ll::Operation::ListXAttr { arg } => {
                filesystem.listxattr(req, req.request.nodeid(), arg.size, req.reply()).await;
            }
            ll::Operation::RemoveXAttr { name } => {
                filesystem.removexattr(req, req.request.nodeid(), name, req.reply()).await;
            }
            ll::Operation::Access { arg } => {
                filesystem.access(req, req.request.nodeid(), arg.mask, req.reply()).await;
            }
            ll::Operation::Create { arg, name } => {
                filesystem.create(req, req.request.nodeid(), &name, arg.mode, arg.flags, req.reply()).await;
            }
            ll::Operation::GetLk { arg } => {
                filesystem.getlk(req, req.request.nodeid(), arg.fh, arg.owner, arg.lk.start, arg.lk.end, arg.lk.typ, arg.lk.pid, req.reply()).await;
            }
            ll::Operation::SetLk { arg } => {
                filesystem.setlk(req, req.request.nodeid(), arg.fh, arg.owner, arg.lk.start, arg.lk.end, arg.lk.typ, arg.lk.pid, false, req.reply()).await;
            }
            ll::Operation::SetLkW { arg } => {
                filesystem.setlk(req, req.request.nodeid(), arg.fh, arg.owner, arg.lk.start, arg.lk.end, arg.lk.typ, arg.lk.pid, true, req.reply()).await;
            }
            ll::Operation::BMap { arg } => {
                filesystem.bmap(req, req.request.nodeid(), arg.blocksize, arg.block, req.reply()).await;
            }

            #[cfg(target_os = "macos")]
            ll::Operation::SetVolName { name } => {
                filesystem.setvolname(req, name, req.reply()).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::GetXTimes => {
                filesystem.getxtimes(req, req.request.nodeid(), req.reply()).await;
            }
            #[cfg(target_os = "macos")]
            ll::Operation::Exchange { arg, oldname, newname } => {
                filesystem.exchange(req, arg.olddir, &oldname, arg.newdir, &newname, arg.options, req.reply()).await;
            }

            _ => {}
        }
    }

    /// Create a reply object for this request that can be passed to the filesystem
    /// implementation and makes sure that a request is replied exactly once
    fn reply<T: Reply>(&self) -> T {
        Reply::new(self.request.unique(), self.ch)
    }

    /// Returns the unique identifier of this request
    #[inline]
    #[allow(dead_code)]
    pub fn unique(&self) -> u64 {
        self.request.unique()
    }

    /// Returns the uid of this request
    #[inline]
    #[allow(dead_code)]
    pub fn uid(&self) -> u32 {
        self.request.uid()
    }

    /// Returns the gid of this request
    #[inline]
    #[allow(dead_code)]
    pub fn gid(&self) -> u32 {
        self.request.gid()
    }

    /// Returns the pid of this request
    #[inline]
    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.request.pid()
    }
}