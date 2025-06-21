//! Memory caches.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, io};

use tokio::fs;
use tokio::sync::{Mutex, MutexGuard, RwLock};
use tracing::{error, info};

use crate::Error;
use crate::api::IMAGE_DIRECTORY;
use crate::db::Db;

/// File in which known checksums are stored.
const CHECKSUMS_PATH: &str = "./alarm_checksums";

/// ALARM tarball download URI.
const TARBALL_MD5_URI: &str = "https://archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz.md5";

/// Minimum free disk space percentage.
const MIN_FREE_SPACE_PERCENTAGE: f64 = 0.05;

/// Minimum frequency between new download attempts.
const MIN_INTERVAL: Duration = Duration::from_secs(60 * 5);

/// Maximum number of historic checksums stored.
const MAX_CHECKSUMS: usize = 32;

/// Checksum cache for the latest ALARM tarball.
pub struct AlarmChecksumCache {
    data: RwLock<AlarmChecksumCacheData>,
    enabled: bool,
    db: Arc<Db>,
}

impl AlarmChecksumCache {
    pub async fn new(db: Arc<Db>) -> Self {
        let enabled = !env::var("VALIDATE_TARBALL_MD5").is_ok_and(|v| v == "0");
        Self { enabled, db, data: RwLock::new(AlarmChecksumCacheData::new().await) }
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

        // Get latest available checksum.
        drop(data);
        let latest_checksum = self.update_checksum().await;

        latest_checksum.is_some_and(|md5| md5 == md5sum)
    }

    /// Check for ALARM rootfs checksum updates.
    ///
    /// Returns the new checksum if it has changed.
    pub async fn update_checksum(&self) -> Option<String> {
        // Debounce excessive update requests.
        let data = self.data.read().await;
        let now = Instant::now();
        if now - data.last_update < MIN_INTERVAL {
            return None;
        }

        // Get latest checksum from archlinuxarm.org.
        drop(data);
        let latest_checksum = Self::latest_checksum().await;
        let mut data = self.data.write().await;
        data.last_update = now;

        // Update known checksums.
        if let Some(latest_checksum) = latest_checksum
            && data.md5sums.first() != Some(&latest_checksum)
        {
            info!("Adding new ALARM tarball MD5: {latest_checksum}");

            // Update cache data.
            data.md5sums.insert(0, latest_checksum.clone());
            data.md5sums.truncate(MAX_CHECKSUMS);

            // Write new checksums to cache file.
            Self::persist_checksums(&data.md5sums).await;

            // Delete all outdated requests and images.
            if let Err(err) = self.db.remove_done().await {
                error!("Failed to remove outdated requests: {err}");
            }
            if let Err(err) = fs::remove_dir_all(IMAGE_DIRECTORY).await {
                error!("Failed to delete image directory: {err}");
            }

            return Some(latest_checksum);
        }

        None
    }

    /// Get the latest ALARM tarball checksum.
    ///
    /// This will download the latest tarball from archlinuxarm.org,
    /// so it must not be called frequently.
    async fn latest_checksum() -> Option<String> {
        info!("Downloading latest ALARM tarball MD5…");

        // Send request for the latest ALARM tarball.
        let request = reqwest::get(TARBALL_MD5_URI)
            .await
            .inspect_err(|err| error!("ALARM tarball MD5 request failed: {err}"))
            .ok()?;

        // Parse checksum response.
        let checksum = request
            .text()
            .await
            .inspect_err(|err| error!("Invalid ALARM tarball MD5 response: {err}"))
            .ok()?;
        let (md5, _) = checksum.split_once(' ')?;

        Some(md5.into())
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

/// Installation image LRU cache.
pub struct ImageCache {
    data: Mutex<ImageCacheData>,
}

impl ImageCache {
    pub async fn new() -> Result<Self, io::Error> {
        Ok(Self { data: Mutex::new(ImageCacheData::new().await?) })
    }

    /// Obtain a write lock to the underlying data.
    ///
    /// This allows calling [`ImageCacheData::free_space`] and writing to disk
    /// in series while ensuring no other image write claims the space for
    /// itself.
    pub async fn write(&self) -> MutexGuard<'_, ImageCacheData> {
        self.data.lock().await
    }
}

/// Writeable installation image LRU cache.
pub struct ImageCacheData {
    images: Vec<PathBuf>,
}

impl ImageCacheData {
    async fn new() -> Result<Self, io::Error> {
        // Get all existing image files.
        let mut images = Vec::new();
        let mut entries = fs::read_dir(IMAGE_DIRECTORY).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_file() {
                images.push(entry.path());
            }
        }

        Ok(Self { images })
    }
}

impl ImageCacheData {
    /// Update last access time for a cache entry.
    pub async fn accessed<P>(&mut self, path: P)
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        if let Some(index) = self.images.iter().rposition(|p| p == path) {
            for i in (1..=index).rev() {
                self.images.swap(i, i - 1);
            }
        }
    }

    /// Make `size` bytes available for future image writes.
    ///
    /// This will remove the least recently used image files until at least
    /// `size` bytes are available.
    pub async fn free_space(&mut self, required: u64) -> Result<(), Error> {
        let mut available_size = available_image_space()?;

        info!("Freeing image space for {required} bytes (available: {available_size})");

        // Remove least recently used images until we have enough space.
        while available_size < required {
            // Get the least recently used image's path.
            let path = match self.images.last() {
                Some(path) => path,
                None => break,
            };

            // Remove the file and add its size to the available space.
            let size = fs::metadata(&path).await?.len();
            fs::remove_file(&path).await?;
            available_size += size;

            info!("Removed {path:?}: {size} bytes (available: {available_size})");
        }

        // Return error if there's still not enough space left.
        if available_size < required {
            let kind = io::ErrorKind::StorageFull;
            Err(io::Error::new(kind, "insufficient storage left for image").into())
        } else {
            Ok(())
        }
    }

    /// Add an image's path to the cache.
    pub fn add<P>(&mut self, path: P)
    where
        P: Into<PathBuf>,
    {
        self.images.insert(0, path.into());
    }
}

/// Get space available for writing images.
///
/// This is based on the available disk space with a slight bit of buffer to
/// prevent catastrophic failures.
fn available_image_space() -> Result<u64, Error> {
    let statvfs = rustix::fs::statvfs(IMAGE_DIRECTORY)?;

    let total = statvfs.f_blocks * statvfs.f_bsize;
    let reserved = (total as f64 * MIN_FREE_SPACE_PERCENTAGE).ceil() as u64;

    let available = statvfs.f_bavail * statvfs.f_bsize;
    Ok(available.saturating_sub(reserved))
}
