use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Notify;

use crate::config::Config;
use crate::db::{Job, Stem, Store};
use crate::storage::Storage;

const AUDIO_EXTS: [&str; 5] = ["mp3", "wav", "flac", "ogg", "m4a"];

pub struct Worker {
    pub cfg: Arc<Config>,
    pub store: Store,
    pub storage: Storage,
    pub notify: Arc<Notify>,
}

impl Worker {
    /// Runs forever. Spawn exactly one of these on a small CPU box — demucs
    /// already saturates every core, so a second concurrent job makes both slower.
    pub async fn run(self) {
        loop {
            match self.store.claim_next().await {
                Ok(Some(job)) => {
                    let id = job.id.clone();
                    tracing::info!(job = %id, file = %job.filename, "job started");

                    match self.process(&job).await {
                        Ok(stems) => {
                            if let Err(e) = self.store.mark_done(&id, &stems).await {
                                tracing::error!(job = %id, error = ?e, "mark_done failed");
                            } else {
                                tracing::info!(job = %id, stems = stems.len(), "job done");
                            }
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            tracing::warn!(job = %id, error = %msg, "job failed");
                            let _ = self.store.mark_failed(&id, &msg).await;
                        }
                    }

                    // Best-effort scratch cleanup; a leftover dir must never
                    // stop the next job from running.
                    let scratch = self.cfg.work_dir.join(&id);
                    let _ = tokio::fs::remove_dir_all(&scratch).await;
                }

                Ok(None) => {
                    // Nothing queued. Wake on a new upload, but also poll on a
                    // timer so a missed notification can't wedge the queue.
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    }
                }

                Err(e) => {
                    tracing::error!(error = ?e, "claim_next failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn process(&self, job: &Job) -> Result<Vec<Stem>> {
        let scratch = self.cfg.work_dir.join(&job.id);
        let in_dir = scratch.join("in");
        let out_dir = scratch.join("out");
        tokio::fs::create_dir_all(&in_dir).await?;
        tokio::fs::create_dir_all(&out_dir).await?;

        // 1. pull the upload back down from RustFS
        let local_in = in_dir.join(sanitize(&job.filename));
        self.storage
            .get_to_file(&job.input_key, &local_in)
            .await
            .context("downloading input from storage")?;

        // 2. separate
        self.run_demucs(job, &local_in, &out_dir).await?;

        // 3. push every produced stem back up
        let produced = collect_audio(&out_dir).await?;
        if produced.is_empty() {
            bail!("demucs exited successfully but produced no audio files");
        }

        let mut stems = Vec::with_capacity(produced.len());
        for path in produced {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("stem")
                .to_string();
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("bin")
                .to_string();

            let key = format!("jobs/{}/output/{}.{}", job.id, name, ext);
            let bytes = self
                .storage
                .put_file(&key, &path)
                .await
                .with_context(|| format!("uploading stem {name}"))?;

            stems.push(Stem { name, key, bytes });
        }

        stems.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(stems)
    }

    async fn run_demucs(&self, job: &Job, input: &Path, out_dir: &Path) -> Result<()> {
        let cfg = &self.cfg;

        let mut cmd = Command::new(&cfg.demucs_bin);
        cmd.arg("-n")
            .arg(&job.model)
            .arg("-d")
            .arg("cpu")
            .arg("-j")
            .arg(cfg.demucs_jobs.to_string())
            .arg("--segment")
            .arg(cfg.demucs_segment.to_string())
            .arg("-o")
            .arg(out_dir);

        if cfg.demucs_format == "mp3" {
            cmd.arg("--mp3").arg("--mp3-bitrate").arg("320");
        }

        if let Some(stem) = job.two_stems.as_deref() {
            cmd.arg("--two-stems").arg(stem);
        }

        cmd.arg(input);

        // Pin thread counts. Without this, torch and OpenMP each spawn a pool
        // sized to the machine and fight each other for the same 4 cores.
        let threads = cfg.demucs_threads.to_string();
        cmd.env("OMP_NUM_THREADS", &threads)
            .env("MKL_NUM_THREADS", &threads)
            .env("TORCH_NUM_THREADS", &threads)
            // unbuffered python, otherwise the progress bar arrives in one lump
            .env("PYTHONUNBUFFERED", "1");

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning `{}` — is demucs on PATH?", cfg.demucs_bin))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("no stderr pipe"))?;
        let stdout = child.stdout.take();

        // demucs writes its progress bar to stderr with \r, so read raw and
        // split on both \r and \n rather than using a line reader.
        let store = self.store.clone();
        let job_id = job.id.clone();
        let stderr_task =
            tokio::spawn(async move { pump_progress(stderr, store, job_id).await });

        let stdout_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(mut out) = stdout {
                let _ = out.read_to_string(&mut buf).await;
            }
            buf
        });

        let wait = child.wait();
        let status = match tokio::time::timeout(
            std::time::Duration::from_secs(cfg.job_timeout_secs),
            wait,
        )
        .await
        {
            Ok(s) => s.context("waiting for demucs")?,
            Err(_) => {
                bail!(
                    "demucs exceeded the {}s timeout and was killed",
                    cfg.job_timeout_secs
                );
            }
        };

        let stderr_text = stderr_task.await.unwrap_or_default();
        let _ = stdout_task.await;

        if !status.success() {
            bail!(
                "demucs exited with {}: {}",
                status.code().unwrap_or(-1),
                tail(&stderr_text, 1500)
            );
        }

        Ok(())
    }
}

/// Reads demucs' stderr, scrapes the `NN%` out of the progress bar, and mirrors
/// it into the job row. Returns the full stderr text for error reporting.
async fn pump_progress<R>(mut reader: R, store: Store, job_id: String) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let re = regex::Regex::new(r"(\d{1,3})%").expect("static regex");

    let mut full = String::new();
    let mut pending = String::new();
    let mut buf = [0u8; 4096];
    let mut last_reported: i64 = -1;

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        let chunk = String::from_utf8_lossy(&buf[..n]);
        full.push_str(&chunk);
        pending.push_str(&chunk);

        // Keep only the last partial segment; everything before a \r or \n is
        // a complete render of the bar.
        let segments: Vec<&str> = pending.split(['\r', '\n']).collect();
        let (complete, remainder) = segments.split_at(segments.len().saturating_sub(1));

        for seg in complete {
            if let Some(c) = re.captures(seg) {
                if let Ok(pct) = c[1].parse::<i64>() {
                    if pct != last_reported && (0..=100).contains(&pct) {
                        last_reported = pct;
                        let _ = store.set_progress(&job_id, pct).await;
                    }
                }
            }
        }

        pending = remainder.first().map(|s| s.to_string()).unwrap_or_default();

        if full.len() > 200_000 {
            // Runaway output — keep the tail only.
            full = tail(&full, 100_000);
        }
    }

    full
}

fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Slice on a char boundary so we never panic on multi-byte output.
        let start = s.len() - n;
        let start = (start..s.len())
            .find(|i| s.is_char_boundary(*i))
            .unwrap_or(s.len());
        s[start..].to_string()
    }
}

/// Demucs writes to `<out>/<model>/<track name>/<stem>.<ext>`; rather than
/// rebuilding that path (and getting the track-name normalisation wrong), just
/// walk the output tree.
async fn collect_audio(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let is_audio = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                    .unwrap_or(false);
                if is_audio {
                    found.push(path);
                }
            }
        }
    }

    Ok(found)
}

/// Strip path separators and anything exotic out of an uploaded filename.
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();

    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "upload.mp3".to_string()
    } else {
        cleaned
    }
}
