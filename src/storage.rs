use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tokio::io::AsyncWriteExt;

use crate::config::Config;

fn client_for(cfg: &Config, endpoint: &str) -> Client {
    let creds = Credentials::new(
        cfg.s3_access_key.clone(),
        cfg.s3_secret_key.clone(),
        None,
        None,
        "static",
    );

    // RustFS speaks the S3 API, but like MinIO it needs path-style addressing
    // (http://host:9000/bucket/key) instead of virtual-host style.
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(cfg.s3_region.clone()))
        .endpoint_url(endpoint.to_string())
        .credentials_provider(creds)
        .force_path_style(true)
        .build();

    Client::from_conf(conf)
}

#[derive(Clone)]
pub struct Storage {
    client: Client,
    /// Same credentials, different endpoint. SigV4 signs the host, so a
    /// presigned URL can't be string-rewritten after the fact — the public host
    /// has to be baked in before signing.
    presign_client: Client,
    bucket: String,
    presign_secs: u64,
}

impl Storage {
    pub fn new(cfg: &Config) -> Self {
        let client = client_for(cfg, &cfg.s3_endpoint);
        let presign_client = if cfg.s3_public_endpoint == cfg.s3_endpoint {
            client.clone()
        } else {
            client_for(cfg, &cfg.s3_public_endpoint)
        };

        Self {
            client,
            presign_client,
            bucket: cfg.s3_bucket.clone(),
            presign_secs: cfg.presign_secs,
        }
    }

    /// Create the bucket if it isn't there yet. Safe to call on every boot.
    pub async fn ensure_bucket(&self) -> Result<()> {
        if self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }

        match self.client.create_bucket().bucket(&self.bucket).send().await {
            Ok(_) => {
                tracing::info!(bucket = %self.bucket, "created bucket");
                Ok(())
            }
            Err(e) => {
                // Another replica may have won the race.
                if self
                    .client
                    .head_bucket()
                    .bucket(&self.bucket)
                    .send()
                    .await
                    .is_ok()
                {
                    Ok(())
                } else {
                    Err(anyhow::Error::from(e)).context("create_bucket failed")
                }
            }
        }
    }

    pub async fn put_file(&self, key: &str, path: &Path) -> Result<i64> {
        let len = tokio::fs::metadata(path).await?.len() as i64;
        let body = ByteStream::from_path(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;

        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(body)
            .send()
            .await
            .with_context(|| format!("put_object {key}"))?;

        Ok(len)
    }

    pub async fn get_to_file(&self, key: &str, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut obj = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("get_object {key}"))?;

        let mut file = tokio::fs::File::create(dest).await?;
        while let Some(chunk) = obj.body.try_next().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }

    /// Raw object body. Used for encrypted stems: the browser has to decrypt
    /// them in JS, and a cross-origin presigned URL would need CORS on the
    /// bucket, so those stream back through this process instead. Only ever
    /// ciphertext, so the plaintext still never passes through here.
    pub async fn get_stream(&self, key: &str) -> Result<ByteStream> {
        let obj = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("get_object {key}"))?;
        Ok(obj.body)
    }

    /// Short-lived download URL, so the client pulls bytes straight from RustFS
    /// instead of streaming them through this process.
    /// `disposition` is a complete Content-Disposition value — the caller builds
    /// it, because getting a non-ASCII filename through a header is RFC 5987's
    /// problem, not storage's.
    pub async fn presign_get(&self, key: &str, disposition: &str) -> Result<String> {
        let cfg = PresigningConfig::expires_in(Duration::from_secs(self.presign_secs))?;
        let disposition = disposition.to_string();

        let req = self
            .presign_client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .response_content_disposition(disposition)
            .presigned(cfg)
            .await
            .with_context(|| format!("presign {key}"))?;

        Ok(req.uri().to_string())
    }

    pub async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        let mut continuation: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(continuation.clone())
                .send()
                .await?;

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    let _ = self
                        .client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .send()
                        .await;
                }
            }

            if resp.is_truncated().unwrap_or(false) {
                continuation = resp.next_continuation_token().map(str::to_string);
            } else {
                return Ok(());
            }
        }
    }
}
