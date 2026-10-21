use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use log::{debug, info, warn};

use crate::cdp::{CdpClient, CdpTarget};

const TTL: Duration = Duration::from_millis(500);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TabFileKind {
    Url,
    Title,
    Text,
    Html,
    Eval,
    Screenshot,
    Id,
    Reload,
    Activate,
    Close,
}

impl TabFileKind {
    pub fn name(&self) -> &'static str {
        match self {
            TabFileKind::Url => "url",
            TabFileKind::Title => "title",
            TabFileKind::Text => "text",
            TabFileKind::Html => "html",
            TabFileKind::Eval => "eval",
            TabFileKind::Screenshot => "screenshot.png",
            TabFileKind::Id => "id",
            TabFileKind::Reload => "reload",
            TabFileKind::Activate => "activate",
            TabFileKind::Close => "close",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "url" => Some(TabFileKind::Url),
            "title" => Some(TabFileKind::Title),
            "text" => Some(TabFileKind::Text),
            "html" => Some(TabFileKind::Html),
            "eval" => Some(TabFileKind::Eval),
            "screenshot.png" => Some(TabFileKind::Screenshot),
            "id" => Some(TabFileKind::Id),
            "reload" => Some(TabFileKind::Reload),
            "activate" => Some(TabFileKind::Activate),
            "close" => Some(TabFileKind::Close),
            _ => None,
        }
    }

    pub fn all() -> &'static [TabFileKind] {
        &[
            TabFileKind::Url,
            TabFileKind::Title,
            TabFileKind::Text,
            TabFileKind::Html,
            TabFileKind::Eval,
            TabFileKind::Screenshot,
            TabFileKind::Id,
            TabFileKind::Reload,
            TabFileKind::Activate,
            TabFileKind::Close,
        ]
    }

    pub fn is_writable(&self) -> bool {
        match self {
            TabFileKind::Url
            | TabFileKind::Title
            | TabFileKind::Html
            | TabFileKind::Eval
            | TabFileKind::Reload
            | TabFileKind::Activate
            | TabFileKind::Close => true,
            TabFileKind::Text | TabFileKind::Screenshot | TabFileKind::Id => false,
        }
    }

    pub fn mode(&self) -> u16 {
        if self.is_writable() { 0o644 } else { 0o444 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InodeKey {
    Root,
    TabDir(String), // target_id
    TabFile(String, TabFileKind),
}

#[derive(Clone, Debug)]
pub enum InodeEntry {
    Root,
    TabDir {
        target_id: String,
    },
    TabFile {
        target_id: String,
        kind: TabFileKind,
    },
}

struct OpenFile {
    target_id: String,
    kind: Option<TabFileKind>,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    is_write: bool,
}

pub struct CdpFs {
    client: CdpClient,
    start_time: SystemTime,
    uid: u32,
    gid: u32,

    // Inode allocation and lookup
    next_ino: AtomicU64,
    ino_to_entry: RwLock<HashMap<INodeNo, InodeEntry>>,
    key_to_ino: RwLock<HashMap<InodeKey, INodeNo>>,

    // Mapping between target_id and sanitized tab name
    target_names: RwLock<HashMap<String, String>>, // target_id -> sanitized_name
    name_to_target: RwLock<HashMap<String, String>>, // sanitized_name -> target_id

    // Evaluation results cache: target_id -> last eval output
    eval_results: RwLock<HashMap<String, String>>,

    // Open file handles
    next_fh: AtomicU64,
    open_files: RwLock<HashMap<FileHandle, OpenFile>>,
}

/// Sanitize tab title to make it safe for POSIX filename
pub fn sanitize_title(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|c| match c {
            '/' => '-',
            '\0' | '\r' | '\n' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else if trimmed.chars().count() > 180 {
        trimmed.chars().take(180).collect()
    } else {
        trimmed.to_string()
    }
}

impl CdpFs {
    pub fn new(client: CdpClient) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let start_time = SystemTime::now();

        let mut ino_to_entry = HashMap::new();
        let mut key_to_ino = HashMap::new();

        // Inode ROOT
        ino_to_entry.insert(INodeNo::ROOT, InodeEntry::Root);
        key_to_ino.insert(InodeKey::Root, INodeNo::ROOT);

        Self {
            client,
            start_time,
            uid,
            gid,
            next_ino: AtomicU64::new(2),
            ino_to_entry: RwLock::new(ino_to_entry),
            key_to_ino: RwLock::new(key_to_ino),
            target_names: RwLock::new(HashMap::new()),
            name_to_target: RwLock::new(HashMap::new()),
            eval_results: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            open_files: RwLock::new(HashMap::new()),
        }
    }

    fn get_or_create_ino(&self, key: InodeKey) -> INodeNo {
        {
            let key_map = self.key_to_ino.read().unwrap();
            if let Some(&ino) = key_map.get(&key) {
                return ino;
            }
        }

        let mut key_map = self.key_to_ino.write().unwrap();
        let mut ino_map = self.ino_to_entry.write().unwrap();

        if let Some(&ino) = key_map.get(&key) {
            return ino;
        }

        let raw_ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        let ino = INodeNo(raw_ino);
        let entry = match &key {
            InodeKey::Root => InodeEntry::Root,
            InodeKey::TabDir(id) => InodeEntry::TabDir {
                target_id: id.clone(),
            },
            InodeKey::TabFile(id, kind) => InodeEntry::TabFile {
                target_id: id.clone(),
                kind: *kind,
            },
        };

        key_map.insert(key, ino);
        ino_map.insert(ino, entry);
        ino
    }

    fn get_entry(&self, ino: INodeNo) -> Option<InodeEntry> {
        let ino_map = self.ino_to_entry.read().unwrap();
        ino_map.get(&ino).cloned()
    }

    /// Refresh tab list from Chrome and update sanitized name mappings
    fn refresh_tabs(&self) -> Vec<CdpTarget> {
        let tabs = match self.client.list_tabs() {
            Ok(t) => t,
            Err(e) => {
                debug!("Failed to list tabs from CDP: {}", e);
                return Vec::new();
            }
        };

        let mut name_counts: HashMap<String, usize> = HashMap::new();
        let mut new_target_names = HashMap::new();
        let mut new_name_to_target = HashMap::new();

        for tab in &tabs {
            let base_name = sanitize_title(&tab.title);
            let count = name_counts.entry(base_name.clone()).or_insert(0);
            *count += 1;

            let final_name = if *count == 1 {
                base_name
            } else {
                format!("{} ({})", base_name, *count)
            };

            // Pre-allocate inodes for tab and its files
            let _tab_ino = self.get_or_create_ino(InodeKey::TabDir(tab.id.clone()));
            for &kind in TabFileKind::all() {
                let _file_ino = self.get_or_create_ino(InodeKey::TabFile(tab.id.clone(), kind));
            }

            new_target_names.insert(tab.id.clone(), final_name.clone());
            new_name_to_target.insert(final_name, tab.id.clone());
        }

        {
            let mut tn = self.target_names.write().unwrap();
            *tn = new_target_names;
            let mut nt = self.name_to_target.write().unwrap();
            *nt = new_name_to_target;
        }

        tabs
    }

    fn make_attr(&self, ino: INodeNo, kind: FileType, size: u64, perm: u16) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: self.start_time,
            mtime: self.start_time,
            ctime: self.start_time,
            crtime: self.start_time,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

impl Filesystem for CdpFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        debug!("FUSE lookup: parent={:?}, name={:?}", parent, name);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if parent == INodeNo::ROOT {
            // Looking up a tab directory in root
            let target_id = {
                let map = self.name_to_target.read().unwrap();
                map.get(name_str).cloned()
            };

            let target_id = match target_id {
                Some(id) => id,
                None => {
                    // Refresh tabs from Chrome in case a new tab was opened
                    self.refresh_tabs();
                    let map = self.name_to_target.read().unwrap();
                    match map.get(name_str).cloned() {
                        Some(id) => id,
                        None => {
                            reply.error(Errno::ENOENT);
                            return;
                        }
                    }
                }
            };

            let ino = self.get_or_create_ino(InodeKey::TabDir(target_id));
            let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
            reply.entry(&TTL, &attr, Generation(0));
        } else {
            // Looking up a file inside a tab directory
            let parent_entry = match self.get_entry(parent) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

            if let InodeEntry::TabDir { target_id } = parent_entry {
                let kind = match TabFileKind::from_name(name_str) {
                    Some(k) => k,
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                };

                let default_size = match kind {
                    TabFileKind::Id => (target_id.len() + 1) as u64,
                    TabFileKind::Reload | TabFileKind::Activate | TabFileKind::Close => 0,
                    _ => 1024 * 1024, // 1MB virtual capacity so kernel VFS allows read
                };

                let ino = self.get_or_create_ino(InodeKey::TabFile(target_id, kind));
                let attr = self.make_attr(ino, FileType::RegularFile, default_size, kind.mode());
                reply.entry(&TTL, &attr, Generation(0));
            } else {
                reply.error(Errno::ENOTDIR);
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        debug!("FUSE getattr: ino={:?}, fh={:?}", ino, fh);
        if ino == INodeNo::ROOT {
            let attr = self.make_attr(INodeNo::ROOT, FileType::Directory, 0, 0o755);
            reply.attr(&TTL, &attr);
            return;
        }

        let entry = match self.get_entry(ino) {
            Some(e) => e,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match entry {
            InodeEntry::Root => {
                let attr = self.make_attr(INodeNo::ROOT, FileType::Directory, 0, 0o755);
                reply.attr(&TTL, &attr);
            }
            InodeEntry::TabDir { .. } => {
                let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
                reply.attr(&TTL, &attr);
            }
            InodeEntry::TabFile {
                ref target_id,
                kind,
            } => {
                // If the file is currently open in a file handle, report exact buffer size
                let size = if let Some(handle) = fh {
                    let files = self.open_files.read().unwrap();
                    files
                        .get(&handle)
                        .map(|f| f.read_buf.len() as u64)
                        .unwrap_or(0)
                } else {
                    match kind {
                        TabFileKind::Id => (target_id.len() + 1) as u64,
                        TabFileKind::Reload | TabFileKind::Activate | TabFileKind::Close => 0,
                        _ => 1024 * 1024,
                    }
                };

                let attr = self.make_attr(ino, FileType::RegularFile, size, kind.mode());
                reply.attr(&TTL, &attr);
            }
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let entry = match self.get_entry(ino) {
            Some(e) => e,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match entry {
            InodeEntry::Root | InodeEntry::TabDir { .. } => {
                let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
                reply.attr(&TTL, &attr);
            }
            InodeEntry::TabFile { kind, .. } => {
                if let (Some(handle), Some(sz)) = (fh, size) {
                    let mut files = self.open_files.write().unwrap();
                    if let Some(file) = files.get_mut(&handle) {
                        file.write_buf.truncate(sz as usize);
                    }
                }
                let attr =
                    self.make_attr(ino, FileType::RegularFile, size.unwrap_or(0), kind.mode());
                reply.attr(&TTL, &attr);
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino == INodeNo::ROOT {
            // Root directory lists tabs
            let tabs = self.refresh_tabs();

            let mut entries = Vec::new();
            entries.push((INodeNo::ROOT, FileType::Directory, ".".to_string()));
            entries.push((INodeNo::ROOT, FileType::Directory, "..".to_string()));

            let names_map = self.target_names.read().unwrap();
            for tab in tabs {
                if let Some(name) = names_map.get(&tab.id) {
                    let tab_ino = self.get_or_create_ino(InodeKey::TabDir(tab.id.clone()));
                    entries.push((tab_ino, FileType::Directory, name.clone()));
                }
            }

            for (i, (child_ino, kind, name)) in
                entries.into_iter().enumerate().skip(offset as usize)
            {
                if reply.add(child_ino, (i + 1) as u64, kind, name) {
                    break;
                }
            }
            reply.ok();
        } else {
            // Inside a tab directory
            let entry = match self.get_entry(ino) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

            if let InodeEntry::TabDir { target_id } = entry {
                let mut entries = Vec::new();
                entries.push((ino, FileType::Directory, ".".to_string()));
                entries.push((INodeNo::ROOT, FileType::Directory, "..".to_string()));

                for &kind in TabFileKind::all() {
                    let file_ino =
                        self.get_or_create_ino(InodeKey::TabFile(target_id.clone(), kind));
                    entries.push((file_ino, FileType::RegularFile, kind.name().to_string()));
                }

                for (i, (child_ino, kind, name)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(child_ino, (i + 1) as u64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            } else {
                reply.error(Errno::ENOTDIR);
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        debug!("FUSE open: ino={:?}, flags={:?}", ino, flags);
        let entry = match self.get_entry(ino) {
            Some(e) => e,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let (target_id, kind) = match entry {
            InodeEntry::TabFile { target_id, kind } => (target_id, kind),
            _ => {
                reply.error(Errno::EACCES);
                return;
            }
        };

        let acc_mode = flags.0 & libc::O_ACCMODE;
        let is_write = acc_mode == libc::O_WRONLY || acc_mode == libc::O_RDWR;
        let is_read = acc_mode == libc::O_RDONLY || acc_mode == libc::O_RDWR;

        if is_write && !kind.is_writable() {
            reply.error(Errno::EACCES);
            return;
        }

        let mut read_buf = Vec::new();

        if is_read {
            // Resolve WebSocket URL
            let ws_url = match self.client.resolve_ws_url(&target_id) {
                Ok(url) => url,
                Err(e) => {
                    debug!("resolve_ws_url error: {}", e);
                    reply.error(e.into_errno());
                    return;
                }
            };

            // Fetch file content based on kind
            let content_res = match kind {
                TabFileKind::Url => self
                    .client
                    .get_url(&ws_url)
                    .map(|u| format!("{}\n", u).into_bytes()),
                TabFileKind::Title => self
                    .client
                    .get_title(&ws_url)
                    .map(|t| format!("{}\n", t).into_bytes()),
                TabFileKind::Text => self
                    .client
                    .get_text(&ws_url)
                    .map(|t| format!("{}\n", t).into_bytes()),
                TabFileKind::Html => self
                    .client
                    .get_html(&ws_url)
                    .map(|h| format!("{}\n", h).into_bytes()),
                TabFileKind::Eval => {
                    let cached = self
                        .eval_results
                        .read()
                        .unwrap()
                        .get(&target_id)
                        .cloned()
                        .unwrap_or_default();
                    Ok(cached.into_bytes())
                }
                TabFileKind::Screenshot => self.client.capture_screenshot(&ws_url),
                TabFileKind::Id => Ok(format!("{}\n", target_id).into_bytes()),
                TabFileKind::Reload | TabFileKind::Activate | TabFileKind::Close => Ok(Vec::new()),
            };

            match content_res {
                Ok(bytes) => {
                    read_buf = bytes;
                }
                Err(e) => {
                    warn!("Error reading {:?} for tab {}: {}", kind, target_id, e);
                    reply.error(e.into_errno());
                    return;
                }
            }
        }

        let raw_fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        let fh = FileHandle(raw_fh);
        let open_file = OpenFile {
            target_id,
            kind: Some(kind),
            read_buf,
            write_buf: Vec::new(),
            is_write,
        };

        {
            let mut files = self.open_files.write().unwrap();
            files.insert(fh, open_file);
        }

        reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let files = self.open_files.read().unwrap();
        let file = match files.get(&fh) {
            Some(f) => f,
            None => {
                debug!("FUSE read: fh={:?} NOT FOUND", fh);
                reply.error(Errno::EBADF);
                return;
            }
        };
        debug!(
            "FUSE read: ino={:?}, fh={:?}, offset={}, size={}, data_len={}",
            _ino,
            fh,
            offset,
            size,
            file.read_buf.len()
        );

        if offset >= file.read_buf.len() as u64 {
            reply.data(&[]);
            return;
        }

        let start = offset as usize;
        let end = std::cmp::min(file.read_buf.len(), start + size as usize);
        reply.data(&file.read_buf[start..end]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut files = self.open_files.write().unwrap();
        let file = match files.get_mut(&fh) {
            Some(f) => f,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        file.write_buf.extend_from_slice(data);
        reply.written(data.len() as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        let (target_id, kind, write_buf, is_write) = {
            let files = self.open_files.read().unwrap();
            match files.get(&fh) {
                Some(f) => (f.target_id.clone(), f.kind, f.write_buf.clone(), f.is_write),
                None => {
                    reply.ok();
                    return;
                }
            }
        };

        if let (true, Some(k)) = (is_write && !write_buf.is_empty(), kind) {
            let text = String::from_utf8_lossy(&write_buf);
            let trimmed = text.trim();

            let ws_url = match self.client.resolve_ws_url(&target_id) {
                Ok(u) => u,
                Err(e) => {
                    debug!("flush resolve_ws_url error: {}", e);
                    reply.error(e.into_errno());
                    return;
                }
            };

            match k {
                TabFileKind::Url => {
                    if let Err(e) = self.client.navigate(&ws_url, trimmed) {
                        reply.error(e.into_errno());
                        return;
                    }
                }
                TabFileKind::Title => {
                    if let Err(e) = self.client.set_title(&ws_url, trimmed) {
                        reply.error(e.into_errno());
                        return;
                    }
                }
                TabFileKind::Html => {
                    if let Err(e) = self.client.set_html(&ws_url, &text) {
                        reply.error(e.into_errno());
                        return;
                    }
                }
                TabFileKind::Eval => match self.client.evaluate(&ws_url, trimmed) {
                    Ok(result) => {
                        let mut results = self.eval_results.write().unwrap();
                        results.insert(target_id, format!("{}\n", result));
                    }
                    Err(e) => {
                        reply.error(e.into_errno());
                        return;
                    }
                },
                TabFileKind::Reload => {
                    if let Err(e) = self.client.reload(&ws_url) {
                        reply.error(e.into_errno());
                        return;
                    }
                }
                TabFileKind::Activate => {
                    if let Err(e) = self.client.activate_target(&target_id) {
                        reply.error(e.into_errno());
                        return;
                    }
                }
                TabFileKind::Close => {
                    let _ = self.client.close_target(&target_id);
                }
                _ => {}
            }
        }

        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut files = self.open_files.write().unwrap();
        files.remove(&fh);
        reply.ok();
    }

    /// Handle rmdir: closes the tab when someone does `rmdir /mountpoint/tab-name`
    /// or `rm -rf /mountpoint/tab-name`
    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if parent != INodeNo::ROOT {
            reply.error(Errno::ENOTDIR);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let target_id = {
            let map = self.name_to_target.read().unwrap();
            map.get(name_str).cloned()
        };

        let target_id = match target_id {
            Some(id) => id,
            None => {
                self.refresh_tabs();
                let map = self.name_to_target.read().unwrap();
                match map.get(name_str).cloned() {
                    Some(id) => id,
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                }
            }
        };

        info!("Closing tab {} ('{}') via rmdir", target_id, name_str);
        if let Err(e) = self.client.close_target(&target_id) {
            warn!("Failed to close tab {}: {}", target_id, e);
        }

        // Clean up mappings
        {
            let mut tn = self.target_names.write().unwrap();
            tn.remove(&target_id);
            let mut nt = self.name_to_target.write().unwrap();
            nt.remove(name_str);
        }

        reply.ok();
    }

    /// Handle unlink: allows unlinking files inside a tab directory.
    /// This is crucial for `rm -rf /mountpoint/tab-name` to succeed, because
    /// `rm -rf` first unlinks all files inside the directory before calling rmdir!
    fn unlink(&self, _req: &Request, parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        let parent_entry = match self.get_entry(parent) {
            Some(e) => e,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match parent_entry {
            InodeEntry::TabDir { .. } => {
                // Virtual file deletion succeeds to allow rm -rf to proceed to rmdir
                reply.ok();
            }
            _ => {
                reply.error(Errno::EPERM);
            }
        }
    }

    /// Handle mkdir: allows creating a new tab!
    /// E.g. `mkdir /mountpoint/NewTab` opens a new tab.
    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if parent != INodeNo::ROOT {
            reply.error(Errno::EPERM);
            return;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let initial_url = if name_str.starts_with("http://") || name_str.starts_with("https://") {
            name_str
        } else {
            "about:blank"
        };

        match self.client.new_tab(initial_url) {
            Ok(new_tab) => {
                let ino = self.get_or_create_ino(InodeKey::TabDir(new_tab.id.clone()));
                let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
                self.refresh_tabs();
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(e) => {
                warn!("Failed to create new tab via mkdir: {}", e);
                reply.error(e.into_errno());
            }
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(
            1024 * 1024, // blocks
            1024 * 1024, // bfree
            1024 * 1024, // bavail
            1000,        // files
            1000,        // ffree
            512,         // bsize
            255,         // namelen
            512,         // frsize
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_title() {
        assert_eq!(sanitize_title("Normal Title"), "Normal Title");
        assert_eq!(
            sanitize_title("GitHub - user/repo: project"),
            "GitHub - user-repo: project"
        );
        assert_eq!(
            sanitize_title("Title\nwith\rnewlines\tand null\0byte"),
            "Title with newlines and null byte"
        );
        assert_eq!(sanitize_title("   trimmed   "), "trimmed");
        assert_eq!(sanitize_title(""), "Untitled");
        assert_eq!(sanitize_title("   ///   "), "---");
    }

    #[test]
    fn test_tab_file_kinds() {
        for &kind in TabFileKind::all() {
            let name = kind.name();
            let parsed = TabFileKind::from_name(name);
            assert_eq!(parsed, Some(kind));
        }
        assert_eq!(TabFileKind::from_name("nonexistent"), None);
    }
}
