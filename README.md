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
cp .env.example .env      # credentials and demucs tuning; not tracked
docker compose up --build
```

`.env` holds nothing container-specific, so the same file also configures
`cargo run` on a host. Container paths (`DATABASE_URL`, `WORK_DIR`, …) stay in
`docker-compose.yml`.

The image builds on both amd64 and arm64. arm64 costs an extra minute: demucs
depends on sphn, which publishes linux wheels for x86_64 only, so on arm64 it
compiles from source and the Dockerfile installs a toolchain for that step
alone.

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

Uploads are multipart and stay outside the encrypted JSON channel — see
[Encryption](#encryption).

### Poll

Reads are POSTs because they go through the E2EE middleware, which needs a body
to decrypt. With `E2EE_SHARED_KEY` set you can drive them from a script using
[`lazynton-js`](https://www.npmjs.com/package/lazynton-js); the raw endpoints
speak `nonce(24) || ciphertext` and reject anything else with a 401 or 400.

```ts
import { LazyntonClient } from 'lazynton-js';
const api = LazyntonClient.withSharedKey('http://localhost:8080', process.env.E2EE_SHARED_KEY);
await api.post('/api/jobs/get', { id: '3f2a...' });
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

`POST /api/jobs/search`, also encrypted:

```json
{ "status": "done", "q": "monday", "sort": "name", "favorite": true, "limit": 20, "offset": 0 }
```

`sort` is one of `newest` (default), `oldest`, `name`, `name_desc`; starred jobs
sort ahead of the rest either way. `q` matches the filename as a literal
substring — a `%` or `_` typed into the search box is not a wildcard. The
response carries `total` alongside the page so a pager can be drawn.

### Rename and star

```json
PATCH /api/jobs/3f2a...   { "filename": "New Order - Blue Monday.mp3", "favorite": true }
```

Either field alone is fine. A rename changes the display name and the download
filename only — the stored objects keep their original key, and demucs keeps
using the name the file was uploaded under.

The display name is not restricted to ASCII: `ลมหายใจ.mp3` stays itself, in the
job list, in search, and on the downloaded file. Only path separators, control
characters and quotes are stripped, and it is capped at 200 characters. That
works because the name travels in the JSON body and never becomes a storage key;
the one place it has to enter a header, the download's `Content-Disposition`, it
goes as RFC 5987 `filename*=UTF-8''…` with an ASCII `filename` fallback beside
it. The same now applies to uploads — a file called `ลมหายใจ.mp3` keeps its title
in the console while the object under it is stored under an ASCII key.

### Download

```bash
curl -L -O -J http://localhost:8080/api/jobs/3f2a.../download/vocals
```

Without `AUDIO_KEY`, this is a 307 to a presigned RustFS URL (15 min default) so
the bytes never pass through this process; `-L` is required. With `AUDIO_KEY`
set it streams the ciphertext back instead — see below.

Adding `?inline=true` streams the stem same-origin with an `inline` disposition
instead of redirecting. That is what the console's player uses: `fetch` against a
presigned URL on another host would need CORS on the bucket, and a cross-origin
`<audio>` element can't be routed through Web Audio at all.

## Player

Each finished job has a **Play** button that opens a small mixer: transport,
scrubber, and one row per stem with mute, solo and a volume fader.

All the stems are decoded up front and started on a single `AudioContext` clock,
so they stay sample-aligned — muting `vocals` leaves the rest exactly where it
was. Separate `<audio>` elements would drift apart within seconds, which is the
whole thing you separated the track to avoid. The cost is memory: every stem is
held as a decoded `AudioBuffer`, roughly 85 MB per four-minute stereo stem.

### Delete

```bash
curl -X DELETE http://localhost:8080/api/jobs/3f2a... --data '{}'
```

Removes the row and every object under `jobs/{id}/`. Refuses while running. The
body is not read but must be present: it goes through the E2EE middleware, and
an encrypted empty string is indistinguishable there from a failed decrypt.

## Encryption

Two separate layers, both optional, neither of them a substitute for TLS.

**The JSON API** goes through [lazynton](https://crates.io/crates/lazynton):
the console does an X25519 handshake at `/handshake`, then every request body
and successful response body is XChaCha20-Poly1305 over
`application/octet-stream`. The session key is persisted encrypted-at-rest in
the browser under a non-extractable WebCrypto key. `/healthz`, the multipart
upload and the download route stay in the clear — the first is a container
healthcheck, and the other two carry audio rather than JSON.

**The audio** is covered by `AUDIO_KEY` instead, in a chunked lazyxchacha format
(`src/crypto.rs`, mirrored in `web/src/lib/audio-crypto.ts`, with a test that
fails if the two ever disagree). The console seals a file before it leaves the
browser, RustFS only ever stores that ciphertext, and the worker unseals it just
long enough for demucs to run. Stems are sealed again before they go back up and
are decrypted in the browser, which is why they stream back through the service
when a key is set: fetching a presigned URL from JS would need CORS on the
bucket. Sealing costs 45 bytes per megabyte-sized chunk and nothing else, so
`MAX_UPLOAD_BYTES` still means what it says.

What this does and does not buy you, plainly:

- The bytes are unreadable to anyone holding the RustFS bucket, or watching the
  wire, without also holding the service's key.
- It is **not** end-to-end in the strict sense. demucs needs plaintext audio, so
  the service holds `AUDIO_KEY` and hands it to any client that completes a
  handshake. With no auth in front of the service, that means anyone who can
  reach it. The threat it actually addresses is a shared or untrusted object
  store, not a compromised service.
- Changing or removing `AUDIO_KEY` orphans everything uploaded under it. Turning
  it on for the first time is safe: objects are checked for the format marker,
  so jobs from before the switch still work.

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

- No auth. Put it behind a reverse proxy or add a middleware layer. This is also
  what keeps the audio encryption from being true E2EE — see above.
- With `AUDIO_KEY` set, stem downloads stream through the service instead of
  straight from RustFS, and the browser holds a whole stem in memory to decrypt
  it. Fine for a 40 MB stem; not a streaming design.
- Presigned URLs are signed against `S3_PUBLIC_ENDPOINT`, which has to be
  reachable from the *client*, not just from the app container. It falls back to
  `S3_ENDPOINT`, which is wrong the moment those two differ.
- The upload is buffered to local disk before going to RustFS. Fine for songs;
  swap to a multipart upload if you ever take hour-long files.
- `progress` depends on demucs' stderr format. If a future version changes the
  bar, progress stops moving — the job still completes normally.
