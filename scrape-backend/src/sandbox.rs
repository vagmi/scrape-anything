//! Docker sandbox for file analysis: one long-lived container per session, its
//! workspace (`crate::workspace`) bind-mounted at `/work`, commands run with
//! `docker exec`. The container is cattle — all state lives in the mounted
//! directory — so a container that died or was reaped is simply started again
//! on the next command.
//!
//! Like `tools::html_to_markdown` with pandoc, this shells out to the `docker`
//! CLI rather than talking to the daemon API, so it works unchanged with Docker
//! Desktop, colima or OrbStack.
//!
//! Network is left on (the agent may `pip install` / `bun add` or fetch
//! reference data); isolation is the container boundary plus resource limits,
//! dropped capabilities and a non-root uid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config;
use crate::workspace::{self, Workspace};

const LABEL: &str = "scrape.session";

pub struct Sandbox {
    cfg: config::Sandbox,
    /// Absolute `cfg.data_dir` (bind mounts need absolute host paths).
    data_dir: PathBuf,
    /// `docker` is reachable and the image exists.
    available: bool,
    /// Last command time per session, for the idle reaper.
    last_used: Mutex<HashMap<String, Instant>>,
    /// Serializes container start-up so two calls can't race on `docker run`.
    start_lock: Mutex<()>,
}

#[derive(Debug, Serialize)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub duration_ms: u128,
}

impl Sandbox {
    /// Resolve the data dir, check docker + the image, and remove containers a
    /// previous backend process left behind (their sessions' idle timers died
    /// with it).
    pub async fn init(cfg: config::Sandbox) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let data_dir = Path::new(&cfg.data_dir).canonicalize()?;
        workspace::sweep_staging(&data_dir, Duration::from_secs(24 * 3600));

        let available = cfg.enabled && image_present(&cfg.image).await;
        if cfg.enabled && !available {
            tracing::warn!(
                image = %cfg.image,
                "sandbox image not found (or docker unavailable) — run_command disabled. \
                 Build it with: docker build -t {} sandbox",
                cfg.image
            );
        }
        if available {
            reap_all().await;
            tracing::info!(image = %cfg.image, data_dir = %data_dir.display(), "sandbox ready");
        }
        Ok(Self {
            cfg,
            data_dir,
            available,
            last_used: Mutex::new(HashMap::new()),
            start_lock: Mutex::new(()),
        })
    }

    pub fn available(&self) -> bool {
        self.available
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn workspace(&self, session_id: &str) -> Workspace {
        Workspace::new(&self.data_dir, session_id)
    }

    /// Run `command` with `bash -c` in `/work` of the session's container,
    /// starting the container first if needed.
    pub async fn exec(&self, session_id: &str, command: &str, timeout_secs: Option<u64>) -> Result<ExecOutput, String> {
        if !self.available {
            return Err("the sandbox is unavailable (docker or the sandbox image is missing)".into());
        }
        let secs = timeout_secs
            .unwrap_or(self.cfg.command_timeout_secs)
            .clamp(1, self.cfg.max_command_timeout_secs);
        let name = self.ensure_container(session_id).await?;
        self.last_used.lock().await.insert(session_id.to_string(), Instant::now());

        // `timeout` inside the container kills the command itself; the outer
        // tokio timeout only guards against a wedged `docker exec` client.
        let started = Instant::now();
        let child = Command::new("docker")
            .args(["exec", "-w", "/work", &name, "timeout", "-k", "5", &secs.to_string(), "bash", "-c", command])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to launch docker: {e}"))?;
        let output = tokio::time::timeout(Duration::from_secs(secs + 15), child.wait_with_output())
            .await
            .map_err(|_| format!("command did not finish within {secs}s"))?
            .map_err(|e| e.to_string())?;

        let exit_code = output.status.code();
        let max = self.cfg.max_output_bytes;
        let (stdout, out_clipped) = clip_output(&output.stdout, max);
        let (stderr, err_clipped) = clip_output(&output.stderr, max);
        Ok(ExecOutput {
            exit_code,
            // 124: `timeout` fired; 137: it had to SIGKILL after the grace period.
            timed_out: matches!(exit_code, Some(124) | Some(137)),
            stdout,
            stderr,
            truncated: out_clipped || err_clipped,
            duration_ms: started.elapsed().as_millis(),
        })
    }

    async fn ensure_container(&self, session_id: &str) -> Result<String, String> {
        let name = container_name(session_id);
        let _guard = self.start_lock.lock().await;
        if container_running(&name).await {
            return Ok(name);
        }

        let ws = self.workspace(session_id);
        ws.ensure().map_err(|e| format!("could not create workspace: {e}"))?;
        let root = ws.root().to_string_lossy().to_string();
        let uploads = ws.root().join(workspace::UPLOADS_DIR).to_string_lossy().to_string();
        let user = owner_of(ws.root());

        // A stopped container with this name (crashed, OOM-killed) is in the way.
        let _ = docker(&["rm", "-f", &name]).await;
        let label = format!("{LABEL}={session_id}");
        let work_mount = format!("{root}:/work");
        let uploads_mount = format!("{uploads}:/work/uploads:ro");
        let pids = self.cfg.pids_limit.to_string();
        let mut args = vec![
            "run", "-d", "--name", &name, "--label", &label,
            "-v", &work_mount, "-v", &uploads_mount,
            "--memory", &self.cfg.memory, "--cpus", &self.cfg.cpus, "--pids-limit", &pids,
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
        ];
        if let Some(user) = &user {
            args.extend(["--user", user]);
        }
        args.push(&self.cfg.image);
        docker(&args).await.map_err(|e| format!("could not start sandbox container: {e}"))?;
        tracing::info!(target: "scrape_backend::sandbox", %session_id, container = %name, "sandbox container started");
        Ok(name)
    }

    /// Remove the session's container (its workspace stays).
    pub async fn stop(&self, session_id: &str) {
        self.last_used.lock().await.remove(session_id);
        let _ = docker(&["rm", "-f", &container_name(session_id)]).await;
    }

    /// Every minute, remove containers idle longer than `idle_ttl_secs`.
    pub fn spawn_reaper(self: &Arc<Self>) {
        if !self.available {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let ttl = Duration::from_secs(this.cfg.idle_ttl_secs);
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let idle: Vec<String> = this
                    .last_used
                    .lock()
                    .await
                    .iter()
                    .filter(|(_, t)| t.elapsed() > ttl)
                    .map(|(sid, _)| sid.clone())
                    .collect();
                for sid in idle {
                    tracing::info!(target: "scrape_backend::sandbox", session_id = %sid, "reaping idle sandbox container");
                    this.stop(&sid).await;
                }
            }
        });
    }
}

fn container_name(session_id: &str) -> String {
    format!("scrape-{}", session_id.to_ascii_lowercase())
}

/// `uid:gid` of the workspace dir, so the container writes files the backend
/// can read back (matters on Linux; Docker Desktop maps ownership anyway).
fn owner_of(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(format!("{}:{}", meta.uid(), meta.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

async fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("failed to launch docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

async fn image_present(image: &str) -> bool {
    docker(&["image", "inspect", "--format", "{{.Id}}", image]).await.is_ok()
}

async fn container_running(name: &str) -> bool {
    docker(&["inspect", "--format", "{{.State.Running}}", name]).await.is_ok_and(|s| s == "true")
}

async fn reap_all() {
    let filter = format!("label={LABEL}");
    let Ok(ids) = docker(&["ps", "-aq", "--filter", &filter]).await else { return };
    for id in ids.lines().filter(|l| !l.is_empty()) {
        let _ = docker(&["rm", "-f", id]).await;
    }
}

/// Lossy-decode `bytes`; if longer than `max`, keep the head and the tail
/// (errors usually end a stream) around a marker. Returns `(text, clipped)`.
fn clip_output(bytes: &[u8], max: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= max {
        return (text.into_owned(), false);
    }
    let half = max / 2;
    let head_end = (0..=half).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let tail_start = (text.len() - half..text.len()).find(|&i| text.is_char_boundary(i)).unwrap_or(text.len());
    let omitted = tail_start - head_end;
    (
        format!(
            "{}\n…[{omitted} bytes omitted — write large output to a file under /work/out and read it in parts]…\n{}",
            &text[..head_end],
            &text[tail_start..]
        ),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_head_and_tail() {
        let (s, clipped) = clip_output(b"short", 100);
        assert_eq!((s.as_str(), clipped), ("short", false));

        let long = format!("{}{}", "a".repeat(100), "z".repeat(100));
        let (s, clipped) = clip_output(long.as_bytes(), 20);
        assert!(clipped);
        assert!(s.starts_with("aaaaaaaaaa\n"));
        assert!(s.ends_with("\nzzzzzzzzzz"));
        assert!(s.contains("180 bytes omitted"));

        // Never splits a multi-byte char.
        let (s, _) = clip_output("é".repeat(50).as_bytes(), 11);
        assert!(s.starts_with("éé"));
    }

    /// Needs docker and the image: `cargo test -- --ignored sandbox_runs`.
    #[tokio::test]
    #[ignore]
    async fn sandbox_runs_tools_and_times_out() {
        // Under the crate, not the OS temp dir: Docker Desktop on macOS only
        // file-shares some host paths, and /var/folders may not be one.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("sandbox-test-{}", ulid::Ulid::new()));
        let cfg = config::Sandbox {
            data_dir: dir.to_string_lossy().into(),
            max_output_bytes: 64,
            max_command_timeout_secs: 2,
            ..Default::default()
        };
        let sb = Sandbox::init(cfg).await.unwrap();
        assert!(sb.available(), "build the image first: docker build -t scrape-sandbox:0.1 sandbox");
        let sid = "TESTSESSION01";
        let ws = sb.workspace(sid);
        ws.ensure().unwrap();
        std::fs::write(ws.root().join("uploads/a.json"), r#"[{"n":1},{"n":2}]"#).unwrap();

        let out = sb.exec(sid, "jq '[.[].n] | add' uploads/a.json", None).await.unwrap();
        assert_eq!((out.exit_code, out.stdout.trim()), (Some(0), "3"), "{out:?}");

        ws.write("work/sum.py", "import duckdb, pandas\nprint(duckdb.sql(\"select sum(n) from 'uploads/a.json'\").fetchall()[0][0])\n").unwrap();
        let out = sb.exec(sid, "python sum.py", None).await.unwrap();
        assert_eq!(out.stdout.trim(), "3", "{out:?}");

        let out = sb.exec(sid, "bun -e 'console.log(1+1)' && echo hi > out/x.txt", None).await.unwrap();
        assert_eq!(out.stdout.trim(), "2", "{out:?}");
        assert_eq!(std::fs::read_to_string(ws.root().join("out/x.txt")).unwrap(), "hi\n");

        let out = sb.exec(sid, "touch uploads/nope", None).await.unwrap();
        assert_ne!(out.exit_code, Some(0), "uploads are mounted read-only");

        let out = sb.exec(sid, "seq 1 1000", None).await.unwrap();
        assert!(out.truncated);

        let out = sb.exec(sid, "sleep 10", Some(30)).await.unwrap();
        assert!(out.timed_out, "{out:?}");

        sb.stop(sid).await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
