use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

impl Kind {
    pub(crate) fn from_db(s: &str) -> Kind {
        if s == "dir" { Kind::Dir } else { Kind::File }
    }

    pub(crate) fn as_db(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
        }
    }
}

/// A file or folder, with its path as the caller's handle sees it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: Uuid,
    pub path: String,
    pub kind: Kind,
    /// Bytes for a file, 0 for a folder.
    pub size: u64,
    pub mime: Option<String>,
    /// Starts at 1 and goes up on every change to a file's contents; 0 for folders. Pass it back
    /// as `Expect::Version` to make a write fail if someone else changed the file meanwhile.
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

/// One page of a folder listing, in path order.
#[derive(Debug, Clone)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Pass as `ListOptions::after` to get the next page; `None` when this was the last.
    pub next: Option<String>,
}

/// Lines of a text file, numbered from 1.
#[derive(Debug, Clone)]
pub struct TextWindow {
    pub lines: Vec<String>,
    /// Number of the first line in `lines`.
    pub first_line: usize,
    pub total_lines: usize,
    pub version: i32,
}

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub version: i32,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<String>,
}

/// Something deleted, waiting in the trash to be restored or purged.
#[derive(Debug, Clone)]
pub struct TrashItem {
    pub id: Uuid,
    pub path: String,
    pub kind: Kind,
    /// Bytes of the current versions of the files in it.
    pub size: u64,
    pub trashed_at: DateTime<Utc>,
    pub trashed_by: Option<String>,
}

/// A workspace's measured usage and the limits the app set for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// Stored bytes: every kept version and the trash count, a copied file counts once.
    pub used_bytes: u64,
    /// Files and folders, the trash included.
    pub node_count: u64,
    pub quota_bytes: Option<u64>,
    pub quota_nodes: Option<u64>,
}

/// The size of a folder (or file) as it is now, without old versions or the trash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderUsage {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// Bytes and files of one storage category ("images", "videos", "audio", "documents",
/// "text", "archives", "other").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeUsage {
    pub category: String,
    pub files: u64,
    pub bytes: u64,
}

/// Where a workspace's space goes, for storage screens and cleanup prompts.
#[derive(Debug, Clone)]
pub struct Breakdown {
    /// The workspace's billed usage and limits (contents shared by copies count once).
    pub usage: Usage,
    pub files: u64,
    pub folders: u64,
    /// Current contents of every file.
    pub file_bytes: u64,
    /// Kept versions older than the current one, and their bytes.
    pub old_versions: u64,
    pub version_bytes: u64,
    /// Deleted items waiting in the trash, the files in them, and their bytes.
    pub trash_items: u64,
    pub trashed_files: u64,
    pub trash_bytes: u64,
    /// Current files by category, largest first.
    pub by_type: Vec<TypeUsage>,
}

/// Totals across every workspace of an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppUsage {
    pub workspaces: u64,
    pub used_bytes: u64,
    pub node_count: u64,
}

#[derive(Debug, Clone)]
pub struct ShareInfo {
    pub id: Uuid,
    pub path: String,
    pub kind: Kind,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A new share. `token` is shown only now: sysfile3 keeps just its hash.
#[derive(Debug, Clone)]
pub struct NewShare {
    pub info: ShareInfo,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct GlobResult {
    pub entries: Vec<Entry>,
    /// True when the scan limit was hit before every candidate was checked.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub files_searched: usize,
    /// Files passed over because they are binary or larger than `max_text_bytes`.
    pub files_skipped: usize,
    /// True when a scan or match limit cut the search short.
    pub truncated: bool,
}

/// Where and how a client uploads a file directly to object storage.
#[derive(Debug, Clone)]
pub struct UploadTicket {
    pub id: Uuid,
    /// Send the bytes with an HTTP PUT to this URL, then call `finish_upload`.
    pub url: String,
    pub size: u64,
    pub expires_at: DateTime<Utc>,
}
