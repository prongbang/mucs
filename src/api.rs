use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use lazynton::E2ee;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::config::Config;
use crate::db::{Job, ListQuery, Stem, Store, STATUS_DONE};
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
    let e2ee = E2ee::new(state.cfg.e2ee_shared_key.clone());

    // lazynton decrypts the request body and encrypts the response, so every
    // route behind it must carry a body — that's why the reads are POSTs.
    let encrypted = Router::new()
        .route("/api/jobs/search", post(list_jobs))
        .route("/api/jobs/get", post(get_job))
        .route("/api/audio-key", post(audio_key))
        .route("/api/jobs/{id}", patch(patch_job).delete(delete_job))
        .layer(axum::middleware::from_fn_with_state(
            e2ee.clone(),
            lazynton::middleware,
        ))
        .with_state(state.clone());

    Router::new()
        // Plaintext by necessity: the healthcheck has no client library, the
        // upload is 256 MB of multipart, and the download is a redirect to
        // RustFS. The audio on those two paths carries its own encryption.
        .route("/healthz", get(healthz))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}/download/{stem}", get(download_stem))
        .with_state(state)
        .merge(encrypted)
        .merge(e2ee.handshake_router("/handshake"))
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
    pub favorite: bool,
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
            favorite: job.favorite,
            created_at: job.created_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
        }
    }
}

// ------------------------------------------------------------------- naming

/// Cleans a user-supplied display name while leaving its script alone. This is
/// not [`sanitize`] — that one produces storage keys and demucs input paths and
/// has to stay ASCII. Returns `None` if nothing usable is left.
pub fn clean_display_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);

    let cleaned: String = base
        // Control characters would break the header this ends up in, and a
        // quote would end the quoted string early.
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .collect();

    // Long enough for any real title, short enough that it can't bloat a header.
    let cleaned: String = cleaned.trim().trim_matches('.').chars().take(200).collect();
    let cleaned = cleaned.trim().to_string();

    (!cleaned.is_empty()).then_some(cleaned)
}

/// A header value can't hold raw non-ASCII, so a Thai title has to travel
/// percent-encoded in `filename*` (RFC 5987). `filename` keeps an ASCII
/// approximation for anything that doesn't read the starred form.
pub fn content_disposition(name: &str) -> String {
    // attr-char from RFC 5987 — everything else gets percent-encoded.
    const SAFE: &[u8] = b"!#$&+-.^_`|~";
    let mut encoded = String::new();
    for b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || SAFE.contains(b) {
            encoded.push(*b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }

    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        sanitize(name),
        encoded
    )
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
    // What the console shows. Kept apart from the storage name so an upload
    // called `ลมหายใจ.mp3` reads as itself in the job list.
    let mut display: Option<String> = None;
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
                let raw = field.file_name().unwrap_or("upload.mp3").to_string();
                // The stored key and demucs' input path stay ASCII; only the
                // display name keeps the original script.
                let name = sanitize(&raw);
                display = clean_display_name(&raw);
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
        .insert(
            &id,
            display.as_deref().unwrap_or(&filename),
            &input_key,
            &model,
            two_stems.as_deref(),
        )
        .await
        .map_err(AppError::Other)?;

    // Nudge the worker so it doesn't sit out its poll interval.
    st.notify.notify_one();

    Ok((StatusCode::ACCEPTED, Json(JobView::from(job))))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ListParams {
    pub status: Option<String>,
    /// Substring match on the filename.
    pub q: Option<String>,
    /// `newest` (default) · `oldest` · `name` · `name_desc`
    pub sort: Option<String>,
    pub favorite: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// `POST /api/jobs/search` — a POST because it goes through the E2EE
/// middleware, which has no encrypted body to read on a GET.
async fn list_jobs(
    State(st): State<AppState>,
    Json(p): Json<ListParams>,
) -> AppResult<Json<serde_json::Value>> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let offset = p.offset.unwrap_or(0).max(0);

    let (jobs, total) = st
        .store
        .list(&ListQuery {
            status: p.status.as_deref(),
            search: p.q.as_deref(),
            favorites_only: p.favorite.unwrap_or(false),
            sort: p.sort.as_deref().unwrap_or("newest"),
            limit,
            offset,
        })
        .await
        .map_err(AppError::Other)?;

    let views: Vec<JobView> = jobs.into_iter().map(JobView::from).collect();
    Ok(Json(
        json!({ "jobs": views, "total": total, "limit": limit, "offset": offset }),
    ))
}

#[derive(Deserialize)]
pub struct JobRef {
    pub id: String,
}

/// `POST /api/jobs/get` with `{"id": "…"}` — see [`list_jobs`] for why it's a POST.
async fn get_job(State(st): State<AppState>, Json(r): Json<JobRef>) -> AppResult<Json<JobView>> {
    let job = st
        .store
        .get(&r.id)
        .await
        .map_err(AppError::Other)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(JobView::from(job)))
}

/// The audio key, handed out only over the encrypted channel. It is the same
/// key for every client — the service has to hold it anyway, because demucs
/// needs plaintext — so this protects the bytes on the wire and inside RustFS,
/// not from the service itself.
async fn audio_key(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "key": st.cfg.audio_key, "chunk": crate::crypto::CHUNK }))
}

/// Hands back a 307 to a presigned RustFS URL so the bytes never transit this
/// process. Falls back to nothing clever — if the job isn't done, say so.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DownloadParams {
    /// Play it rather than save it: streams same-origin so the console can
    /// `fetch` and decode the bytes. Spelt `?inline=true` — a query string
    /// carries no types, so serde won't take `1` for a bool.
    pub inline: bool,
}

async fn download_stem(
    State(st): State<AppState>,
    AxPath((id, stem_name)): AxPath<(String, String)>,
    Query(p): Query<DownloadParams>,
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

    // Two reasons to stream through this process instead of redirecting:
    // an encrypted stem has to be decrypted in the browser, and the player has
    // to read the bytes with `fetch` to decode them. Both would need CORS on the
    // bucket to work against a presigned URL. Whatever passes through here is
    // still ciphertext whenever a key is set.
    if st.cfg.audio_key.is_some() || p.inline {
        let stream = st
            .storage
            .get_stream(&stem.key)
            .await
            .map_err(AppError::Other)?;

        let content_type = match (st.cfg.audio_key.is_some(), ext) {
            (true, _) => "application/octet-stream",
            (false, "wav") => "audio/wav",
            (false, "flac") => "audio/flac",
            (false, _) => "audio/mpeg",
        };

        // `inline` is the player asking to play it, not the user asking to keep it.
        let disposition = if p.inline {
            "inline".to_string()
        } else {
            content_disposition(&download_name)
        };

        return Ok((
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::CONTENT_DISPOSITION, disposition),
            ],
            Body::from_stream(tokio_util::io::ReaderStream::new(stream.into_async_read())),
        )
            .into_response());
    }

    let url = st
        .storage
        .presign_get(&stem.key, &content_disposition(&download_name))
        .await
        .map_err(AppError::Other)?;

    Ok(Redirect::temporary(&url).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_keep_their_script() {
        // The case this exists for: a Thai title used to come back as `_______`.
        assert_eq!(
            clean_display_name("ลมหายใจ.mp3").as_deref(),
            Some("ลมหายใจ.mp3")
        );
        assert_eq!(
            clean_display_name("Björk — Jóga.flac").as_deref(),
            Some("Björk — Jóga.flac")
        );

        // Still not a path, and still can't break out of the header.
        assert_eq!(
            clean_display_name("../../etc/ลับ.mp3").as_deref(),
            Some("ลับ.mp3")
        );
        // A name carrying a slash is a path: only the last segment survives, so
        // this keeps nothing of the injected part.
        assert_eq!(clean_display_name("a\"; rm -rf /\".mp3").as_deref(), Some("mp3"));
        assert_eq!(clean_display_name("bad\r\nname.mp3").as_deref(), Some("badname.mp3"));

        // Nothing usable left.
        assert_eq!(clean_display_name("   "), None);
        assert_eq!(clean_display_name("///"), None);
        assert_eq!(clean_display_name(""), None);

        assert_eq!(clean_display_name(&"ก".repeat(400)).unwrap().chars().count(), 200);
    }

    #[test]
    fn thai_names_survive_the_download_header() {
        let d = content_disposition("ลม - vocals.mp3");

        // A header value has to be ASCII whatever the name was.
        assert!(d.is_ascii(), "{d}");
        // The starred form carries the real name; the plain one is the fallback.
        assert!(d.contains("filename*=UTF-8''"), "{d}");
        assert!(d.contains("%E0%B8%A5%E0%B8%A1"), "{d}"); // ลม
        assert!(d.contains("filename=\"__ - vocals.mp3\""), "{d}");

        // ASCII names come through unmangled in both forms.
        let plain = content_disposition("song - bass.mp3");
        assert!(plain.contains("filename=\"song - bass.mp3\""), "{plain}");
        assert!(plain.contains("filename*=UTF-8''song%20-%20bass.mp3"), "{plain}");
    }
}

#[derive(Deserialize)]
pub struct PatchBody {
    pub filename: Option<String>,
    pub favorite: Option<bool>,
}

/// Rename and/or star a job. A rename changes the display name and the
/// download filename; the stored objects don't move.
async fn patch_job(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(body): Json<PatchBody>,
) -> AppResult<Json<JobView>> {
    // Unlike an upload, this name never becomes a storage key or a path handed
    // to demucs — it is only ever displayed and echoed back in JSON. So it keeps
    // its own script; `สมชาย - ลมหายใจ.mp3` stays that instead of being flattened
    // to underscores. Separators and controls still go, and the header that
    // carries it on the download route is RFC 5987 encoded.
    let filename = match body.filename {
        Some(f) => match clean_display_name(&f) {
            Some(name) => Some(name),
            None => return Err(AppError::BadRequest("filename is empty".into())),
        },
        None => None,
    };

    if filename.is_none() && body.favorite.is_none() {
        return Err(AppError::BadRequest("nothing to update".into()));
    }

    let job = st
        .store
        .patch(&id, filename.as_deref(), body.favorite)
        .await
        .map_err(AppError::Other)?
        .ok_or(AppError::NotFound)?;

    Ok(Json(JobView::from(job)))
}

/// Returns a JSON body rather than 204: the E2EE middleware encrypts the
/// response, and a 204 is not allowed to carry one.
async fn delete_job(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> AppResult<Json<serde_json::Value>> {
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

    Ok(Json(json!({ "deleted": id })))
}
