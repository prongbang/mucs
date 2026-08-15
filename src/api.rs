use std::sync::Arc;

use axum::extract::{Multipart, Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::config::Config;
use crate::db::{Job, Stem, Store, STATUS_DONE};
use crate::error::{AppError, AppResult};
use crate::storage::Storage;
use crate::worker::sanitize;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Store,
    pub storage: Storage,
    pub notify: Arc<Notify>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/jobs", post(create_job).get(list_jobs))
        .route("/api/jobs/{id}", get(get_job).delete(delete_job))
        .route("/api/jobs/{id}/download/{stem}", get(download_stem))
        .with_state(state)
        // Anything that isn't the API is the console.
        .fallback(crate::web::handler)
}

// ---------------------------------------------------------------- responses

#[derive(Serialize)]
pub struct JobView {
    pub id: String,
    pub filename: String,
    pub status: String,
    pub progress: i64,
    pub model: String,
    pub two_stems: Option<String>,
    pub stems: Vec<StemView>,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Serialize)]
pub struct StemView {
    pub name: String,
    pub bytes: i64,
    pub download_url: String,
}

impl JobView {
    fn from(job: Job) -> Self {
        let stems: Vec<Stem> = job
            .stems
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let stem_views = stems
            .into_iter()
            .map(|s| StemView {
                bytes: s.bytes,
                download_url: format!("/api/jobs/{}/download/{}", job.id, s.name),
                name: s.name,
            })
            .collect();

        Self {
            id: job.id,
            filename: job.filename,
            status: job.status,
            progress: job.progress,
            model: job.model,
            two_stems: job.two_stems,
            stems: stem_views,
            error: job.error,
            created_at: job.created_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
        }
    }
}

// ---------------------------------------------------------------- handlers

async fn healthz(State(st): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let depth = st.store.queue_depth().await.map_err(AppError::Other)?;
    Ok(Json(json!({ "ok": true, "queued": depth })))
}

/// `POST /api/jobs` — multipart form:
///   file       (required) the audio file
///   model      (optional) demucs model name, defaults to config
///   two_stems  (optional) e.g. "vocals" for a vocals/no_vocals split
async fn create_job(
    State(st): State<AppState>,
    mut multipart: Multipart,
) -> AppResult<(StatusCode, Json<JobView>)> {
    let id = uuid::Uuid::new_v4().to_string();
    let scratch = st.cfg.work_dir.join(format!("upload-{id}"));
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|e| AppError::Other(e.into()))?;

    let mut filename: Option<String> = None;
    let mut temp_path: Option<std::path::PathBuf> = None;
    let mut model = st.cfg.demucs_model.clone();
    let mut two_stems: Option<String> = None;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("bad multipart body: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                let name = sanitize(field.file_name().unwrap_or("upload.mp3"));
                let dest = scratch.join(&name);

                // Stream to disk instead of buffering the whole song in memory.
                let mut file = tokio::fs::File::create(&dest)
                    .await
                    .map_err(|e| AppError::Other(e.into()))?;

                let mut written: usize = 0;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("upload aborted: {e}")))?
                {
                    written += chunk.len();
                    if written > st.cfg.max_upload_bytes {
                        let _ = tokio::fs::remove_dir_all(&scratch).await;
                        return Err(AppError::BadRequest(format!(
                            "file exceeds the {} byte limit",
                            st.cfg.max_upload_bytes
                        )));
                    }
                    file.write_all(&chunk)
                        .await
                        .map_err(|e| AppError::Other(e.into()))?;
                }
                file.flush().await.map_err(|e| AppError::Other(e.into()))?;

                filename = Some(name);
                temp_path = Some(dest);
            }
            "model" => {
                let v = field.text().await.unwrap_or_default();
                if !v.trim().is_empty() {
                    model = v.trim().to_string();
                }
            }
            "two_stems" => {
                let v = field.text().await.unwrap_or_default();
                if !v.trim().is_empty() {
                    two_stems = Some(v.trim().to_string());
                }
            }
            _ => {}
        }
    }

    let (filename, temp_path) = match (filename, temp_path) {
        (Some(f), Some(p)) => (f, p),
        _ => {
            let _ = tokio::fs::remove_dir_all(&scratch).await;
            return Err(AppError::BadRequest("missing `file` field".into()));
        }
    };

    // Upload first, insert second: if the upload fails there's no orphan row
    // pointing at a key that doesn't exist.
    let input_key = format!("jobs/{id}/input/{filename}");
    let put = st.storage.put_file(&input_key, &temp_path).await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    put.map_err(AppError::Other)?;

    let job = st
        .store
        .insert(&id, &filename, &input_key, &model, two_stems.as_deref())
        .await
        .map_err(AppError::Other)?;

    // Nudge the worker so it doesn't sit out its poll interval.
    st.notify.notify_one();

    Ok((StatusCode::ACCEPTED, Json(JobView::from(job))))
}

#[derive(Deserialize)]
pub struct ListParams {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

async fn list_jobs(
    State(st): State<AppState>,
    Query(p): Query<ListParams>,
) -> AppResult<Json<serde_json::Value>> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let offset = p.offset.unwrap_or(0).max(0);

    let jobs = st
        .store
        .list(p.status.as_deref(), limit, offset)
        .await
        .map_err(AppError::Other)?;

    let views: Vec<JobView> = jobs.into_iter().map(JobView::from).collect();
    Ok(Json(json!({ "jobs": views, "limit": limit, "offset": offset })))
}

async fn get_job(State(st): State<AppState>, AxPath(id): AxPath<String>) -> AppResult<Json<JobView>> {
    let job = st
        .store
        .get(&id)
        .await
        .map_err(AppError::Other)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(JobView::from(job)))
}

/// Hands back a 307 to a presigned RustFS URL so the bytes never transit this
/// process. Falls back to nothing clever — if the job isn't done, say so.
async fn download_stem(
    State(st): State<AppState>,
    AxPath((id, stem_name)): AxPath<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let job = st
        .store
        .get(&id)
        .await
        .map_err(AppError::Other)?
        .ok_or(AppError::NotFound)?;

    if job.status != STATUS_DONE {
        return Err(AppError::BadRequest(format!(
            "job is `{}`, not ready for download",
            job.status
        )));
    }

    let stems: Vec<Stem> = job
        .stems
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let stem = stems
        .into_iter()
        .find(|s| s.name == stem_name)
        .ok_or(AppError::NotFound)?;

    let base = job
        .filename
        .rsplit_once('.')
        .map(|(b, _)| b.to_string())
        .unwrap_or(job.filename.clone());
    let ext = stem.key.rsplit('.').next().unwrap_or("mp3");
    let download_name = format!("{base} - {}.{ext}", stem.name);

    let url = st
        .storage
        .presign_get(&stem.key, &download_name)
        .await
        .map_err(AppError::Other)?;

    Ok(Redirect::temporary(&url))
}

async fn delete_job(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> AppResult<StatusCode> {
    let removed = st.store.delete(&id).await.map_err(AppError::Other)?;
    if !removed {
        return Err(AppError::BadRequest(
            "job not found, or still running".into(),
        ));
    }

    st.storage
        .delete_prefix(&format!("jobs/{id}/"))
        .await
        .map_err(AppError::Other)?;

    Ok(StatusCode::NO_CONTENT)
}
