use chrono::{DateTime, Utc};
use uuid::Uuid;

/// File or folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A file, with contents.
    File,
    /// A folder.
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
    /// Stable id; it survives renames and moves.
    pub id: Uuid,
    /// Path as the handle sees it, relative to the handle's root.
    pub path: String,
    /// File or folder.
    pub kind: Kind,
    /// Bytes for a file, 0 for a folder.
    pub size: u64,
    /// Content type, such as `image/png`.
    pub mime: Option<String>,
    /// Starts at 1 and goes up on every change to a file's contents; 0 for folders. Pass it back
    /// as `Expect::Version` to make a write fail if someone else changed the file meanwhile.
    pub version: i32,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When its contents or name last changed.
    pub updated_at: DateTime<Utc>,
    /// The actor that created it, when one was set.
    pub created_by: Option<String>,
    /// The actor that last changed it, when one was set.
    pub updated_by: Option<String>,
}

impl Entry {
    /// True for folders.
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

/// One page of a folder listing, in path order.
#[derive(Debug, Clone)]
pub struct Listing {
    /// Files and folders on this page, in path order.
    pub entries: Vec<Entry>,
    /// Pass as `ListOptions::after` to get the next page; `None` when this was the last.
    pub next: Option<String>,
}

/// Lines of a text file, numbered from 1.
#[derive(Debug, Clone)]
pub struct TextWindow {
    /// The lines, without line endings.
    pub lines: Vec<String>,
    /// Number of the first line in `lines`.
    pub first_line: usize,
    /// Lines in the whole file.
    pub total_lines: usize,
    /// The file version these lines came from; pass it to `Expect::Version` to edit safely.
    pub version: i32,
}

/// One kept version of a file.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// Version number; the file's first version is 1.
    pub version: i32,
    /// Bytes in this version.
    pub size: u64,
    /// When this version was written.
    pub created_at: DateTime<Utc>,
    /// The actor that wrote it, when one was set.
    pub created_by: Option<String>,
}

/// Something deleted, waiting in the trash to be restored or purged.
#[derive(Debug, Clone)]
pub struct TrashItem {
    /// Pass to `restore` or `purge`.
    pub id: Uuid,
    /// Where it was, as the handle sees it.
    pub path: String,
    /// File or folder.
    pub kind: Kind,
    /// Bytes of the current versions of the files in it.
    pub size: u64,
    /// When it was deleted.
    pub trashed_at: DateTime<Utc>,
    /// The actor that deleted it, when one was set.
    pub trashed_by: Option<String>,
}

/// A workspace's measured usage and the limits the app set for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// Stored bytes: every kept version and the trash count, a copied file counts once.
    pub used_bytes: u64,
    /// Files and folders, the trash included.
    pub node_count: u64,
    /// Byte limit; `None` is unlimited.
    pub quota_bytes: Option<u64>,
    /// File and folder limit; `None` is unlimited.
    pub quota_nodes: Option<u64>,
}

/// The size of a folder (or file) as it is now, without old versions or the trash. `folders`
/// counts the folders inside, not the folder itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderUsage {
    /// Files inside, or 1 for a single file.
    pub files: u64,
    /// Folders inside (not counting the folder itself).
    pub folders: u64,
    /// Bytes of the current contents of those files.
    pub bytes: u64,
}

/// Bytes and files of one storage category ("images", "videos", "audio", "documents",
/// "text", "archives", "other").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeUsage {
    /// One of `images`, `videos`, `audio`, `documents`, `text`, `archives`, `other`.
    pub category: String,
    /// Files in this category.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// Where a workspace's space goes, for storage screens and cleanup prompts.
#[derive(Debug, Clone)]
pub struct Breakdown {
    /// The workspace's billed usage and limits (contents shared by copies count once).
    pub usage: Usage,
    /// Files, not counting the trash.
    pub files: u64,
    /// Folders, not counting the trash.
    pub folders: u64,
    /// Current contents of every file.
    pub file_bytes: u64,
    /// Kept versions older than the current one, and their bytes.
    pub old_versions: u64,
    /// Bytes of those older versions.
    pub version_bytes: u64,
    /// Deleted items waiting in the trash, the files in them, and their bytes.
    pub trash_items: u64,
    /// Files inside the trashed items.
    pub trashed_files: u64,
    /// Bytes of the trashed files.
    pub trash_bytes: u64,
    /// Current files by category, largest first.
    pub by_type: Vec<TypeUsage>,
}

/// Totals across every workspace of an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppUsage {
    /// Workspaces the app has.
    pub workspaces: u64,
    /// Stored bytes across all of them.
    pub used_bytes: u64,
    /// Files and folders across all of them.
    pub node_count: u64,
}

/// A live read-only link.
#[derive(Debug, Clone)]
pub struct ShareInfo {
    /// Pass to `revoke_share`.
    pub id: Uuid,
    /// What it shares, as the handle sees it.
    pub path: String,
    /// File or folder.
    pub kind: Kind,
    /// When it was made.
    pub created_at: DateTime<Utc>,
    /// The actor that made it, when one was set.
    pub created_by: Option<String>,
    /// When it stops working; `None` never.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A new share. `token` is shown only now: sysfile3 keeps just its hash.
#[derive(Debug, Clone)]
pub struct NewShare {
    /// The link itself.
    pub info: ShareInfo,
    /// The secret for the URL. Only its hash is stored, so this is the only chance to read it.
    pub token: String,
}

/// Files matching a `glob` pattern.
#[derive(Debug, Clone)]
pub struct GlobResult {
    /// Matching files, in path order.
    pub entries: Vec<Entry>,
    /// True when the scan limit was hit before every candidate was checked.
    pub truncated: bool,
}

/// One line `grep` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// The file, as the handle sees it.
    pub path: String,
    /// Line number, from 1.
    pub line: usize,
    /// The line, shortened if very long.
    pub text: String,
}

/// What `grep` found.
#[derive(Debug, Clone)]
pub struct GrepResult {
    /// Matching lines, by file then line.
    pub matches: Vec<GrepMatch>,
    /// Text files searched.
    pub files_searched: usize,
    /// Files passed over because they are binary or larger than `max_text_bytes`.
    pub files_skipped: usize,
    /// True when a scan or match limit cut the search short.
    pub truncated: bool,
}

/// Where and how a client uploads a file directly to object storage.
#[derive(Debug, Clone)]
pub struct UploadTicket {
    /// Pass to `finish_upload` or `cancel_upload`.
    pub id: Uuid,
    /// Send the bytes with an HTTP PUT to this URL, then call `finish_upload`.
    pub url: String,
    /// Exact bytes the client must upload.
    pub size: u64,
    /// When the URL and the reservation expire.
    pub expires_at: DateTime<Utc>,
}
