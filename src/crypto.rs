//! Chunked AES-256-GCM for the audio path.
//!
//! lazynton covers the JSON API, but its helpers hex-encode the whole body in
//! memory — fine for a job list, hopeless for a 256 MB upload. So the audio gets
//! its own framing: a magic header followed by independently sealed chunks, which
//! streams in constant memory on both ends and maps onto WebCrypto's AES-GCM in
//! the browser (hardware-accelerated; a JS XChaCha20 implementation is not).
//!
//! ```text
//! "mucsE1\0\0"                       8 bytes
//! per chunk, repeated:
//!     u32 BE  ciphertext length      (plaintext len + 16 byte tag)
//!     nonce                          12 bytes
//!     ciphertext || tag
//! ```
//!
//! Each chunk is sealed with `index (u64 BE) || is_last (u8)` as associated
//! data, so a reordered, duplicated, dropped or truncated stream fails to
//! authenticate instead of silently decoding to the wrong audio.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{bail, Context, Result};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};

pub const MAGIC: &[u8; 8] = b"mucsE1\0\0";
/// Plaintext bytes per chunk. Also the browser's chunk size — both sides must agree.
pub const CHUNK: usize = 1024 * 1024;
const NONCE: usize = 12;
const TAG: usize = 16;
/// A chunk can never legitimately exceed this; guards against a corrupt length
/// header turning into a huge allocation.
const MAX_FRAME: usize = CHUNK + TAG + 1024;

fn cipher(key_hex: &str) -> Result<Aes256Gcm> {
    let raw = hex::decode(key_hex).context("audio key is not hex")?;
    if raw.len() != 32 {
        bail!("audio key must be 32 bytes ({} given)", raw.len());
    }
    Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&raw)))
}

fn aad(index: u64, last: bool) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(&index.to_be_bytes());
    a[8] = last as u8;
    a
}

fn nonce_for(index: u64) -> [u8; NONCE] {
    // Deterministic counter nonce. Safe here because a key never encrypts two
    // different streams: each object is sealed once, and a re-upload writes a
    // new object under a new job id.
    let mut n = [0u8; NONCE];
    n[4..].copy_from_slice(&index.to_be_bytes());
    n
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
    // One chunk of lookahead: the last chunk is sealed with a different AAD, and
    // we only know a chunk is last once the read after it comes back empty.
    let mut pending: Option<Vec<u8>> = None;

    loop {
        let n = read_full(&mut reader, &mut buf).await?;
        let chunk = (n > 0).then(|| buf[..n].to_vec());

        if let Some(prev) = pending.take() {
            write_chunk(&mut writer, &cipher, index, chunk.is_none(), &prev).await?;
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
        write_chunk(&mut writer, &cipher, 0, true, &[]).await?;
    }

    writer.flush().await?;
    Ok(())
}

async fn write_chunk<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    cipher: &Aes256Gcm,
    index: u64,
    last: bool,
    plain: &[u8],
) -> Result<()> {
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce_for(index)),
            Payload {
                msg: plain,
                aad: &aad(index, last),
            },
        )
        .map_err(|_| anyhow::anyhow!("chunk {index} failed to encrypt"))?;

    writer.write_all(&(sealed.len() as u32).to_be_bytes()).await?;
    writer.write_all(&nonce_for(index)).await?;
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
        if len < TAG || len > MAX_FRAME {
            bail!("chunk {index} declares an implausible length of {len}");
        }

        let mut nonce = [0u8; NONCE];
        reader.read_exact(&mut nonce).await?;
        let mut sealed = vec![0u8; len];
        reader.read_exact(&mut sealed).await?;

        // Try "not last" first; a failure there means either this is the final
        // chunk or the stream has been tampered with — the second attempt tells
        // the two apart, because only genuine final chunks authenticate.
        let (plain, last) = match open(&cipher, &nonce, index, false, &sealed) {
            Some(p) => (p, false),
            None => match open(&cipher, &nonce, index, true, &sealed) {
                Some(p) => (p, true),
                None => bail!("chunk {index} failed to authenticate"),
            },
        };

        writer.write_all(&plain).await?;
        if last {
            break;
        }
        index += 1;
    }

    writer.flush().await?;
    Ok(())
}

fn open(
    cipher: &Aes256Gcm,
    nonce: &[u8; NONCE],
    index: u64,
    last: bool,
    sealed: &[u8],
) -> Option<Vec<u8>> {
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: &aad(index, last),
            },
        )
        .ok()
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
        let script = format!(
            r#"
            import {{ encryptFile, decryptBytes }} from {web:?};
            const dir = {dir:?};
            const plain = new Blob([await Bun.file(dir + "/plain").arrayBuffer()]);
            const sealed = await encryptFile(plain, {KEY:?});
            await Bun.write(dir + "/bun.enc", sealed);
            const opened = await decryptBytes(
                await Bun.file(dir + "/rust.enc").arrayBuffer(), {KEY:?});
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

        tokio::fs::write(&sealed, &good).await?;
        let other = "0".repeat(64);
        assert!(decrypt_file(&sealed, &back, &other).await.is_err(), "wrong key");

        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(())
    }
}
