# demucs-service

Rust HTTP service that takes a song, stores it in RustFS, queues it, runs Demucs
`htdemucs` on CPU, and puts the separated stems back into RustFS.

```
POST /api/jobs ──► RustFS (jobs/{id}/input/...)
                   │
                   ├──► SQLite row: status=queued
                   │
                   └──► worker claims ──► download ──► demucs ──► upload stems
                                                                  │
GET /api/jobs      ◄──────────────────────────────────────────────┘
GET /api/jobs/{id}/download/{stem} ──► 307 to a presigned RustFS URL
```

## Why these pieces

**RustFS via `aws-sdk-s3`.** RustFS is S3-compatible, so no bespoke client is
needed — just point `endpoint_url` at it and turn on path-style addressing.

**SQLite as the queue.** The job table has to exist anyway to answer
`GET /api/jobs` with a status. Making it the queue too means one atomic
`UPDATE ... RETURNING` is the whole claim mechanism, with crash recovery falling
out of a single `UPDATE` at boot. If you later need retries with backoff, cron
schedules, or a dashboard, swap in [`apalis`](https://github.com/geofmureithi/apalis)
— its SQLite backend fits the same shape.

**One worker.** Demucs on CPU already uses every core for its matmuls. Running
two jobs at once on 4 cores makes both take roughly twice as long and doubles
peak RAM. Serialise, don't parallelise.

## Running

```bash
cp .env.example .env
docker compose up --build
```

RustFS console: http://localhost:9001 (`rustfsadmin` / `rustfsadmin`)

Running the binary outside Docker needs `demucs` on `PATH`:

```bash
pip install demucs
cargo run --release
```

## Console

A SvelteKit page in `web/` that wraps the same API: drag-and-drop upload with a
real progress bar, live job status, and one-click stem downloads.

It ships *inside* the binary. `adapter-static` prerenders it to `web/build`, and
`rust-embed` bakes that folder in at compile time, so the service serves the
console on the same port as the API — one process, one port, no extra volume:

```bash
cd web && bun install && bun run build   # must run before cargo build
cargo build --release                    # console is now in the binary
```

The Docker build does both stages itself, so `docker compose up --build` needs
no separate step.

For working on the console, `cd web && bun run dev` is still the loop —
port 5173, with `/api` and `/healthz` proxied to `127.0.0.1:8080`, so it's
same-origin either way and no CORS layer is needed. In debug builds rust-embed
reads `web/build` off disk, so a rebuilt console shows up without recompiling
Rust.

Download buttons hand the browser a presigned RustFS URL, which is signed
against `S3_PUBLIC_ENDPOINT` (defaults to `S3_ENDPOINT`). Set it whenever the
app and the browser reach RustFS at different addresses — e.g. `http://rustfs:9000`
inside a compose network vs `http://127.0.0.1:9000` from the desktop.

## API

### Submit

```bash
curl -F file=@song.mp3 http://localhost:8080/api/jobs
# optional: -F model=htdemucs_ft  -F two_stems=vocals
```

`202 Accepted`:

```json
{ "id": "3f2a...", "filename": "song.mp3", "status": "queued", "progress": 0, "stems": [] }
```

### Poll

```bash
curl http://localhost:8080/api/jobs/3f2a...
```

```json
{
  "id": "3f2a...",
  "status": "done",
  "progress": 100,
  "stems": [
    { "name": "bass",   "bytes": 4210332, "download_url": "/api/jobs/3f2a.../download/bass" },
    { "name": "drums",  "bytes": 5011290, "download_url": "/api/jobs/3f2a.../download/drums" },
    { "name": "other",  "bytes": 6120044, "download_url": "/api/jobs/3f2a.../download/other" },
    { "name": "vocals", "bytes": 4890111, "download_url": "/api/jobs/3f2a.../download/vocals" }
  ]
}
```

`progress` is scraped from the demucs progress bar on stderr, so it moves in
real time rather than jumping 0 → 100.

### List

```bash
curl "http://localhost:8080/api/jobs?status=done&limit=20"
```

### Download

```bash
curl -L -O -J http://localhost:8080/api/jobs/3f2a.../download/vocals
```

Returns a 307 to a presigned RustFS URL (15 min default), so file bytes never
pass through this process. `-L` is required.

### Delete

```bash
curl -X DELETE http://localhost:8080/api/jobs/3f2a...
```

Removes the row and every object under `jobs/{id}/`. Refuses while running.

## Tuning for a 4-core box

| Env | Default | Notes |
|---|---|---|
| `DEMUCS_MODEL` | `htdemucs` | `htdemucs_ft` is ~4× slower for a small quality gain — a bad trade on CPU |
| `DEMUCS_JOBS` | `1` | demucs `-j`. Leave at 1; torch is already multi-threaded |
| `DEMUCS_THREADS` | core count | Pins `OMP_NUM_THREADS` etc. so the pools don't oversubscribe |
| `DEMUCS_SEGMENT` | `7` | Seconds per chunk. Drop to `5` if RAM is under 8GB |
| `JOB_TIMEOUT_SECS` | `3600` | Kills a stuck run instead of blocking the queue forever |

Expect roughly 5–15 minutes per 4-minute track. Two of the four cores staying
idle usually means `DEMUCS_THREADS` didn't take.

## Known gaps

- No auth. Put it behind a reverse proxy or add a middleware layer.
- Presigned URLs are signed against `S3_PUBLIC_ENDPOINT`, which has to be
  reachable from the *client*, not just from the app container. It falls back to
  `S3_ENDPOINT`, which is wrong the moment those two differ.
- The upload is buffered to local disk before going to RustFS. Fine for songs;
  swap to a multipart upload if you ever take hour-long files.
- `progress` depends on demucs' stderr format. If a future version changes the
  bar, progress stops moving — the job still completes normally.
