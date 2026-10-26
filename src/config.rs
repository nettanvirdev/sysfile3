use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// Limits and tuning. `Config::default()` suits a multi-user service; every field can be changed.
#[derive(Debug, Clone)]
pub struct Config {
    /// Object keys are `{key_prefix}/{workspace id}/{blob id}`.
    pub key_prefix: String,
    /// Largest single file, however it arrives.
    pub max_file_bytes: u64,
    /// Largest file `write` and `read` take or return in one piece; bigger files go through
    /// `write_stream`, `open`, `read_range` or signed URLs.
    pub max_inline_bytes: u64,
    /// Largest file the text tools (`read_text`, `edit`, `grep`) will load.
    pub max_text_bytes: u64,
    /// Versions kept per file, the current one included. Older ones are deleted on write.
    pub max_versions: u32,
    /// Quotas given to a workspace when it is first opened. `None` is unlimited.
    pub default_quota_bytes: Option<u64>,
    pub default_quota_nodes: Option<u64>,
    /// Most entries `list` returns per page.
    pub list_limit: usize,
    /// Most files `glob` and `grep` look at in one call.
    pub search_scan_limit: usize,
    /// Most matching lines `grep` returns.
    pub grep_max_matches: usize,
    /// Most files and folders a single `copy` creates.
    pub max_copy_nodes: usize,
    /// In-memory cache of small file contents, shared by every workspace.
    pub cache_bytes: u64,
    /// Files larger than this are never cached.
    pub cache_item_bytes: u64,
    /// A cached file is dropped after going unread this long; every read renews it.
    pub cache_idle: Duration,
    /// How long signed upload and download URLs stay valid.
    pub upload_ttl: Duration,
    pub download_ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            key_prefix: "sysfile3".into(),
            max_file_bytes: 5 * 1024 * MIB,
            max_inline_bytes: 64 * MIB,
            max_text_bytes: 8 * MIB,
            max_versions: 20,
            default_quota_bytes: None,
            default_quota_nodes: None,
            list_limit: 1000,
            search_scan_limit: 5000,
            grep_max_matches: 500,
            max_copy_nodes: 10_000,
            cache_bytes: 256 * MIB,
            cache_item_bytes: 256 * 1024,
            cache_idle: Duration::from_secs(3600),
            upload_ttl: Duration::from_secs(3600),
            download_ttl: Duration::from_secs(900),
        }
    }
}
