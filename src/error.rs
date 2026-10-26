use std::fmt;

/// Which limit a write ran into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quota {
    /// Total stored bytes, versions and trash included.
    Bytes,
    /// Number of files and folders, trash included.
    Nodes,
}

impl fmt::Display for Quota {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Quota::Bytes => "storage limit reached",
            Quota::Nodes => "file count limit reached",
        })
    }
}

/// Everything that can go wrong. Paths in errors are as the caller sees them (relative to its
/// handle's root), so an error never reveals folders outside what the caller may see.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No such file, folder, version, trash item or upload.
    #[error("not found: {0}")]
    NotFound(String),
    /// The path is taken.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// A folder was needed but this is a file.
    #[error("not a folder: {0}")]
    NotADirectory(String),
    /// A file was needed but this is a folder.
    #[error("is a folder: {0}")]
    IsADirectory(String),
    /// Deleting or replacing a folder that has something in it.
    #[error("folder is not empty: {0}")]
    NotEmpty(String),
    /// The file changed since the version the caller named.
    #[error("{path} changed: expected version {expected}, found {actual}")]
    Conflict {
        /// The file.
        path: String,
        /// The version the caller expected.
        expected: i32,
        /// The version it is now.
        actual: i32,
    },
    /// The write would go over the workspace's limit; nothing was stored.
    #[error("{0}")]
    QuotaExceeded(Quota),
    /// Over a size limit from [`Config`](crate::Config).
    #[error("too large: {size} bytes, the limit is {limit}")]
    TooLarge {
        /// The size asked for.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// A text operation on a binary file.
    #[error("not a text file: {0}")]
    NotText(String),
    /// The text to replace is not in the file.
    #[error("the text to replace was not found in {0}")]
    EditNotFound(String),
    /// The text to replace appears more than once and `replace_all` was not set.
    #[error("the text to replace appears {count} times in {path}; include more context or replace all")]
    EditAmbiguous {
        /// The file.
        path: String,
        /// How many times the text appears.
        count: usize,
    },
    /// A path that breaks the rules (`..`, control characters, too long, too deep).
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// A bad argument.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The handle may not do this.
    #[error("not allowed: {0}")]
    PermissionDenied(&'static str),
    /// The link does not exist, expired, was revoked, or its file is in the trash.
    #[error("this link does not exist, has expired or was revoked")]
    ShareNotFound,
    /// Not available in this setup (for example signed URLs without a signer).
    #[error("not supported: {0}")]
    Unsupported(&'static str),
    /// Postgres failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Object storage failed.
    #[error("storage error: {0}")]
    Storage(#[from] object_store::Error),
    /// Reading the caller's upload stream failed.
    #[error("reading the upload failed: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// A stable, machine-readable code for HTTP and MCP layers.
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "not_found",
            Error::AlreadyExists(_) => "already_exists",
            Error::NotADirectory(_) => "not_a_directory",
            Error::IsADirectory(_) => "is_a_directory",
            Error::NotEmpty(_) => "not_empty",
            Error::Conflict { .. } => "conflict",
            Error::QuotaExceeded(_) => "quota_exceeded",
            Error::TooLarge { .. } => "too_large",
            Error::NotText(_) => "not_text",
            Error::EditNotFound(_) => "edit_not_found",
            Error::EditAmbiguous { .. } => "edit_ambiguous",
            Error::InvalidPath(_) => "invalid_path",
            Error::InvalidInput(_) => "invalid_input",
            Error::PermissionDenied(_) => "permission_denied",
            Error::ShareNotFound => "share_not_found",
            Error::Unsupported(_) => "unsupported",
            Error::Database(_) => "database",
            Error::Storage(_) => "storage",
            Error::Io(_) => "io",
        }
    }

    /// True for errors caused by the request rather than by sysfile3 or its backends.
    pub fn is_client_error(&self) -> bool {
        !matches!(self, Error::Database(_) | Error::Storage(_))
    }

    pub(crate) fn is_unique_violation(&self) -> bool {
        matches!(self, Error::Database(sqlx::Error::Database(e)) if e.is_unique_violation())
    }
}

/// The result of every sysfile3 call.
pub type Result<T, E = Error> = std::result::Result<T, E>;
