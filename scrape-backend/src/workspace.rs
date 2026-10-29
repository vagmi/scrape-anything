//! Per-session workspace: a directory on the host (`<data_dir>/sessions/<id>/`)
//! that is bind-mounted at `/work` in the session's sandbox container.
//!
//! Layout:
//!   - `uploads/` — files the user attached (read-only inside the container).
//!   - `out/`     — where the agent's scripts write their results.
//!   - `session/` — backend-maintained context, e.g. `artifact.json`, the rows
//!     scraped so far, refreshed before every `run_command`.
//!
//! The agent's file tools reach the workspace under the `work/` prefix (the
//! container path `/work/...` is accepted too), so `read_file`/`search_file`/
//! `write_file` work on uploads and scripts exactly as on the per-origin store.
//!
//! Uploads arrive before a session exists (the side panel creates the session
//! on the first send), so they are staged under `<data_dir>/staging/<upload_id>/`
//! and moved into the workspace when a message references them.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

pub const PREFIX: &str = "work/";
const CONTAINER_PREFIX: &str = "/work/";
pub const UPLOADS_DIR: &str = "uploads";
const READ_ONLY_PREFIX: &str = "work/uploads/";

/// Is `path` a workspace path (`work/...` or `/work/...`)?
pub fn is_workspace_path(path: &str) -> bool {
    path.starts_with(PREFIX) || path.starts_with(CONTAINER_PREFIX)
}

/// Normalize `/work/x` to `work/x`.
pub fn normalize(path: &str) -> String {
    match path.strip_prefix(CONTAINER_PREFIX) {
        Some(rest) => format!("{PREFIX}{rest}"),
        None => path.to_string(),
    }
}

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// `data_dir` must already be absolute (see `Sandbox::init`).
    pub fn new(data_dir: &Path, session_id: &str) -> Self {
        Self { root: data_dir.join("sessions").join(session_id) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [UPLOADS_DIR, "out", "session"] {
            fs::create_dir_all(self.root.join(dir))?;
        }
        Ok(())
    }

    /// Map a `work/...` path to a host path inside the workspace. Rejects `..`,
    /// absolute components, and anything that resolves (through a symlink the
    /// agent made in the container) to outside the workspace.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let norm = normalize(path);
        let rel = norm.strip_prefix(PREFIX).ok_or_else(|| format!("{path} is not under {PREFIX}"))?;
        let rel_path = Path::new(rel);
        if rel.is_empty() || rel_path.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(format!("invalid workspace path: {path}"));
        }
        let full = self.root.join(rel_path);

        // Containment check on the deepest existing ancestor, canonicalized —
        // covers both existing files and the parent of a file about to be made.
        let root = self.root.canonicalize().map_err(|e| format!("workspace unavailable: {e}"))?;
        let mut probe = full.as_path();
        while !probe.exists() {
            probe = probe.parent().unwrap_or(&self.root);
        }
        let real = probe.canonicalize().map_err(|e| e.to_string())?;
        if !real.starts_with(&root) {
            return Err(format!("{path} resolves outside the workspace"));
        }
        Ok(full)
    }

    /// Files under the workspace whose `work/...` path starts with `prefix`,
    /// as `(path, size)`. Symlinks are skipped.
    pub fn list(&self, prefix: &str) -> Vec<(String, u64)> {
        let prefix = normalize(prefix);
        let mut out = Vec::new();
        if self.exists() {
            walk(&self.root, &self.root, &mut out);
        }
        out.retain(|(p, _)| p.starts_with(&prefix));
        out.sort();
        out
    }

    /// Text content of a workspace file, `None` if it doesn't exist.
    pub fn read(&self, path: &str) -> Result<Option<String>, String> {
        let full = self.resolve(path)?;
        if !full.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&full).map_err(|e| e.to_string())?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{path} is not UTF-8 text; inspect it with run_command instead"))
    }

    pub fn write(&self, path: &str, content: &str) -> Result<(), String> {
        if normalize(path).starts_with(READ_ONLY_PREFIX) {
            return Err(format!("{path} is an uploaded file and read-only; write to work/out/ instead"));
        }
        let full = self.resolve(path)?;
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&full, content).map_err(|e| e.to_string())
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else { continue };
        if meta.is_dir() {
            walk(root, &entry.path(), out);
        } else if meta.is_file() {
            if let Ok(rel) = entry.path().strip_prefix(root) {
                out.push((format!("{PREFIX}{}", rel.to_string_lossy()), meta.len()));
            }
        }
    }
}

// ───────────────────────────── uploads ──────────────────────────────────────

/// Reduce a client-supplied filename to a safe basename.
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    let cleaned: String = cleaned.chars().take(100).collect();
    if cleaned.is_empty() { "file".into() } else { cleaned }
}

pub fn staging_dir(data_dir: &Path, upload_id: &str) -> Result<PathBuf, String> {
    // Upload ids are ULIDs we minted; anything else is a forged path.
    if upload_id.is_empty() || !upload_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("invalid upload id: {upload_id}"));
    }
    Ok(data_dir.join("staging").join(upload_id))
}

/// Move staged uploads into the session's `uploads/` dir. Returns one
/// descriptor per file (path, size, preview) for the attachment turn.
pub fn attach(data_dir: &Path, ws: &Workspace, upload_ids: &[String]) -> Result<Vec<Value>, String> {
    ws.ensure().map_err(|e| e.to_string())?;
    let dest_dir = ws.root().join(UPLOADS_DIR);
    let mut files = Vec::new();
    for id in upload_ids {
        let stage = staging_dir(data_dir, id)?;
        let Some(src) = fs::read_dir(&stage).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()) else {
            return Err(format!("unknown or already used upload: {id}"));
        };
        let name = src.file_name().to_string_lossy().to_string();
        let dest = unique_path(&dest_dir, &name);
        fs::rename(src.path(), &dest).map_err(|e| e.to_string())?;
        let _ = fs::remove_dir(&stage);

        let name = dest.file_name().unwrap().to_string_lossy().to_string();
        let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
        files.push(json!({
            "path": format!("{PREFIX}{UPLOADS_DIR}/{name}"),
            "name": name,
            "size": size,
            "preview": preview(&dest),
        }));
    }
    Ok(files)
}

/// `dir/name`, or `dir/stem-N.ext` if that is taken.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem}-{n}{ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

/// Remove staged uploads older than `max_age` (never attached to a session).
pub fn sweep_staging(data_dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = fs::read_dir(data_dir.join("staging")) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

// ───────────────────────────── previews ─────────────────────────────────────

const PREVIEW_ROWS: usize = 3;
const PREVIEW_CHARS: usize = 600;
/// Parse JSON files up to this size to describe their shape.
const PREVIEW_JSON_MAX: u64 = 20 * 1024 * 1024;

/// A small description of a file's shape so the model can write its first
/// script without a probing call: header + first rows for CSV/TSV, top-level
/// type/keys/length for JSON, line count + first record for JSONL.
pub fn preview(path: &Path) -> Value {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    match ext.as_str() {
        "csv" | "tsv" => preview_lines(path, true),
        "jsonl" | "ndjson" => preview_lines(path, false),
        "json" if size <= PREVIEW_JSON_MAX => preview_json(path),
        _ => preview_head(path),
    }
}

fn preview_lines(path: &Path, header: bool) -> Value {
    let Ok(f) = fs::File::open(path) else { return Value::Null };
    let mut first: Vec<String> = Vec::new();
    let mut lines = 0usize;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { return preview_head(path) };
        if first.len() < PREVIEW_ROWS + header as usize {
            first.push(clip(&line, PREVIEW_CHARS));
        }
        lines += 1;
    }
    if header {
        let header_line = first.first().cloned().unwrap_or_default();
        json!({
            "format": "csv",
            "rows": lines.saturating_sub(1),
            "header": header_line,
            "first_rows": first.get(1..).unwrap_or_default(),
        })
    } else {
        json!({ "format": "jsonl", "records": lines, "first_records": first })
    }
}

fn preview_json(path: &Path) -> Value {
    let parsed = fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    match parsed {
        Some(Value::Array(arr)) => json!({
            "format": "json",
            "type": "array",
            "length": arr.len(),
            "first_item_keys": arr.first().and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()),
            "first_item": arr.first().map(|v| clip(&v.to_string(), PREVIEW_CHARS)),
        }),
        Some(Value::Object(obj)) => json!({
            "format": "json",
            "type": "object",
            "keys": obj.iter().take(50).map(|(k, v)| format!("{k}: {}", type_name(v))).collect::<Vec<_>>(),
        }),
        Some(other) => json!({ "format": "json", "type": type_name(&other) }),
        None => preview_head(path),
    }
}

fn preview_head(path: &Path) -> Value {
    let mut buf = vec![0u8; PREVIEW_CHARS];
    let n = fs::File::open(path).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    match std::str::from_utf8(&buf[..n]) {
        Ok(text) => json!({ "format": "text", "head": text }),
        // A multi-byte char may be cut at the end of the buffer.
        Err(e) if e.valid_up_to() + 4 >= n && e.valid_up_to() > 0 => {
            json!({ "format": "text", "head": String::from_utf8_lossy(&buf[..e.valid_up_to()]) })
        }
        Err(_) => json!({ "format": "binary" }),
    }
}

fn type_name(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Number(_) => "number".into(),
        Value::String(_) => "string".into(),
        Value::Array(a) => format!("array[{}]", a.len()),
        Value::Object(_) => "object".into(),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws() -> (PathBuf, Workspace) {
        let dir = std::env::temp_dir().join(format!("ws-test-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let ws = Workspace::new(&dir, "S1");
        ws.ensure().unwrap();
        (dir, ws)
    }

    #[test]
    fn sanitizes_filenames() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\x\\my file (1).csv"), "my_file__1_.csv");
        assert_eq!(sanitize_filename(".env"), "env");
        assert_eq!(sanitize_filename(""), "file");
    }

    #[test]
    fn resolve_rejects_escapes() {
        let (dir, ws) = temp_ws();
        assert!(ws.resolve("work/out/a.json").is_ok());
        assert!(ws.resolve("/work/out/a.json").is_ok());
        assert!(ws.resolve("work/../x").is_err());
        assert!(ws.resolve("work/").is_err());
        assert!(ws.resolve("notes.md").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", ws.root().join("out/etc")).unwrap();
            assert!(ws.resolve("work/out/etc/hosts").is_err());
            assert!(ws.list("").iter().all(|(p, _)| !p.contains("etc")));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uploads_are_read_only_and_listed() {
        let (dir, ws) = temp_ws();
        assert!(ws.write("work/uploads/x.csv", "a").is_err());
        ws.write("work/out/r.json", "[]").unwrap();
        assert_eq!(ws.read("/work/out/r.json").unwrap().as_deref(), Some("[]"));
        assert_eq!(ws.list("work/out"), vec![("work/out/r.json".to_string(), 2)]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn attach_moves_staged_files_and_previews_csv() {
        let (dir, ws) = temp_ws();
        for id in ["U1", "U2"] {
            let stage = staging_dir(&dir, id).unwrap();
            fs::create_dir_all(&stage).unwrap();
            fs::write(stage.join("leads.csv"), "name,url\na,https://a\nb,https://b\n").unwrap();
        }
        let files = attach(&dir, &ws, &["U1".into(), "U2".into()]).unwrap();
        assert_eq!(files[0]["path"], "work/uploads/leads.csv");
        assert_eq!(files[1]["path"], "work/uploads/leads-1.csv");
        assert_eq!(files[0]["preview"]["rows"], 2);
        assert_eq!(files[0]["preview"]["header"], "name,url");
        assert!(attach(&dir, &ws, &["U1".into()]).is_err(), "an upload attaches once");
        assert!(staging_dir(&dir, "../x").is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
