//! Filesystem-backed workflow catalog. Reads `.workflow`
//! files from a directory, parses them lazily on first
//! access, and caches the parsed AST in memory keyed by
//! workflow name.
//!
//! The store is the single source of truth the coordinator's
//! `workflow.list` and `workflow.run` capabilities consult.
//! File-level errors (missing dir, IO failure, parse error)
//! are surfaced as `StoreError` so the coordinator can render
//! an operator-actionable message.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use super::ast::Workflow;
use super::parser::{ParseError, parse_str};

/// Store-level errors. Each variant carries enough context
/// for the coordinator to render a useful error message.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    #[error("workflows directory `{0}` does not exist")]
    DirMissing(PathBuf),

    #[error("workflows directory `{path}` could not be read: {cause}")]
    DirIo { path: PathBuf, cause: String },

    #[error("workflow file `{path}` could not be read: {cause}")]
    FileIo { path: PathBuf, cause: String },

    #[error("workflow `{name}` not found in `{dir}`")]
    NotFound { name: String, dir: PathBuf },

    #[error(
        "workflow `{name}` in file `{path}` failed to parse: {message} (line {line}, column {column})"
    )]
    Parse {
        name: String,
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
}

/// One catalog entry returned by [`WorkflowStore::list`].
#[derive(Debug, Clone)]
pub struct WorkflowEntry {
    pub name: String,
    pub description: String,
    pub version: u32,
    pub path: PathBuf,
}

/// Workflow catalog. Cheap to clone — the underlying cache
/// is shared via `Arc`.
#[derive(Clone)]
pub struct WorkflowStore {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    cache: RwLock<BTreeMap<String, Arc<Workflow>>>,
}

impl WorkflowStore {
    /// Build a store rooted at `dir`. Existence is checked
    /// lazily so a coordinator that starts with no workflows
    /// directory still works — operators can create one
    /// later and the next list/run reflects the change.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                dir,
                cache: RwLock::new(BTreeMap::new()),
            }),
        }
    }

    /// Directory backing this store. Used in error messages
    /// + the `workflow.list` response body.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Enumerate every `.workflow` file in the directory.
    /// Each file is parsed eagerly so the response includes
    /// a real description / version. Files that fail to
    /// parse are skipped from the list (the operator sees
    /// the parse error when they try to RUN that workflow).
    pub fn list(&self) -> Result<Vec<WorkflowEntry>, StoreError> {
        if !self.inner.dir.exists() {
            return Err(StoreError::DirMissing(self.inner.dir.clone()));
        }
        let read = std::fs::read_dir(&self.inner.dir).map_err(|e| StoreError::DirIo {
            path: self.inner.dir.clone(),
            cause: e.to_string(),
        })?;
        let mut entries = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("workflow") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Pull from cache when present so list+run share
            // the same parsed instance and a freshly-edited
            // file picks up after the cache is cleared.
            let cached = self
                .inner
                .cache
                .read()
                .ok()
                .and_then(|c| c.get(stem).cloned());
            let parsed = match cached {
                Some(w) => w,
                None => match self.load_from_path(&path, stem) {
                    Ok(w) => w,
                    Err(_) => continue,
                },
            };
            entries.push(WorkflowEntry {
                name: parsed.name.clone(),
                description: parsed.description.clone(),
                version: parsed.version,
                path: path.clone(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Load (or fetch from cache) the workflow named `name`.
    /// The file is expected at `<dir>/<name>.workflow`.
    pub fn get(&self, name: &str) -> Result<Arc<Workflow>, StoreError> {
        if let Ok(cache) = self.inner.cache.read()
            && let Some(w) = cache.get(name)
        {
            return Ok(w.clone());
        }
        let path = self.inner.dir.join(format!("{name}.workflow"));
        if !path.exists() {
            return Err(StoreError::NotFound {
                name: name.to_string(),
                dir: self.inner.dir.clone(),
            });
        }
        self.load_from_path(&path, name)
    }

    fn load_from_path(
        &self,
        path: &Path,
        expected_name: &str,
    ) -> Result<Arc<Workflow>, StoreError> {
        let source = std::fs::read_to_string(path).map_err(|e| StoreError::FileIo {
            path: path.to_path_buf(),
            cause: e.to_string(),
        })?;
        let parsed = parse_str(&source).map_err(|e: ParseError| StoreError::Parse {
            name: expected_name.to_string(),
            path: path.to_path_buf(),
            line: e.line,
            column: e.column,
            message: e.message,
        })?;
        let arc = Arc::new(parsed);
        if let Ok(mut cache) = self.inner.cache.write() {
            cache.insert(expected_name.to_string(), arc.clone());
        }
        Ok(arc)
    }

    /// Drop every cached entry. Called by operators (via
    /// `workflow.reload` when wired) after they edit a
    /// `.workflow` file in place.
    #[allow(dead_code)]
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.inner.cache.write() {
            cache.clear();
        }
    }
}
