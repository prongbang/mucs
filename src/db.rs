use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::str::FromStr;

pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stem {
    pub name: String,
    pub key: String,
    pub bytes: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Job {
    pub id: String,
    pub filename: String,
    pub input_key: String,
    pub status: String,
    pub progress: i64,
    pub model: String,
    pub two_stems: Option<String>,
    /// JSON array of `Stem`, filled in once the job succeeds.
    pub stems: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            // WAL so the API can read while the worker writes progress.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10));

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS jobs (
                id          TEXT PRIMARY KEY,
                filename    TEXT NOT NULL,
                input_key   TEXT NOT NULL,
                status      TEXT NOT NULL,
                progress    INTEGER NOT NULL DEFAULT 0,
                model       TEXT NOT NULL,
                two_stems   TEXT,
                stems       TEXT,
                error       TEXT,
                created_at  TEXT NOT NULL,
                started_at  TEXT,
                finished_at TEXT
            );
            "#,
        )
        .execute(&pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status, created_at);")
            .execute(&pool)
            .await?;

        Ok(Self { pool })
    }

    /// Anything left `running` belongs to a process that died. Put it back in line.
    pub async fn requeue_orphans(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE jobs SET status = ?1, progress = 0, started_at = NULL WHERE status = ?2",
        )
        .bind(STATUS_QUEUED)
        .bind(STATUS_RUNNING)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn insert(
        &self,
        id: &str,
        filename: &str,
        input_key: &str,
        model: &str,
        two_stems: Option<&str>,
    ) -> Result<Job> {
        let job = sqlx::query_as::<_, Job>(
            r#"
            INSERT INTO jobs (id, filename, input_key, status, progress, model, two_stems, created_at)
            VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)
            RETURNING *
            "#,
        )
        .bind(id)
        .bind(filename)
        .bind(input_key)
        .bind(STATUS_QUEUED)
        .bind(model)
        .bind(two_stems)
        .bind(now())
        .fetch_one(&self.pool)
        .await?;
        Ok(job)
    }

    /// Atomically take the oldest queued job. Single UPDATE ... RETURNING, so two
    /// workers can never grab the same row.
    pub async fn claim_next(&self) -> Result<Option<Job>> {
        let job = sqlx::query_as::<_, Job>(
            r#"
            UPDATE jobs
               SET status = ?1, started_at = ?2, progress = 0, error = NULL
             WHERE id = (
                   SELECT id FROM jobs
                    WHERE status = ?3
                 ORDER BY created_at ASC
                    LIMIT 1
             )
            RETURNING *
            "#,
        )
        .bind(STATUS_RUNNING)
        .bind(now())
        .bind(STATUS_QUEUED)
        .fetch_optional(&self.pool)
        .await?;
        Ok(job)
    }

    pub async fn set_progress(&self, id: &str, progress: i64) -> Result<()> {
        sqlx::query("UPDATE jobs SET progress = ?1 WHERE id = ?2")
            .bind(progress.clamp(0, 100))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_done(&self, id: &str, stems: &[Stem]) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET status = ?1, progress = 100, stems = ?2, finished_at = ?3 WHERE id = ?4",
        )
        .bind(STATUS_DONE)
        .bind(serde_json::to_string(stems)?)
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_failed(&self, id: &str, err: &str) -> Result<()> {
        // Keep the tail of the error: demucs tracebacks are long and the useful
        // part is always at the bottom.
        let trimmed: String = if err.len() > 4000 {
            err[err.len() - 4000..].to_string()
        } else {
            err.to_string()
        };

        sqlx::query("UPDATE jobs SET status = ?1, error = ?2, finished_at = ?3 WHERE id = ?4")
            .bind(STATUS_FAILED)
            .bind(trimmed)
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Job>> {
        let job = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(job)
    }

    pub async fn list(&self, status: Option<&str>, limit: i64, offset: i64) -> Result<Vec<Job>> {
        let jobs = match status {
            Some(s) => {
                sqlx::query_as::<_, Job>(
                    "SELECT * FROM jobs WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2 OFFSET ?3",
                )
                .bind(s)
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as::<_, Job>(
                    "SELECT * FROM jobs ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(jobs)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM jobs WHERE id = ?1 AND status != ?2")
            .bind(id)
            .bind(STATUS_RUNNING)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn queue_depth(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE status = ?1")
            .bind(STATUS_QUEUED)
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }
}
