use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub work_dir: PathBuf,
    pub max_upload_bytes: usize,

    // ---- RustFS (S3) ----
    pub s3_endpoint: String,
    /// Host to sign download URLs against. Inside Docker `s3_endpoint` is
    /// `http://rustfs:9000`, which a browser can't resolve — set this to the
    /// address the *client* sees. Defaults to `s3_endpoint`.
    pub s3_public_endpoint: String,
    pub s3_region: String,
    pub s3_bucket: String,
    pub s3_access_key: String,
    pub s3_secret_key: String,
    pub presign_secs: u64,

    // ---- Demucs ----
    pub demucs_bin: String,
    pub demucs_model: String,
    /// Demucs `-j`. Keep at 1 on a small CPU box: torch already uses every core
    /// for the matmuls, so `-j 4` on 4 cores just oversubscribes and multiplies RAM.
    pub demucs_jobs: u32,
    /// Demucs `--segment` in seconds. htdemucs caps around 7.8s. Lower = less RAM.
    pub demucs_segment: u32,
    /// Threads handed to torch/OMP. Default = all cores.
    pub demucs_threads: u32,
    /// Output format: "mp3" or "wav".
    pub demucs_format: String,
    /// Hard ceiling on a single separation run.
    pub job_timeout_secs: u64,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4);

        Ok(Self {
            bind_addr: env_or("BIND_ADDR", "0.0.0.0:8080"),
            database_url: env_or("DATABASE_URL", "sqlite://data/jobs.db?mode=rwc"),
            work_dir: PathBuf::from(env_or("WORK_DIR", "data/work")),
            max_upload_bytes: env_parse("MAX_UPLOAD_BYTES", 256 * 1024 * 1024),

            s3_endpoint: env_or("S3_ENDPOINT", "http://127.0.0.1:9000"),
            s3_public_endpoint: env_or(
                "S3_PUBLIC_ENDPOINT",
                &env_or("S3_ENDPOINT", "http://127.0.0.1:9000"),
            ),
            s3_region: env_or("S3_REGION", "us-east-1"),
            s3_bucket: env_or("S3_BUCKET", "demucs"),
            s3_access_key: env_or("S3_ACCESS_KEY", "rustfsadmin"),
            s3_secret_key: env_or("S3_SECRET_KEY", "rustfsadmin"),
            presign_secs: env_parse("PRESIGN_SECS", 900),

            demucs_bin: env_or("DEMUCS_BIN", "demucs"),
            demucs_model: env_or("DEMUCS_MODEL", "htdemucs"),
            demucs_jobs: env_parse("DEMUCS_JOBS", 1),
            demucs_segment: env_parse("DEMUCS_SEGMENT", 7),
            demucs_threads: env_parse("DEMUCS_THREADS", cores),
            demucs_format: env_or("DEMUCS_FORMAT", "mp3"),
            job_timeout_secs: env_parse("JOB_TIMEOUT_SECS", 3600),
        })
    }
}
