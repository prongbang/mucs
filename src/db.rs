use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
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
    pub favorite: bool,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// Everything the jobs list can be narrowed or ordered by.
#[derive(Default)]
pub struct ListQuery<'a> {
    pub status: Option<&'a str>,
    pub search: Option<&'a str>,
    pub favorites_only: bool,
    pub sort: &'a str,
    pub limit: i64,
    pub offset: i64,
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

/// Whitelist: this is the one place a client string reaches SQL unbound.
fn order_by(sort: &str) -> &'static str {
    match sort {
        "oldest" => "created_at ASC",
        "name" => "filename COLLATE NOCASE ASC",
        "name_desc" => "filename COLLATE NOCASE DESC",
        _ => "created_at DESC",
    }
}

fn push_filters<'a>(qb: &mut QueryBuilder<'a, Sqlite>, p: &ListQuery<'a>) {
    qb.push(" WHERE 1 = 1");

    if let Some(status) = p.status {
        qb.push(" AND status = ").push_bind(status);
    }

    if let Some(term) = p.search.map(str::trim).filter(|t| !t.is_empty()) {
        // The wildcards are ours, not the user's.
        let escaped = term.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_");
        qb.push(" AND filename LIKE ")
            .push_bind(format!("%{escaped}%"))
            .push(r" ESCAPE '\'");
    }

    if p.favorites_only {
        qb.push(" AND favorite = 1");
    }
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
                favorite    INTEGER NOT NULL DEFAULT 0,
                created_at  TEXT NOT NULL,
                started_at  TEXT,
                finished_at TEXT
            );
            "#,
        )
        .execute(&pool)
        .await?;

        // Databases created before favourites existed. Erroring here means the
        // column is already there, which is the whole point.
        let _ = sqlx::query("ALTER TABLE jobs ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0")
            .execute(&pool)
            .await;

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

    /// Rename and/or star. `None` leaves that field alone. A rename touches the
    /// display name only — `input_key` keeps pointing at the uploaded object.
    pub async fn patch(
        &self,
        id: &str,
        filename: Option<&str>,
        favorite: Option<bool>,
    ) -> Result<Option<Job>> {
        let job = sqlx::query_as::<_, Job>(
            r#"
            UPDATE jobs
               SET filename = COALESCE(?1, filename),
                   favorite = COALESCE(?2, favorite)
             WHERE id = ?3
            RETURNING *
            "#,
        )
        .bind(filename)
        .bind(favorite)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(job)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Job>> {
        let job = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(job)
    }

    /// One page of jobs, plus how many match the filters in total so the console
    /// can draw the pager.
    pub async fn list(&self, p: &ListQuery<'_>) -> Result<(Vec<Job>, i64)> {
        let mut page = QueryBuilder::new("SELECT * FROM jobs");
        push_filters(&mut page, p);
        // Starred first regardless of sort — that's what starring is for.
        page.push(" ORDER BY favorite DESC, ")
            .push(order_by(p.sort))
            .push(" LIMIT ")
            .push_bind(p.limit)
            .push(" OFFSET ")
            .push_bind(p.offset);

        let jobs = page
            .build_query_as::<Job>()
            .fetch_all(&self.pool)
            .await?;

        let mut count = QueryBuilder::new("SELECT COUNT(*) FROM jobs");
        push_filters(&mut count, p);
        let (total,): (i64,) = count.build_query_as().fetch_one(&self.pool).await?;

        Ok((jobs, total))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(jobs: &[Job]) -> Vec<&str> {
        jobs.iter().map(|j| j.filename.as_str()).collect()
    }

    #[tokio::test]
    async fn search_sort_page_and_patch() -> Result<()> {
        let path = std::env::temp_dir().join(format!("mucs-{}.db", uuid::Uuid::new_v4()));
        let store = Store::connect(&format!("sqlite://{}?mode=rwc", path.display())).await?;

        for (i, name) in ["b.mp3", "a.mp3", "100%_x.mp3"].iter().enumerate() {
            store
                .insert(&format!("id{i}"), name, "key", "htdemucs", None)
                .await?;
        }
        store.patch("id0", None, Some(true)).await?;

        // A `%` typed into the search box is a literal, not "match everything".
        let (jobs, total) = store
            .list(&ListQuery {
                search: Some("100%"),
                limit: 10,
                ..Default::default()
            })
            .await?;
        assert_eq!((names(&jobs), total), (vec!["100%_x.mp3"], 1));

        // Starred first, then the requested order.
        let (jobs, total) = store
            .list(&ListQuery {
                sort: "name",
                limit: 10,
                ..Default::default()
            })
            .await?;
        assert_eq!((names(&jobs), total), (vec!["b.mp3", "100%_x.mp3", "a.mp3"], 3));

        // Last page is short, but `total` still counts every match.
        let (jobs, total) = store
            .list(&ListQuery {
                sort: "name",
                limit: 2,
                offset: 2,
                ..Default::default()
            })
            .await?;
        assert_eq!((names(&jobs), total), (vec!["a.mp3"], 3));

        let (jobs, total) = store
            .list(&ListQuery {
                favorites_only: true,
                limit: 10,
                ..Default::default()
            })
            .await?;
        assert_eq!((names(&jobs), total), (vec!["b.mp3"], 1));

        // A rename must not move the object the job points at.
        let job = store.patch("id1", Some("renamed.mp3"), None).await?.unwrap();
        assert_eq!((job.filename.as_str(), job.input_key.as_str()), ("renamed.mp3", "key"));
        assert!(!job.favorite, "patching the name must not clear the star");

        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}
