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
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use super::ast::Workflow;
use super::parser::{ParseError, parse_str};

/// SECTION 8: hard cap on a `.workflow` file's size. Files above
/// this are rejected without being read into memory, so a
/// hostile multi-GB file cannot OOM the controller. 4 MiB is far
/// beyond any legitimate workflow definition.
pub const MAX_WORKFLOW_BYTES: u64 = 4 * 1024 * 1024;

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

    /// SECTION 8: the wire-supplied workflow name is not a safe
    /// single filename — it contains a path separator, `..`, or
    /// otherwise resolves outside the workflow store directory.
    #[error("workflow name `{name}` is invalid: {reason}")]
    InvalidName { name: String, reason: String },

    /// SECTION 8: the `.workflow` file exceeds the size cap. We
    /// refuse to read it into memory (a multi-GB file would OOM
    /// the controller).
    #[error("workflow `{name}` file is too large: {size} bytes exceeds the {limit}-byte limit")]
    TooLarge { name: String, size: u64, limit: u64 },

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
    ///
    /// SECTION 8: `name` reaches here from the wire
    /// (`workflow.run`), so it is validated as a safe single
    /// filename BEFORE being joined to the store directory — a
    /// name like `../../etc/shadow` is rejected, not resolved.
    pub fn get(&self, name: &str) -> Result<Arc<Workflow>, StoreError> {
        Self::validate_name(name)?;
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
        // Defense in depth: confirm the CANONICAL resolved path is
        // still inside the canonical store directory before
        // opening (defeats a symlink that points outside the
        // store even when the name itself is a plain filename).
        if let (Ok(canon_dir), Ok(canon_path)) =
            (self.inner.dir.canonicalize(), path.canonicalize())
            && !canon_path.starts_with(&canon_dir)
        {
            return Err(StoreError::InvalidName {
                name: name.to_string(),
                reason: "resolves outside the workflow store directory".to_string(),
            });
        }
        self.load_from_path(&path, name)
    }

    /// SECTION 8: validate a wire-supplied workflow name. It must
    /// be a single, normal filename component — no path
    /// separators (`/` or `\`), no `..`, no NUL, not empty, and
    /// not root/drive-anchored. This blocks path traversal at the
    /// name level before any filesystem join happens.
    fn validate_name(name: &str) -> Result<(), StoreError> {
        let invalid = |reason: &str| StoreError::InvalidName {
            name: name.to_string(),
            reason: reason.to_string(),
        };
        if name.trim().is_empty() {
            return Err(invalid("name is empty"));
        }
        if name.contains('/') || name.contains('\\') {
            return Err(invalid("name contains a path separator"));
        }
        if name.contains("..") {
            return Err(invalid("name contains `..`"));
        }
        if name.contains('\0') {
            return Err(invalid("name contains a NUL byte"));
        }
        // Must be exactly one normal path component (catches
        // platform-specific roots / drive prefixes the substring
        // checks above might miss).
        let mut comps = Path::new(name).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(_)), None) => Ok(()),
            _ => Err(invalid("name is not a single filename component")),
        }
    }

    fn load_from_path(
        &self,
        path: &Path,
        expected_name: &str,
    ) -> Result<Arc<Workflow>, StoreError> {
        // SECTION 8: size-cap BEFORE reading. Stat the file and
        // refuse to slurp anything over the limit into memory.
        let meta = std::fs::metadata(path).map_err(|e| StoreError::FileIo {
            path: path.to_path_buf(),
            cause: e.to_string(),
        })?;
        if meta.len() > MAX_WORKFLOW_BYTES {
            return Err(StoreError::TooLarge {
                name: expected_name.to_string(),
                size: meta.len(),
                limit: MAX_WORKFLOW_BYTES,
            });
        }
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

#[cfg(test)]
mod section8_tests {
    use super::*;

    const SAMPLE: &str = r#"
name: demo
version: 1
description: d
agents:
  only:
    peer: ai
    capability: chat
    input: "hi"
    output: out
flow:
  start: only
  result: "{{only.output}}"
"#;

    fn store_with(name: &str, body: &str) -> (WorkflowStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("{name}.workflow")), body).unwrap();
        (WorkflowStore::new(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn section8_traversal_names_are_rejected_not_resolved() {
        // CRITERION 1: a traversal name must be rejected BEFORE
        // it can resolve to a file outside the store dir.
        let (store, _dir) = store_with("demo", SAMPLE);
        for bad in [
            "../../etc/shadow",
            "../secret",
            "a/b",
            "a\\b",
            "..",
            "",
            "/etc/passwd",
        ] {
            let err = store
                .get(bad)
                .expect_err(&format!("`{bad}` must be rejected"));
            assert!(
                matches!(err, StoreError::InvalidName { .. }),
                "`{bad}` should be InvalidName, got {err:?}"
            );
        }
    }

    #[test]
    fn section8_legitimate_in_store_name_loads() {
        // CRITERION 2: a normal in-store workflow still loads.
        let (store, _dir) = store_with("demo", SAMPLE);
        let wf = store.get("demo").expect("legitimate name must load");
        assert_eq!(wf.name, "demo");
    }

    #[test]
    fn section8_oversize_workflow_file_is_rejected_without_oom() {
        // CRITERION 3: a file over the cap is rejected by stat,
        // never read into memory.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.workflow");
        // Write one byte past the cap (sparse-ish; we write a real
        // buffer just over a small portion — use set_len to avoid
        // allocating 4MiB in the test).
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_WORKFLOW_BYTES + 1).unwrap();
        drop(f);
        let store = WorkflowStore::new(dir.path().to_path_buf());
        let err = store
            .get("huge")
            .expect_err("oversize file must be rejected");
        match err {
            StoreError::TooLarge { size, limit, .. } => {
                assert_eq!(limit, MAX_WORKFLOW_BYTES);
                assert!(size > limit);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }
}
