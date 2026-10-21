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

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AssetCategory {
    Images,
    Videos,
    Audio,
    Other,
}

impl AssetCategory {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Images => "images",
            Self::Videos => "videos",
            Self::Audio => "audio",
            Self::Other => "other",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "images" => Some(Self::Images),
            "videos" => Some(Self::Videos),
            "audio" => Some(Self::Audio),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    pub fn all() -> &'static [AssetCategory] {
        &[Self::Images, Self::Videos, Self::Audio, Self::Other]
    }
}

#[derive(Clone, Debug)]
pub struct TabAsset {
    pub url: String,
    pub frame_id: Option<String>,
    pub filename: String,
    pub category: AssetCategory,
    pub mime_type: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug)]
struct TabAssetsCache {
    fetched_at: std::time::Instant,
    url: String,
    assets: Vec<TabAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InodeKey {
    Root,
    TabDir(String), // target_id
    TabFile(String, TabFileKind),
    AssetsDir(String),
    AssetCategoryDir(String, AssetCategory),
    AssetFile(String, AssetCategory, String),
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
    AssetsDir {
        target_id: String,
    },
    AssetCategoryDir {
        target_id: String,
        category: AssetCategory,
    },
    AssetFile {
        target_id: String,
        category: AssetCategory,
        filename: String,
    },
}

struct OpenFile {
    target_id: String,
    kind: Option<TabFileKind>,
    _asset: Option<(AssetCategory, String)>,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    is_write: bool,
    executed: bool,
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

    // Asset cache: target_id -> TabAssetsCache
    tab_assets: RwLock<HashMap<String, TabAssetsCache>>,

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
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.trim_matches('.').trim().is_empty()
    {
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
            tab_assets: RwLock::new(HashMap::new()),
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
            InodeKey::AssetsDir(id) => InodeEntry::AssetsDir {
                target_id: id.clone(),
            },
            InodeKey::AssetCategoryDir(id, cat) => InodeEntry::AssetCategoryDir {
                target_id: id.clone(),
                category: *cat,
            },
            InodeKey::AssetFile(id, cat, name) => InodeEntry::AssetFile {
                target_id: id.clone(),
                category: *cat,
                filename: name.clone(),
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
            let _assets_ino = self.get_or_create_ino(InodeKey::AssetsDir(tab.id.clone()));
            for &cat in AssetCategory::all() {
                let _cat_ino =
                    self.get_or_create_ino(InodeKey::AssetCategoryDir(tab.id.clone(), cat));
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

    pub fn get_or_discover_assets(&self, target_id: &str) -> Vec<TabAsset> {
        let ws_url = match self.client.resolve_ws_url(target_id) {
            Ok(u) => u,
            Err(e) => {
                debug!("Failed to resolve ws_url for tab {}: {}", target_id, e);
                return Vec::new();
            }
        };

        let current_url = self.client.get_url(&ws_url).unwrap_or_default();

        {
            let cache_map = self.tab_assets.read().unwrap();
            if let Some(cached) = cache_map.get(target_id)
                && cached.url == current_url
                && cached.fetched_at.elapsed() < Duration::from_secs(4)
            {
                return cached.assets.clone();
            }
        }

        struct RawAssetItem {
            url: String,
            frame_id: Option<String>,
            type_str: String,
            mime: String,
            size: Option<u64>,
        }

        let mut raw_items: Vec<RawAssetItem> = Vec::new();
        let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. From CDP get_resource_tree
        if let Ok(tree) = self.client.get_resource_tree(&ws_url) {
            fn collect_resources(
                frame_tree: &crate::cdp::CdpFrameTree,
                out: &mut Vec<RawAssetItem>,
                seen: &mut std::collections::HashSet<String>,
            ) {
                let frame_id = &frame_tree.frame.id;
                for res in &frame_tree.resources {
                    if !res.url.is_empty()
                        && !res.url.starts_with("about:")
                        && !seen.contains(&res.url)
                    {
                        seen.insert(res.url.clone());
                        out.push(RawAssetItem {
                            url: res.url.clone(),
                            frame_id: Some(frame_id.clone()),
                            type_str: res.resource_type.clone(),
                            mime: res.mime_type.clone(),
                            size: res.content_size,
                        });
                    }
                }
                for child in &frame_tree.child_frames {
                    collect_resources(child, out, seen);
                }
            }
            collect_resources(&tree.frame_tree, &mut raw_items, &mut seen_urls);
        }

        // 2. From DOM media query
        if let Ok(dom_items) = self.client.get_dom_media(&ws_url) {
            for item in dom_items {
                if !item.url.is_empty()
                    && !item.url.starts_with("about:")
                    && !seen_urls.contains(&item.url)
                {
                    seen_urls.insert(item.url.clone());
                    raw_items.push(RawAssetItem {
                        url: item.url,
                        frame_id: None,
                        type_str: item.media_type,
                        mime: String::new(),
                        size: None,
                    });
                }
            }
        }

        // 3. Categorize and deduplicate filenames per category
        let mut name_counts: HashMap<(AssetCategory, String), usize> = HashMap::new();
        let mut assets = Vec::new();

        for (index, item) in raw_items.into_iter().enumerate() {
            let category = categorize_asset(&item.type_str, &item.mime, &item.url);
            let base_filename = derive_asset_filename(&item.url, &item.mime, index + 1, category);

            let count = name_counts
                .entry((category, base_filename.clone()))
                .or_insert(0);
            *count += 1;
            let final_filename = if *count == 1 {
                base_filename
            } else if let Some((stem, ext)) = base_filename.rsplit_once('.') {
                format!("{} ({}).{}", stem, *count, ext)
            } else {
                format!("{} ({})", base_filename, *count)
            };

            let _ = self.get_or_create_ino(InodeKey::AssetFile(
                target_id.to_string(),
                category,
                final_filename.clone(),
            ));

            assets.push(TabAsset {
                url: item.url,
                frame_id: item.frame_id,
                filename: final_filename,
                category,
                mime_type: item.mime,
                size: item.size,
            });
        }

        {
            let mut cache_map = self.tab_assets.write().unwrap();
            cache_map.insert(
                target_id.to_string(),
                TabAssetsCache {
                    fetched_at: std::time::Instant::now(),
                    url: current_url,
                    assets: assets.clone(),
                },
            );
        }

        assets
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

pub fn categorize_asset(type_str: &str, mime: &str, url: &str) -> AssetCategory {
    let embedded_mime = if let Some(rest) = url.strip_prefix("data:") {
        rest.split(',')
            .next()
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
    } else {
        ""
    };
    let effective_mime = if !embedded_mime.is_empty() {
        embedded_mime
    } else {
        mime
    };

    let lower_type = type_str.to_lowercase();
    let lower_mime = effective_mime.to_lowercase();
    let lower_url = url.split('?').next().unwrap_or(url).to_lowercase();

    if lower_type == "image"
        || lower_mime.starts_with("image/")
        || lower_url.ends_with(".png")
        || lower_url.ends_with(".jpg")
        || lower_url.ends_with(".jpeg")
        || lower_url.ends_with(".gif")
        || lower_url.ends_with(".webp")
        || lower_url.ends_with(".svg")
        || lower_url.ends_with(".bmp")
        || lower_url.ends_with(".ico")
        || lower_url.ends_with(".avif")
        || lower_url.ends_with(".tiff")
    {
        return AssetCategory::Images;
    }

    if lower_mime.starts_with("video/")
        || lower_url.ends_with(".mp4")
        || lower_url.ends_with(".webm")
        || lower_url.ends_with(".mkv")
        || lower_url.ends_with(".mov")
        || lower_url.ends_with(".avi")
        || lower_url.ends_with(".m4v")
        || lower_url.ends_with(".ogv")
        || (lower_type == "media" && (lower_url.contains("video") || lower_mime.contains("video")))
        || lower_type == "video"
    {
        return AssetCategory::Videos;
    }

    if lower_mime.starts_with("audio/")
        || lower_url.ends_with(".mp3")
        || lower_url.ends_with(".wav")
        || lower_url.ends_with(".ogg")
        || lower_url.ends_with(".m4a")
        || lower_url.ends_with(".flac")
        || lower_url.ends_with(".aac")
        || lower_url.ends_with(".opus")
        || lower_url.ends_with(".wma")
        || lower_url.ends_with(".oga")
        || (lower_type == "media" && (lower_url.contains("audio") || lower_mime.contains("audio")))
        || lower_type == "audio"
    {
        return AssetCategory::Audio;
    }

    AssetCategory::Other
}

pub fn derive_asset_filename(
    url: &str,
    mime: &str,
    fallback_index: usize,
    category: AssetCategory,
) -> String {
    if let Some(rest) = url.strip_prefix("data:") {
        let header = rest.split(',').next().unwrap_or("");
        let embedded_mime = header.split(';').next().unwrap_or("").trim();
        let effective_mime = if !embedded_mime.is_empty() {
            embedded_mime
        } else {
            mime
        };
        let ext = extension_from_mime(effective_mime).unwrap_or(match category {
            AssetCategory::Images => "png",
            AssetCategory::Videos => "mp4",
            AssetCategory::Audio => "mp3",
            AssetCategory::Other => "bin",
        });
        return format!("data_{}_{}.{}", category.name(), fallback_index, ext);
    }

    let path_part = url.split('#').next().unwrap_or(url);
    let path_part = path_part.split('?').next().unwrap_or(path_part);

    let raw_name = path_part.rsplit('/').next().unwrap_or("");
    let decoded = urlencoding_decode(raw_name);
    let sanitized = sanitize_filename(&decoded);

    let has_ext = sanitized.contains('.') && !sanitized.ends_with('.');
    if !has_ext {
        if let Some(ext) = extension_from_mime(mime) {
            if sanitized.is_empty() {
                format!("{}_{}.{}", category.name(), fallback_index, ext)
            } else {
                format!("{}.{}", sanitized, ext)
            }
        } else if sanitized.is_empty() {
            format!("{}_{}", category.name(), fallback_index)
        } else {
            sanitized
        }
    } else if sanitized.is_empty() {
        format!("{}_{}", category.name(), fallback_index)
    } else {
        sanitized
    }
}

fn extension_from_mime(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or(mime).trim().to_lowercase();
    match base.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some("ico"),
        "image/bmp" => Some("bmp"),
        "image/avif" => Some("avif"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "video/ogg" => Some("ogv"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/aac" => Some("aac"),
        "text/css" => Some("css"),
        "text/javascript" | "application/javascript" => Some("js"),
        "font/woff2" => Some("woff2"),
        "font/woff" => Some("woff"),
        "font/ttf" | "application/font-sfnt" => Some("ttf"),
        "application/json" => Some("json"),
        "application/pdf" => Some("pdf"),
        "text/html" => Some("html"),
        _ => None,
    }
}

fn urlencoding_decode(s: &str) -> String {
    let mut bytes = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%'
            && let (Some(h1), Some(h2)) = (chars.next(), chars.next())
            && let Ok(byte) = u8::from_str_radix(&format!("{}{}", h1, h2), 16)
        {
            bytes.push(byte);
        } else {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn sanitize_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | '\r' | '\n' | '\t' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = s.trim().trim_matches('.');
    if trimmed.is_empty() {
        "file".to_string()
    } else if trimmed.chars().count() > 180 {
        trimmed.chars().take(180).collect()
    } else {
        trimmed.to_string()
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
            let parent_entry = match self.get_entry(parent) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

            match parent_entry {
                InodeEntry::TabDir { target_id } => {
                    if name_str == "assets" {
                        let ino = self.get_or_create_ino(InodeKey::AssetsDir(target_id));
                        let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
                        reply.entry(&TTL, &attr, Generation(0));
                        return;
                    }

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
                    let attr =
                        self.make_attr(ino, FileType::RegularFile, default_size, kind.mode());
                    reply.entry(&TTL, &attr, Generation(0));
                }
                InodeEntry::AssetsDir { target_id } => {
                    let cat = match AssetCategory::from_name(name_str) {
                        Some(c) => c,
                        None => {
                            reply.error(Errno::ENOENT);
                            return;
                        }
                    };
                    let ino = self.get_or_create_ino(InodeKey::AssetCategoryDir(target_id, cat));
                    let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                InodeEntry::AssetCategoryDir {
                    target_id,
                    category,
                } => {
                    let assets = self.get_or_discover_assets(&target_id);
                    if let Some(asset) = assets
                        .into_iter()
                        .find(|a| a.category == category && a.filename == name_str)
                    {
                        let ino = self.get_or_create_ino(InodeKey::AssetFile(
                            target_id,
                            category,
                            name_str.to_string(),
                        ));
                        let size = asset.size.unwrap_or(256 * 1024 * 1024);
                        let attr = self.make_attr(ino, FileType::RegularFile, size, 0o444);
                        reply.entry(&TTL, &attr, Generation(0));
                    } else {
                        reply.error(Errno::ENOENT);
                    }
                }
                _ => {
                    reply.error(Errno::ENOTDIR);
                }
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
            InodeEntry::Root
            | InodeEntry::TabDir { .. }
            | InodeEntry::AssetsDir { .. }
            | InodeEntry::AssetCategoryDir { .. } => {
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
            InodeEntry::AssetFile {
                ref target_id,
                category,
                ref filename,
            } => {
                let size = if let Some(handle) = fh {
                    let files = self.open_files.read().unwrap();
                    files
                        .get(&handle)
                        .map(|f| f.read_buf.len() as u64)
                        .unwrap_or(0)
                } else {
                    let cached_size = {
                        let cache_map = self.tab_assets.read().unwrap();
                        cache_map.get(target_id).and_then(|c| {
                            c.assets
                                .iter()
                                .find(|a| a.category == category && a.filename == *filename)
                                .and_then(|a| a.size)
                        })
                    };
                    cached_size.unwrap_or(256 * 1024 * 1024)
                };

                let attr = self.make_attr(ino, FileType::RegularFile, size, 0o444);
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
            InodeEntry::Root
            | InodeEntry::TabDir { .. }
            | InodeEntry::AssetsDir { .. }
            | InodeEntry::AssetCategoryDir { .. } => {
                let attr = self.make_attr(ino, FileType::Directory, 0, 0o755);
                reply.attr(&TTL, &attr);
            }
            InodeEntry::TabFile {
                ref target_id,
                kind,
            } => {
                if let (Some(handle), Some(sz)) = (fh, size) {
                    let mut files = self.open_files.write().unwrap();
                    if let Some(file) = files.get_mut(&handle) {
                        file.write_buf.truncate(sz as usize);
                    }
                }
                // Handle touch on control files when utimensat/setattr updates timestamps
                if (_atime.is_some() || _mtime.is_some())
                    && matches!(
                        kind,
                        TabFileKind::Reload | TabFileKind::Activate | TabFileKind::Close
                    )
                {
                    match kind {
                        TabFileKind::Reload => {
                            if let Ok(ws_url) = self.client.resolve_ws_url(target_id) {
                                let _ = self.client.reload(&ws_url);
                            }
                        }
                        TabFileKind::Activate => {
                            let _ = self.client.activate_target(target_id);
                        }
                        TabFileKind::Close => {
                            let _ = self.client.close_target(target_id);
                        }
                        _ => {}
                    }
                }
                let attr =
                    self.make_attr(ino, FileType::RegularFile, size.unwrap_or(0), kind.mode());
                reply.attr(&TTL, &attr);
            }
            InodeEntry::AssetFile { .. } => {
                let attr = self.make_attr(ino, FileType::RegularFile, size.unwrap_or(0), 0o444);
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
            let entry = match self.get_entry(ino) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

            match entry {
                InodeEntry::TabDir { target_id } => {
                    let mut entries = Vec::new();
                    entries.push((ino, FileType::Directory, ".".to_string()));
                    entries.push((INodeNo::ROOT, FileType::Directory, "..".to_string()));

                    let assets_ino = self.get_or_create_ino(InodeKey::AssetsDir(target_id.clone()));
                    entries.push((assets_ino, FileType::Directory, "assets".to_string()));

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
                }
                InodeEntry::AssetsDir { target_id } => {
                    let mut entries = Vec::new();
                    entries.push((ino, FileType::Directory, ".".to_string()));
                    let tab_ino = self.get_or_create_ino(InodeKey::TabDir(target_id.clone()));
                    entries.push((tab_ino, FileType::Directory, "..".to_string()));

                    for &cat in AssetCategory::all() {
                        let cat_ino = self
                            .get_or_create_ino(InodeKey::AssetCategoryDir(target_id.clone(), cat));
                        entries.push((cat_ino, FileType::Directory, cat.name().to_string()));
                    }

                    for (i, (child_ino, kind, name)) in
                        entries.into_iter().enumerate().skip(offset as usize)
                    {
                        if reply.add(child_ino, (i + 1) as u64, kind, name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                InodeEntry::AssetCategoryDir {
                    target_id,
                    category,
                } => {
                    let mut entries = Vec::new();
                    entries.push((ino, FileType::Directory, ".".to_string()));
                    let assets_ino = self.get_or_create_ino(InodeKey::AssetsDir(target_id.clone()));
                    entries.push((assets_ino, FileType::Directory, "..".to_string()));

                    let assets = self.get_or_discover_assets(&target_id);
                    for asset in assets {
                        if asset.category == category {
                            let file_ino = self.get_or_create_ino(InodeKey::AssetFile(
                                target_id.clone(),
                                category,
                                asset.filename.clone(),
                            ));
                            entries.push((file_ino, FileType::RegularFile, asset.filename));
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
                }
                _ => {
                    reply.error(Errno::ENOTDIR);
                }
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

        let acc_mode = flags.0 & libc::O_ACCMODE;
        let is_write = acc_mode == libc::O_WRONLY || acc_mode == libc::O_RDWR;
        let is_read = acc_mode == libc::O_RDONLY || acc_mode == libc::O_RDWR;

        match entry {
            InodeEntry::TabFile { target_id, kind } => {
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
                        TabFileKind::Reload | TabFileKind::Activate | TabFileKind::Close => {
                            Ok(Vec::new())
                        }
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
                    _asset: None,
                    read_buf,
                    write_buf: Vec::new(),
                    is_write,
                    executed: false,
                };

                {
                    let mut files = self.open_files.write().unwrap();
                    files.insert(fh, open_file);
                }

                reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
            }
            InodeEntry::AssetFile {
                target_id,
                category,
                filename,
            } => {
                if is_write {
                    reply.error(Errno::EACCES);
                    return;
                }

                let mut read_buf = Vec::new();

                if is_read {
                    let assets = self.get_or_discover_assets(&target_id);
                    let asset = match assets
                        .into_iter()
                        .find(|a| a.category == category && a.filename == filename)
                    {
                        Some(a) => a,
                        None => {
                            reply.error(Errno::ENOENT);
                            return;
                        }
                    };

                    let ws_url = match self.client.resolve_ws_url(&target_id) {
                        Ok(url) => url,
                        Err(e) => {
                            debug!("resolve_ws_url error: {}", e);
                            reply.error(e.into_errno());
                            return;
                        }
                    };

                    match self.client.get_asset_content(
                        &ws_url,
                        asset.frame_id.as_deref(),
                        &asset.url,
                    ) {
                        Ok(bytes) => {
                            let len = bytes.len() as u64;
                            read_buf = bytes;
                            if let Ok(mut cache_map) = self.tab_assets.write()
                                && let Some(cache) = cache_map.get_mut(&target_id)
                                && let Some(entry) = cache
                                    .assets
                                    .iter_mut()
                                    .find(|a| a.category == category && a.filename == filename)
                            {
                                entry.size = Some(len);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Error reading asset {} ({}) for tab {}: {}",
                                filename, asset.url, target_id, e
                            );
                            reply.error(e.into_errno());
                            return;
                        }
                    }
                }

                let raw_fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
                let fh = FileHandle(raw_fh);
                let open_file = OpenFile {
                    target_id,
                    kind: None,
                    _asset: Some((category, filename)),
                    read_buf,
                    write_buf: Vec::new(),
                    is_write: false,
                    executed: false,
                };

                {
                    let mut files = self.open_files.write().unwrap();
                    files.insert(fh, open_file);
                }

                reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
            }
            _ => {
                reply.error(Errno::EACCES);
            }
        }
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
        offset: u64,
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

        let start = offset as usize;
        let end = start + data.len();
        if file.write_buf.len() < end {
            file.write_buf.resize(end, 0);
        }
        file.write_buf[start..end].copy_from_slice(data);
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
        let (target_id, kind, write_buf, should_execute) = {
            let mut files = self.open_files.write().unwrap();
            match files.get_mut(&fh) {
                Some(f) => {
                    let is_control = matches!(
                        f.kind,
                        Some(TabFileKind::Reload | TabFileKind::Activate | TabFileKind::Close)
                    );
                    let should_execute =
                        f.is_write && (!f.write_buf.is_empty() || is_control) && !f.executed;
                    f.executed = true;
                    let buf = std::mem::take(&mut f.write_buf);
                    (f.target_id.clone(), f.kind, buf, should_execute)
                }
                None => {
                    reply.ok();
                    return;
                }
            }
        };

        if should_execute && let Some(k) = kind {
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
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if parent == INodeNo::ROOT {
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
        } else {
            let parent_entry = match self.get_entry(parent) {
                Some(e) => e,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

            match parent_entry {
                InodeEntry::TabDir { .. } => {
                    if name_str == "assets" {
                        reply.ok();
                    } else {
                        reply.error(Errno::ENOENT);
                    }
                }
                InodeEntry::AssetsDir { .. } => {
                    if AssetCategory::from_name(name_str).is_some() {
                        reply.ok();
                    } else {
                        reply.error(Errno::ENOENT);
                    }
                }
                _ => {
                    reply.error(Errno::ENOTDIR);
                }
            }
        }
    }

    /// Handle unlink: allows unlinking files inside a tab directory or asset directories.
    /// This is crucial for `rm -rf /mountpoint/tab-name` to succeed, because
    /// `rm -rf` first unlinks all files inside directories before calling rmdir!
    fn unlink(&self, _req: &Request, parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        let parent_entry = match self.get_entry(parent) {
            Some(e) => e,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match parent_entry {
            InodeEntry::TabDir { .. } | InodeEntry::AssetCategoryDir { .. } => {
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

                // If user provided a custom non-URL title, set it on the new tab in Chrome
                if !name_str.starts_with("http://")
                    && !name_str.starts_with("https://")
                    && name_str != "about:blank"
                {
                    if let Ok(ws_url) = self.client.resolve_ws_url(&new_tab.id) {
                        let _ = self.client.set_title(&ws_url, name_str);
                    }
                }

                self.refresh_tabs();

                // Explicitly ensure the mkdir name is mapped to the new tab so subsequent lookups succeed immediately
                {
                    let mut tn = self.target_names.write().unwrap();
                    let mut nt = self.name_to_target.write().unwrap();
                    tn.insert(new_tab.id.clone(), name_str.to_string());
                    nt.insert(name_str.to_string(), new_tab.id.clone());
                }

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
        assert_eq!(sanitize_title("."), "Untitled");
        assert_eq!(sanitize_title(".."), "Untitled");
        assert_eq!(sanitize_title("..."), "Untitled");
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

    #[test]
    fn test_asset_categories() {
        for &cat in AssetCategory::all() {
            let name = cat.name();
            let parsed = AssetCategory::from_name(name);
            assert_eq!(parsed, Some(cat));
        }
        assert_eq!(AssetCategory::from_name("unknown"), None);
    }

    #[test]
    fn test_categorize_asset() {
        assert_eq!(
            categorize_asset("Image", "image/png", "https://example.com/pic.png"),
            AssetCategory::Images
        );
        assert_eq!(
            categorize_asset("", "image/webp", "https://example.com/asset?id=1"),
            AssetCategory::Images
        );
        assert_eq!(
            categorize_asset("Media", "video/mp4", "https://example.com/movie.mp4"),
            AssetCategory::Videos
        );
        assert_eq!(
            categorize_asset("", "audio/mpeg", "https://example.com/song.mp3"),
            AssetCategory::Audio
        );
        assert_eq!(
            categorize_asset(
                "Script",
                "application/javascript",
                "https://example.com/bundle.js"
            ),
            AssetCategory::Other
        );
        assert_eq!(
            categorize_asset("Stylesheet", "text/css", "https://example.com/style.css"),
            AssetCategory::Other
        );
    }

    #[test]
    fn test_derive_asset_filename() {
        assert_eq!(
            derive_asset_filename(
                "https://example.com/images/hero%20banner.png",
                "image/png",
                1,
                AssetCategory::Images
            ),
            "hero banner.png"
        );
        assert_eq!(
            derive_asset_filename(
                "https://example.com/media/clip?format=mp4#anchor",
                "video/mp4",
                1,
                AssetCategory::Videos
            ),
            "clip.mp4"
        );
        assert_eq!(
            derive_asset_filename(
                "data:image/svg+xml;base64,PHN2Zy...",
                "",
                4,
                AssetCategory::Images
            ),
            "data_images_4.svg"
        );
        assert_eq!(
            derive_asset_filename(
                "data:image/png;base64,iVBORw0KGgo...",
                "image/png",
                3,
                AssetCategory::Images
            ),
            "data_images_3.png"
        );
    }

    #[test]
    fn test_urlencoding_decode() {
        assert_eq!(urlencoding_decode("hello%20world"), "hello world");
        assert_eq!(urlencoding_decode("%2Fpath%2Ffile"), "/path/file");
        assert_eq!(urlencoding_decode("plain_text"), "plain_text");
        assert_eq!(urlencoding_decode("%E2%9C%93_check"), "✓_check");
        assert_eq!(urlencoding_decode("%F0%9F%A6%80_crab"), "🦀_crab");
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("valid_name.png"), "valid_name.png");
        assert_eq!(sanitize_filename("bad/name:here\0"), "bad_name:here_");
        assert_eq!(sanitize_filename("...dots..."), "dots");
    }
}
