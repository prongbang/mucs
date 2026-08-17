//! Chunked XChaCha20-Poly1305 for the audio path, through `lazyxchacha`.
//!
//! lazynton covers the JSON API and lazyxchacha covers the cipher, but
//! lazyxchacha seals a whole body at once — fine for a job list, hopeless for a
//! 256 MB upload. So the audio gets its own framing: a magic header followed by
//! independently sealed chunks, which streams in constant memory on both ends.
//!
//! Both ends use `encrypt_raw`/`decrypt_raw`, so nothing is ever text: the
//! sealed file is the audio plus 45 bytes per chunk.
//!
//! ```text
//! "mucsL2\0\0"                       8 bytes
//! per chunk, repeated:
//!     u32 BE  sealed length
//!     nonce (24) || ciphertext || tag  — exactly what lazyxchacha returns
//! ```
//!
//! lazyxchacha takes no associated data, so the chunk's position rides *inside*
//! the sealed plaintext, as `index (u64 BE) || is_last (u8)` ahead of the audio,
//! and is checked after opening. That keeps the original property: a reordered,
//! duplicated, dropped or truncated stream fails instead of silently decoding to
//! the wrong audio.

use anyhow::{bail, Context, Result};
use lazyxchacha::lazyxchacha::{Cryptography, LazyXChaCha};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};

pub const MAGIC: &[u8; 8] = b"mucsL2\0\0";
/// Plaintext bytes per chunk. Also the browser's chunk size — both sides must agree.
pub const CHUNK: usize = 1024 * 1024;
/// `index (u64 BE) || is_last (u8)`, sealed ahead of the audio in place of the
/// associated data lazyxchacha does not take.
const HEADER: usize = 9;
const NONCE: usize = 24;
const TAG: usize = 16;
/// Nonce, tag and header: a frame carrying no audio at all is still this long.
const MIN_FRAME: usize = NONCE + HEADER + TAG;
/// A chunk can never legitimately exceed this; guards against a corrupt length
/// header turning into a huge allocation.
const MAX_FRAME: usize = MIN_FRAME + CHUNK;

/// lazyxchacha slices the key without checking it, so a bad `AUDIO_KEY` has to
/// be caught here or it panics inside the library. Returns the concrete type
/// rather than `LazyXChaCha::new()`'s `Arc<dyn Cryptography>`, which is not
/// `Send` and so cannot cross the worker's `tokio::spawn`.
fn cipher(key_hex: &str) -> Result<LazyXChaCha> {
    let raw = hex::decode(key_hex).context("audio key is not hex")?;
    if raw.len() != 32 {
        bail!("audio key must be 32 bytes ({} given)", raw.len());
    }
    Ok(LazyXChaCha {})
}

/// The header lazyxchacha's missing associated data is replaced by, prepended to
/// the audio so the whole thing is sealed as one plaintext.
fn frame(index: u64, last: bool, plain: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(HEADER + plain.len());
    framed.extend_from_slice(&index.to_be_bytes());
    framed.push(last as u8);
    framed.extend_from_slice(plain);
    framed
}

fn unframe(opened: Vec<u8>, index: u64) -> Result<(Vec<u8>, bool)> {
    if opened.len() < HEADER {
        bail!("chunk {index} is missing its header");
    }

    let sealed_index = u64::from_be_bytes(opened[..8].try_into()?);
    if sealed_index != index {
        bail!("chunk {index} was sealed as chunk {sealed_index} — the stream is out of order");
    }
    let last = match opened[8] {
        0 => false,
        1 => true,
        other => bail!("chunk {index} has a bogus last-flag {other}"),
    };

    let mut plain = opened;
    plain.drain(..HEADER);
    Ok((plain, last))
}

/// True if `path` starts with the chunked-format magic. Objects written before
/// the audio key was turned on are plaintext and must be passed through as-is.
pub async fn is_encrypted(path: &Path) -> bool {
    let mut head = [0u8; MAGIC.len()];
    match tokio::fs::File::open(path).await {
        Ok(mut f) => f.read_exact(&mut head).await.is_ok() && &head == MAGIC,
        Err(_) => false,
    }
}

pub async fn encrypt_file(src: &Path, dst: &Path, key_hex: &str) -> Result<()> {
    let cipher = cipher(key_hex)?;
    let mut reader = BufReader::new(tokio::fs::File::open(src).await?);
    let mut writer = BufWriter::new(tokio::fs::File::create(dst).await?);
    writer.write_all(MAGIC).await?;

    let mut buf = vec![0u8; CHUNK];
    let mut index: u64 = 0;
    // One chunk of lookahead: the last chunk carries a different header, and we
    // only know a chunk is last once the read after it comes back empty.
    let mut pending: Option<Vec<u8>> = None;

    loop {
        let n = read_full(&mut reader, &mut buf).await?;
        let chunk = (n > 0).then(|| buf[..n].to_vec());

        if let Some(prev) = pending.take() {
            write_chunk(&mut writer, &cipher, key_hex, index, chunk.is_none(), &prev).await?;
            index += 1;
        }

        match chunk {
            Some(c) => pending = Some(c),
            None => break,
        }
    }

    // An empty input still gets one (empty) final chunk, so decrypting it is a
    // normal success rather than a truncation error.
    if index == 0 {
        write_chunk(&mut writer, &cipher, key_hex, 0, true, &[]).await?;
    }

    writer.flush().await?;
    Ok(())
}

async fn write_chunk<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    cipher: &LazyXChaCha,
    key_hex: &str,
    index: u64,
    last: bool,
    plain: &[u8],
) -> Result<()> {
    // lazyxchacha swallows its errors and returns an empty buffer.
    let sealed = cipher.encrypt_raw(&frame(index, last, plain), key_hex);
    if sealed.is_empty() {
        bail!("chunk {index} failed to encrypt");
    }

    writer.write_all(&(sealed.len() as u32).to_be_bytes()).await?;
    writer.write_all(&sealed).await?;
    Ok(())
}

pub async fn decrypt_file(src: &Path, dst: &Path, key_hex: &str) -> Result<()> {
    let cipher = cipher(key_hex)?;
    let mut reader = BufReader::new(tokio::fs::File::open(src).await?);

    let mut magic = [0u8; MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .await
        .context("input is too short to be encrypted audio")?;
    if &magic != MAGIC {
        bail!("input is not in the mucs encrypted-audio format");
    }

    let mut writer = BufWriter::new(tokio::fs::File::create(dst).await?);
    let mut index: u64 = 0;

    loop {
        let mut len = [0u8; 4];
        match reader.read_exact(&mut len).await {
            Ok(_) => {}
            // Ran out of frames without ever seeing the one marked last.
            Err(_) => bail!("encrypted audio is truncated after {index} chunks"),
        }

        let len = u32::from_be_bytes(len) as usize;
        if !(MIN_FRAME..=MAX_FRAME).contains(&len) {
            bail!("chunk {index} declares an implausible length of {len}");
        }

        let mut sealed = vec![0u8; len];
        reader.read_exact(&mut sealed).await?;

        // Empty is how lazyxchacha reports a failed open, and a genuine chunk is
        // never empty — it always carries its 9-byte header.
        let opened = cipher.decrypt_raw(&sealed, key_hex);
        if opened.is_empty() {
            bail!("chunk {index} failed to authenticate");
        }
        let (plain, last) = unframe(opened, index)?;

        writer.write_all(&plain).await?;
        if last {
            break;
        }
        index += 1;
    }

    writer.flush().await?;
    Ok(())
}

/// `read` is allowed to return short reads; a partial chunk would change the
/// framing, so fill the buffer unless the file genuinely ended.
async fn read_full<R: AsyncReadExt + Unpin>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "3d1f8e5c0a7b46293d1f8e5c0a7b46293d1f8e5c0a7b46293d1f8e5c0a7b4629";

    async fn roundtrip(bytes: usize) -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mucs-crypto-{bytes}-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await?;
        let (plain, sealed, back) = (dir.join("a"), dir.join("b"), dir.join("c"));

        // Varying bytes, so a chunk swap can't accidentally still match.
        let data: Vec<u8> = (0..bytes).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&plain, &data).await?;

        encrypt_file(&plain, &sealed, KEY).await?;
        assert!(is_encrypted(&sealed).await);
        assert!(!is_encrypted(&plain).await || bytes == 0);

        decrypt_file(&sealed, &back, KEY).await?;
        assert_eq!(tokio::fs::read(&back).await?, data, "{bytes} bytes");

        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(())
    }

    #[tokio::test]
    async fn roundtrips_across_chunk_boundaries() -> Result<()> {
        for size in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, CHUNK * 2 + 7] {
            roundtrip(size).await?;
        }
        Ok(())
    }

    /// The console encrypts uploads and decrypts stems, so its TypeScript has to
    /// produce byte-for-byte what this module reads — and vice versa. Nothing
    /// else in the test suite would catch the two drifting apart.
    #[tokio::test]
    async fn matches_the_browser_implementation() -> Result<()> {
        let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src/lib/audio-crypto.ts");
        if std::process::Command::new("bun").arg("--version").output().is_err() {
            eprintln!("skipping cross-language check: bun is not installed");
            return Ok(());
        }

        let dir = std::env::temp_dir().join(format!("mucs-cross-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await?;
        let data: Vec<u8> = (0..CHUNK * 2 + 4242).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(dir.join("plain"), &data).await?;

        // Round trip in both directions: bun seals what Rust opens, and opens
        // what Rust sealed.
        encrypt_file(&dir.join("plain"), &dir.join("rust.enc"), KEY).await?;
        // `isSealed` is checked here too: the stem player routes every download
        // through it, so a magic it disagrees with means ciphertext reaches the
        // audio decoder and the only symptom is "unable to decode audio data".
        let script = format!(
            r#"
            import {{ encryptFile, decryptBytes, isSealed }} from {web:?};
            const dir = {dir:?};
            const plain = new Blob([await Bun.file(dir + "/plain").arrayBuffer()]);
            const sealed = await encryptFile(plain, {KEY:?});
            await Bun.write(dir + "/bun.enc", sealed);
            const fromRust = await Bun.file(dir + "/rust.enc").arrayBuffer();
            if (!isSealed(fromRust)) throw new Error("isSealed rejected the server's own output");
            if (isSealed(await plain.arrayBuffer())) throw new Error("isSealed accepted plaintext");
            const opened = await decryptBytes(fromRust, {KEY:?});
            await Bun.write(dir + "/bun.out", opened);
            "#
        );
        tokio::fs::write(dir.join("run.ts"), script).await?;

        let out = std::process::Command::new("bun")
            .arg("run")
            .arg(dir.join("run.ts"))
            .output()?;
        assert!(
            out.status.success(),
            "bun failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        assert_eq!(
            tokio::fs::read(dir.join("bun.out")).await?,
            data,
            "the browser could not decrypt what the server encrypted"
        );

        decrypt_file(&dir.join("bun.enc"), &dir.join("rust.out"), KEY).await?;
        assert_eq!(
            tokio::fs::read(dir.join("rust.out")).await?,
            data,
            "the server could not decrypt what the browser encrypted"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_tampering_truncation_and_wrong_keys() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("mucs-tamper-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await?;
        let (plain, sealed, back) = (dir.join("a"), dir.join("b"), dir.join("c"));
        tokio::fs::write(&plain, vec![7u8; CHUNK * 2 + 100]).await?;
        encrypt_file(&plain, &sealed, KEY).await?;

        let good = tokio::fs::read(&sealed).await?;

        let mut flipped = good.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        tokio::fs::write(&sealed, &flipped).await?;
        assert!(decrypt_file(&sealed, &back, KEY).await.is_err(), "flipped bit");

        // Dropping the final chunk must not read as a shorter, valid file.
        tokio::fs::write(&sealed, &good[..good.len() / 2]).await?;
        assert!(decrypt_file(&sealed, &back, KEY).await.is_err(), "truncated");

        // lazyxchacha has no associated data, so ordering rests entirely on the
        // header sealed inside each chunk. Swap two same-sized frames to prove it.
        let mut swapped = good.clone();
        let body = MAGIC.len() + 4;
        let frame = u32::from_be_bytes(good[MAGIC.len()..body].try_into()?) as usize;
        let (a, b) = (body, body + frame + 4);
        for i in 0..frame {
            swapped.swap(a + i, b + i);
        }
        tokio::fs::write(&sealed, &swapped).await?;
        assert!(decrypt_file(&sealed, &back, KEY).await.is_err(), "reordered");

        tokio::fs::write(&sealed, &good).await?;
        let other = "0".repeat(64);
        assert!(decrypt_file(&sealed, &back, &other).await.is_err(), "wrong key");

        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(())
    }
}
