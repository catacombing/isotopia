//! ALARM checksum validation.

use std::env;
use std::time::{Duration, Instant};

use md5::Context;
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{error, info};

/// File in which known checksums are stored.
const CHECKSUMS_PATH: &str = "./alarm_checksums";

/// ALARM tarball download URI.
const TARBALL_URI: &str = "https://archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz";

/// Minimum frequency between new download attempts.
const MIN_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Maximum number of historic checksums stored.
const MAX_CHECKSUMS: usize = 32;

/// Checksum cache for the latest ALARM tarball.
pub struct AlarmChecksumCache {
    data: RwLock<AlarmChecksumCacheData>,
    enabled: bool,
}

impl AlarmChecksumCache {
    pub async fn new() -> Self {
        let enabled = !env::var("VALIDATE_TARBALL_MD5").is_ok_and(|v| v == "0");
        Self { enabled, data: RwLock::new(AlarmChecksumCacheData::new().await) }
    }

    /// Get the latest know checksum.
    ///
    /// This will never make any requests, but instead return the latest known
    /// checksum.
    pub async fn latest(&self) -> Option<String> {
        self.data.read().await.md5sums.first().cloned()
    }

    /// Check if a checksum matches the latest ALARM tarball checksum.
    pub async fn is_latest(&self, md5sum: &str) -> bool {
        // Approve all checksums when disabled.
        if !self.enabled {
            return true;
        }

        // Short-circuit for trivial checksum matches.
        let data = self.data.read().await;
        if data.md5sums.first().is_some_and(|md5| md5 == md5sum) {
            return true;
        }

        // Disqualify known old checksums.
        if data.md5sums.len() > 1 && md5sum[1..].contains(md5sum) {
            return false;
        }

        // If the last update was recently, assume the checksum is invalid.
        let now = Instant::now();
        if now - data.last_update < MIN_INTERVAL {
            return false;
        }

        // If it's an unknown checksum and we haven't recently checked for
        // updates, download the latest tarball version.
        drop(data);
        let lastest_checksum = Self::latest_checksum().await;
        let mut data = self.data.write().await;
        data.last_update = now;

        if let Some(latest_checksum) = lastest_checksum
            && data.md5sums.first() != Some(&latest_checksum)
        {
            info!("Adding new ALARM tarball MD5: {latest_checksum}");

            // Update cache data.
            data.md5sums.insert(0, latest_checksum);
            data.md5sums.truncate(MAX_CHECKSUMS);

            Self::persist_checksums(&data.md5sums).await;
        }

        data.md5sums.first().is_some_and(|md5| md5 == md5sum)
    }

    /// Get the latest ALARM tarball checksum.
    ///
    /// This will download the latest tarball from archlinuxarm.org,
    /// so it must not be called frequently.
    async fn latest_checksum() -> Option<String> {
        info!("Downloading latest ALARM tarball…");

        // Send request for the latest ALARM tarball.
        let mut request = reqwest::get(TARBALL_URI)
            .await
            .inspect_err(|err| error!("ALARM tarball request failed: {err}"))
            .ok()?;

        // Compute checksum in chunks, to avoid excessive memory usage.
        let mut checksum_context = Context::new();
        loop {
            match request.chunk().await {
                Ok(Some(chunk)) => checksum_context.consume(&chunk),
                Ok(None) => break,
                Err(err) => {
                    error!("Failed to read ALARM tarball response: {err}");
                    return None;
                },
            }
        }

        // Finalize the checksum
        Some(format!("{:x}", checksum_context.compute()))
    }

    /// Write known checksums to disk.
    async fn persist_checksums(md5sums: &[String]) {
        let checksums = md5sums.join("\n");
        if let Err(err) = fs::write(CHECKSUMS_PATH, checksums.as_bytes()).await {
            error!("Failed to persist ALARM checksums: {err}");
        }
    }
}

/// Underlying cache data.
struct AlarmChecksumCacheData {
    last_update: Instant,
    md5sums: Vec<String>,
}

impl AlarmChecksumCacheData {
    pub async fn new() -> Self {
        // Load checksums from file.
        let md5sums = match fs::read_to_string(CHECKSUMS_PATH).await {
            Ok(cached_md5sums) => cached_md5sums
                .split('\n')
                .map(|md5sum| md5sum.trim().to_owned())
                .filter(|md5sum| !md5sum.is_empty())
                .collect(),
            Err(err) => {
                error!("Failed to load ALARM checksums: {err}");
                Vec::new()
            },
        };

        Self { last_update: Instant::now() - MIN_INTERVAL, md5sums }
    }
}
