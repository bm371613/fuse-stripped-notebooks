use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use log::debug;

const TTL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum Mode {
    #[value(name = "strip-outputs")]
    StripOutputs,
    #[value(name = "python-script")]
    PythonScript,
}

pub struct NotebookFs {
    source: PathBuf,
    mode: Mode,
    inner: Mutex<FsState>,
}

struct CachedContent {
    data: Arc<Vec<u8>>,
    mtime_sec: i64,
    mtime_nsec: i64,
}

struct FsState {
    next_ino: u64,
    inodes: HashMap<u64, InodeEntry>,
    path_index: HashMap<PathBuf, u64>,
    content_cache: HashMap<u64, CachedContent>,
    open_files: HashMap<u64, File>,
    next_fh: u64,
}

struct InodeEntry {
    real_path: PathBuf,
    kind: FileType,
}

impl NotebookFs {
    pub fn new(source: PathBuf, mode: Mode) -> Self {
        let mut inodes = HashMap::new();
        let mut path_index = HashMap::new();
        inodes.insert(
            INodeNo::ROOT.0,
            InodeEntry {
                real_path: source.clone(),
                kind: FileType::Directory,
            },
        );
        path_index.insert(source.clone(), INodeNo::ROOT.0);
        NotebookFs {
            source,
            mode,
            inner: Mutex::new(FsState {
                next_ino: 2,
                inodes,
                path_index,
                content_cache: HashMap::new(),
                open_files: HashMap::new(),
                next_fh: 1,
            }),
        }
    }
}

fn assign_or_lookup_ino(state: &mut FsState, real_path: PathBuf) -> Option<(u64, FileType)> {
    if let Some(&ino) = state.path_index.get(&real_path) {
        let kind = state.inodes[&ino].kind;
        return Some((ino, kind));
    }
    let meta = fs::symlink_metadata(&real_path).ok()?;
    let kind = FileType::from_std(meta.file_type())?;
    let ino = state.next_ino;
    state.next_ino += 1;
    state.inodes.insert(
        ino,
        InodeEntry {
            real_path: real_path.clone(),
            kind,
        },
    );
    state.path_index.insert(real_path, ino);
    Some((ino, kind))
}

fn is_notebook(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "ipynb")
}

fn get_or_transform(
    state: &mut FsState,
    ino: u64,
    path: &Path,
    mode: &Mode,
) -> Result<Arc<Vec<u8>>, io::Error> {
    let meta = fs::symlink_metadata(path)?;
    let mtime_sec = meta.mtime();
    let mtime_nsec = meta.mtime_nsec();
    if let Some(cached) = state.content_cache.get(&ino) {
        if cached.mtime_sec == mtime_sec && cached.mtime_nsec == mtime_nsec {
            return Ok(Arc::clone(&cached.data));
        }
        state.content_cache.remove(&ino);
    }
    let raw = fs::read(path)?;
    let transformed = match mode {
        Mode::StripOutputs => crate::transform::strip_outputs(&raw)?,
        Mode::PythonScript => crate::transform::to_python_script(&raw)?,
    };
    let arc = Arc::new(transformed);
    state.content_cache.insert(
        ino,
        CachedContent {
            data: Arc::clone(&arc),
            mtime_sec,
            mtime_nsec,
        },
    );
    Ok(arc)
}

fn to_system_time(secs: i64, nsecs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32)
    } else {
        UNIX_EPOCH
    }
}

fn metadata_to_fileattr(ino: u64, meta: &std::fs::Metadata, size: u64) -> FileAttr {
    let atime = to_system_time(meta.atime(), meta.atime_nsec());
    let mtime = to_system_time(meta.mtime(), meta.mtime_nsec());
    let ctime = to_system_time(meta.ctime(), meta.ctime_nsec());
    let kind = FileType::from_std(meta.file_type()).unwrap_or(FileType::RegularFile);
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: meta.blocks(),
        atime,
        mtime,
        ctime,
        crtime: ctime,
        kind,
        perm: (meta.mode() as u16) & 0o555,
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        flags: 0,
    }
}

use std::io;

impl Filesystem for NotebookFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut state = self.inner.lock().unwrap();
        let parent_path = match state.inodes.get(&parent.0) {
            Some(e) => e.real_path.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let child_path = parent_path.join(name);
        let meta = match fs::symlink_metadata(&child_path) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let (ino, _kind) = match assign_or_lookup_ino(&mut state, child_path.clone()) {
            Some(x) => x,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let size = if is_notebook(&child_path) {
            match get_or_transform(&mut state, ino, &child_path, &self.mode) {
                Ok(data) => data.len() as u64,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }
        } else {
            meta.len()
        };
        let attr = metadata_to_fileattr(ino, &meta, size);
        debug!("lookup {:?} → ino={} size={}", child_path, ino, size);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let mut state = self.inner.lock().unwrap();
        let path = match state.inodes.get(&ino.0) {
            Some(e) => e.real_path.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let size = if is_notebook(&path) {
            match get_or_transform(&mut state, ino.0, &path, &self.mode) {
                Ok(data) => data.len() as u64,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }
        } else {
            meta.len()
        };
        let attr = metadata_to_fileattr(ino.0, &meta, size);
        reply.attr(&TTL, &attr);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut state = self.inner.lock().unwrap();
        let dir_path = match state.inodes.get(&ino.0) {
            Some(e) => e.real_path.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let parent_ino = if ino == INodeNo::ROOT {
            INodeNo::ROOT.0
        } else if let Some(parent_path) = dir_path.parent() {
            state
                .path_index
                .get(parent_path)
                .copied()
                .unwrap_or(INodeNo::ROOT.0)
        } else {
            INodeNo::ROOT.0
        };

        let mut entries: Vec<(u64, FileType, std::ffi::OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];

        if let Ok(read_dir) = fs::read_dir(&dir_path) {
            let children: Vec<_> = read_dir.flatten().collect();
            for entry in children {
                let child_path = entry.path();
                if let Some((child_ino, child_kind)) = assign_or_lookup_ino(&mut state, child_path)
                {
                    entries.push((child_ino, child_kind, entry.file_name()));
                }
            }
        }

        for (i, (child_ino, kind, name)) in entries.iter().enumerate() {
            if (i as u64) < offset {
                continue;
            }
            if reply.add(INodeNo(*child_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        let mut state = self.inner.lock().unwrap();
        let path = match state.inodes.get(&ino.0) {
            Some(e) => e.real_path.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if is_notebook(&path) {
            match get_or_transform(&mut state, ino.0, &path, &self.mode) {
                Ok(_) => {}
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }
            reply.opened(FileHandle(0), FopenFlags::empty());
        } else {
            drop(state);
            match File::open(&path) {
                Ok(file) => {
                    let mut state = self.inner.lock().unwrap();
                    let fh = state.next_fh;
                    state.next_fh += 1;
                    state.open_files.insert(fh, file);
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(e) => {
                    reply.error(Errno::from(e));
                }
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut state = self.inner.lock().unwrap();
        let path = match state.inodes.get(&ino.0) {
            Some(e) => e.real_path.clone(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if is_notebook(&path) {
            match get_or_transform(&mut state, ino.0, &path, &self.mode) {
                Ok(data) => {
                    let start = (offset as usize).min(data.len());
                    let end = (offset as usize + size as usize).min(data.len());
                    reply.data(&data[start..end]);
                }
                Err(_) => {
                    reply.error(Errno::EIO);
                }
            }
        } else {
            let file = match state.open_files.get_mut(&fh.0) {
                Some(f) => f,
                None => {
                    reply.error(Errno::EBADF);
                    return;
                }
            };
            if file.seek(SeekFrom::Start(offset)).is_err() {
                reply.error(Errno::EIO);
                return;
            }
            let mut buf = vec![0u8; size as usize];
            let mut total = 0;
            loop {
                match file.read(&mut buf[total..]) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
            buf.truncate(total);
            reply.data(&buf);
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.inner.lock().unwrap().open_files.remove(&fh.0);
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = {
            let state = self.inner.lock().unwrap();
            match state.inodes.get(&ino.0) {
                Some(e) => e.real_path.clone(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        match fs::read_link(&path) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> io::Result<()> {
        debug!("init: source={:?} mode={:?}", self.source, self.mode);
        Ok(())
    }
}
