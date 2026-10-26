//! `glob` and `grep`.

use futures::stream::{self, StreamExt};
use globset::{GlobBuilder, GlobMatcher};
use regex::RegexBuilder;

use crate::db::{NodeRow, node_cols};
use crate::edit;
use crate::error::{Error, Result};
use crate::model::{GlobResult, GrepMatch, GrepResult, Kind};
use crate::path::VPath;
use crate::workspace::Workspace;

/// Longest matching line returned by `grep`, in characters.
const MAX_LINE_CHARS: usize = 400;

#[derive(Debug, Clone)]
pub struct GrepOptions {
    /// A regular expression (Rust `regex` syntax; always linear time).
    pub pattern: String,
    /// Folder to search under.
    pub under: String,
    /// Only files matching this glob: against the name when it has no `/` (`*.md`), against
    /// the path under `under` otherwise (`docs/**/*.md`).
    pub include: Option<String>,
    pub ignore_case: bool,
    /// At most this many matches, and never more than `Config::grep_max_matches`.
    pub max_matches: Option<usize>,
}

impl GrepOptions {
    pub fn new(pattern: impl Into<String>) -> Self {
        GrepOptions { pattern: pattern.into(), under: "/".into(), include: None, ignore_case: false, max_matches: None }
    }
}

fn matcher(pattern: &str) -> Result<GlobMatcher> {
    let pattern = pattern.trim_start_matches('/');
    if pattern.is_empty() {
        return Err(Error::InvalidInput("empty glob pattern".into()));
    }
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::InvalidInput(format!("bad glob pattern: {e}")))?
        .compile_matcher())
}

impl Workspace {
    /// Files under `under` whose path below it matches `pattern` (`*` stays within a folder,
    /// `**` crosses folders: `**/*.pdf`, `notes/*.md`).
    pub async fn glob(&self, pattern: &str, under: &str) -> Result<GlobResult> {
        self.need_read()?;
        let base = self.abs(under)?;
        let m = matcher(pattern)?;
        // Narrow the scan to the folder named by the pattern's literal start ("docs/2026/*.md"
        // only needs "docs/2026").
        let literal: String = pattern.trim_start_matches('/').chars().take_while(|c| !"*?[{\\".contains(*c)).collect();
        let scope = match literal.rfind('/') {
            Some(cut) => VPath::parse(&literal[..cut]).map(|d| base.join(&d)).unwrap_or_else(|_| base.clone()),
            None => base.clone(),
        };
        let (rows, truncated) = self.files_under(&scope).await?;
        let entries = rows
            .into_iter()
            .filter(|r| {
                let rel = VPath::trusted(r.path.clone()).strip(&base).map(|p| p.to_string()).unwrap_or_default();
                m.is_match(rel.trim_start_matches('/'))
            })
            .map(|r| self.entry(r))
            .collect();
        Ok(GlobResult { entries, truncated })
    }

    /// Lines matching a regular expression in the text files under a folder.
    pub async fn grep(&self, opts: GrepOptions) -> Result<GrepResult> {
        self.need_read()?;
        let base = self.abs(&opts.under)?;
        let re = RegexBuilder::new(&opts.pattern)
            .case_insensitive(opts.ignore_case)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| Error::InvalidInput(format!("bad regular expression: {e}")))?;
        let include = opts.include.as_deref().map(|g| (matcher(g), g.contains('/')));
        let include = match include {
            Some((Ok(m), by_path)) => Some((m, by_path)),
            Some((Err(e), _)) => return Err(e),
            None => None,
        };
        let cfg = &self.inner.cfg;
        let max_matches = opts.max_matches.unwrap_or(cfg.grep_max_matches).clamp(1, cfg.grep_max_matches);

        let (rows, mut truncated) = match self.node(&base).await? {
            // Searching a single file is allowed too.
            Some(row) if !row.is_dir() => (vec![row], false),
            None if !self.is_folder_root(&base) => return Err(Error::NotFound(self.shown(&base))),
            _ => self.files_under(&base).await?,
        };
        let mut skipped = 0;
        let candidates: Vec<NodeRow> = rows
            .into_iter()
            .filter(|r| match &include {
                None => true,
                Some((m, by_path)) => {
                    let p = VPath::trusted(r.path.clone());
                    let subject = if *by_path {
                        p.strip(&base).map(|x| x.to_string()).unwrap_or_default()
                    } else {
                        p.name().unwrap_or_default().to_string()
                    };
                    m.is_match(subject.trim_start_matches('/'))
                }
            })
            .filter(|r| {
                let fits = r.size as u64 <= cfg.max_text_bytes;
                if !fits {
                    skipped += 1;
                }
                fits
            })
            .collect();

        let blobs = &self.inner.blobs;
        let ws = self.id;
        let mut contents = stream::iter(candidates.into_iter().map(|row| async move {
            let bytes = blobs.get(ws, row.blob_id.expect("files have a blob"), row.size as u64).await;
            (row, bytes)
        }))
        .buffered(16);

        let mut matches = Vec::new();
        let mut searched = 0;
        'files: while let Some((row, bytes)) = contents.next().await {
            let bytes = bytes?;
            let Some(text) = edit::as_text(&bytes) else {
                skipped += 1;
                continue;
            };
            searched += 1;
            let shown = self.shown_str(&row.path);
            for (i, line) in edit::lines(text).into_iter().enumerate() {
                if re.is_match(line) {
                    if matches.len() == max_matches {
                        truncated = true;
                        break 'files;
                    }
                    matches.push(GrepMatch { path: shown.clone(), line: i + 1, text: clip(line) });
                }
            }
        }
        Ok(GrepResult { matches, files_searched: searched, files_skipped: skipped, truncated })
    }

    /// Up to `Config::search_scan_limit` files under `dir`, in path order, and whether there
    /// were more.
    async fn files_under(&self, dir: &VPath) -> Result<(Vec<NodeRow>, bool)> {
        let limit = self.inner.cfg.search_scan_limit;
        let (lo, hi) = dir.subtree();
        let mut tx = self.tx().await?;
        let mut rows: Vec<NodeRow> = sqlx::query_as(concat!(
            "SELECT ",
            node_cols!(),
            " FROM sf3_nodes
             WHERE workspace_id = $1 AND trash_id IS NULL AND kind = $2 AND path >= $3 AND path < $4
             ORDER BY path LIMIT $5"
        ))
        .bind(self.id)
        .bind(Kind::File.as_db())
        .bind(&lo)
        .bind(&hi)
        .bind(limit as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, truncated))
    }
}

fn clip(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}
