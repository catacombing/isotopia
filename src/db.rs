//! SQLite requests DB.

use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{Sqlite, SqliteConnectOptions, SqlitePool};
use sqlx::types::chrono::NaiveDateTime;
use sqlx::{Pool, Type};

use crate::Error;

pub struct Db {
    pool: Pool<Sqlite>,
}

impl Db {
    pub async fn new() -> Result<Self, sqlx::Error> {
        // Create or open the SQLite database.
        let options = SqliteConnectOptions::new().filename("db.sqlite").create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();

        // Run database migrations.
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        Ok(Self { pool })
    }

    /// Get pending build requests.
    pub async fn pending(&self) -> Result<Vec<Request>, sqlx::Error> {
        let requests = sqlx::query_as!(
            Request,
            r#"
                SELECT md5sum, device as "device: _", packages, status as "status: _", updated_at
                FROM requests
                WHERE status = 'pending'
                    OR (status = 'building' AND updated_at < datetime('now', '-30 minutes'))
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Get possibly failed build requests.
    pub async fn slow_requests(&self) -> Result<Vec<Request>, sqlx::Error> {
        let requests = sqlx::query_as!(
            Request,
            r#"
                SELECT md5sum, device as "device: _", packages, status as "status: _", updated_at
                FROM requests
                WHERE status = 'building' AND updated_at < datetime('now', '-15 minutes')
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Request a new image to be built.
    ///
    /// This will return `true` if a new built was started.
    pub async fn add_request(
        &self,
        device: Device,
        mut packages: Vec<String>,
    ) -> Result<InsertedRequest, sqlx::Error> {
        // Convert packages list to whitespace-separated string.
        packages.sort_unstable();
        let packages = packages.join(" ");

        // Calculate md5 checksum of the packages list.
        let digest = md5::compute(packages.as_bytes());
        let md5sum = format!("{digest:x}");

        // Insert new request, returning `None` if it was already present.
        let request = sqlx::query_as!(
            InsertedRequest,
            r#"
                INSERT INTO requests (md5sum, device, packages)
                VALUES ($1, $2, $3)
                ON CONFLICT DO UPDATE
                    SET md5sum = EXCLUDED.md5sum
                RETURNING md5sum, status as "status: _", updated_at
            "#,
            md5sum,
            device,
            packages,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(request)
    }

    /// Get the status of a build.
    pub async fn status(
        &self,
        device: Device,
        md5sum: &str,
    ) -> Result<Option<RequestStatus>, Error> {
        let status = sqlx::query_as!(
            RequestStatus,
            r#"
                SELECT status as "status: _", img_md5sum
                FROM requests
                WHERE md5sum = $1 AND device = $2
            "#,
            md5sum,
            device,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(status)
    }

    /// Update the status of a build.
    pub async fn set_status(
        &self,
        device: Device,
        md5sum: &str,
        status: Status,
    ) -> Result<(), Error> {
        match status {
            // Ignore transitions to pending, since it is automatic during insert.
            Status::Pending => (),
            // Only allow this transition if 'pending' or timed out 'building'.
            Status::Building => {
                let status = sqlx::query!(
                    "
                        UPDATE requests
                        SET status = $1
                        WHERE md5sum = $2
                            AND device = $3
                            AND status = 'pending'
                            OR (status = 'building'
                                AND updated_at < datetime('now', '-30 minutes'))
                        RETURNING status
                    ",
                    status,
                    md5sum,
                    device,
                )
                .fetch_optional(&self.pool)
                .await?;

                // Return error if m5dsum is invalid or request is not pending a build.
                if status.is_none() {
                    return Err(Error::StatusConflict);
                }
            },
            Status::Writing => {
                let status = sqlx::query!(
                    "
                        UPDATE requests
                        SET status = $1
                        WHERE md5sum = $2
                            AND device = $3
                            AND status = 'building'
                        RETURNING status
                    ",
                    status,
                    md5sum,
                    device,
                )
                .fetch_optional(&self.pool)
                .await?;

                // Return error if m5dsum is invalid or request is not pending a write.
                if status.is_none() {
                    return Err(Error::StatusConflict);
                }
            },
            // Ignore transitions to done, since it requires setting checksum.
            // See `Self::set_done`.
            Status::Done => (),
        }

        Ok(())
    }

    /// Update the status of a build, without verifying the origin state.
    ///
    /// # Safety
    ///
    /// Using this to transition to a state like `Done` will put the application
    /// in an inconsistent state, which might require manual intervention to
    /// resolve.
    pub async unsafe fn set_status_unchecked(
        &self,
        device: Device,
        md5sum: &str,
        status: Status,
    ) -> Result<(), Error> {
        sqlx::query!(
            "UPDATE requests SET status = $1 WHERE md5sum = $2 AND device = $3",
            status,
            md5sum,
            device,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark request as done.
    pub async fn set_done(
        &self,
        device: Device,
        packages_md5sum: &str,
        img_md5sum: &str,
    ) -> Result<(), Error> {
        sqlx::query!(
            r#"
                UPDATE requests
                SET status = 'done', img_md5sum = $1
                WHERE md5sum = $2 AND device = $3
            "#,
            img_md5sum,
            packages_md5sum,
            device,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a request.
    pub async fn delete(&self, device: Device, md5sum: &str) -> Result<(), Error> {
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Remove all finished requests.
    ///
    /// This should be called whenever the image files have been deleted, to
    /// avoid inconsistency between database and image storage.
    pub async fn delete_done(&self) -> Result<(), Error> {
        sqlx::query!("DELETE FROM requests WHERE status = 'done'").execute(&self.pool).await?;
        Ok(())
    }

    /// Add 1 to the request's download count.
    pub async fn increment_downloads(&self, device: Device, md5sum: &str) -> Result<(), Error> {
        sqlx::query!(
            r#"
                UPDATE requests
                SET downloads = downloads + 1
                WHERE md5sum = $1 AND device = $2
            "#,
            md5sum,
            device,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Image build request.
#[derive(Serialize, PartialEq, Eq, Debug)]
pub struct Request {
    /// MD5 checksum of the package list.
    pub md5sum: String,
    /// Requested target device.
    pub device: Device,
    /// Whitespace separated list of packages.
    pub packages: String,
    /// Build request status.
    pub status: Status,
    /// Last request change.
    pub updated_at: NaiveDateTime,
}

/// Image build request return value on insertion.
///
/// This has the package list stripped to avoid pointless large data transfers.
#[derive(Serialize, PartialEq, Eq, Debug)]
pub struct InsertedRequest {
    /// MD5 checksum of the package list.
    pub md5sum: String,
    /// Build request status.
    pub status: Status,
    /// Last request change.
    pub updated_at: NaiveDateTime,
}

/// Image build request status.
#[derive(Type, Deserialize, Serialize, Copy, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum Status {
    /// Image build was requested.
    Pending,
    /// Worker is currently building the image.
    Building,
    /// Image is currently being written to FS.
    Writing,
    /// A built image is present.
    Done,
}

/// Image target device,
#[derive(Type, Deserialize, Serialize, Copy, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum Device {
    #[serde(rename = "pinephone-pro")]
    #[sqlx(rename = "pinephone-pro")]
    PinePhonePro,
    PinePhone,
    #[serde(rename = "fairphone-fp5")]
    #[sqlx(rename = "fairphone-fp5")]
    Fairphone5,
}

impl Display for Device {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::PinePhonePro => write!(f, "pinephone-pro"),
            Self::Fairphone5 => write!(f, "fairphone-fp5"),
            Self::PinePhone => write!(f, "pinephone"),
        }
    }
}

impl TryFrom<&str> for Device {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "pinephone-pro" => Ok(Device::PinePhonePro),
            "fairphone-fp5" => Ok(Device::Fairphone5),
            "pinephone" => Ok(Device::PinePhone),
            _ => Err(()),
        }
    }
}

/// Status of a build request.
#[derive(Serialize, PartialEq, Eq, Debug)]
pub struct RequestStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_md5sum: Option<String>,
    pub status: Status,
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use tokio::time::{Duration, sleep};

    use super::*;

    #[tokio::test]
    async fn build_lifecycle() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_build_lifecycle".into(), "kumo".into(), "catacomb".into()];
        let combined_packages = "__test_build_lifecycle catacomb kumo";
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhonePro;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        // Ensure requests are initially empty.
        let pending_empty = db.pending().await.unwrap();
        let is_pending = pending_empty.iter().any(|r| r.md5sum == md5sum && r.device == device);
        assert!(!is_pending);

        // Create a new build request.
        let request = db.add_request(device, packages.clone()).await.unwrap();
        assert_eq!(request.status, Status::Pending);
        assert_eq!(request.md5sum, md5sum);

        // Validate updated pending builds.
        let pending_new = db.pending().await.unwrap();
        let is_pending = pending_new
            .iter()
            .any(|r| r.md5sum == md5sum && r.device == device && r.packages == combined_packages);
        assert!(is_pending);

        // Mark request as building.
        db.set_status(device, &md5sum, Status::Building).await.unwrap();

        // Building jobs should be hidden from pending list.
        let pending_building = db.pending().await.unwrap();
        let is_pending = pending_building.iter().any(|r| r.md5sum == md5sum && r.device == device);
        assert!(!is_pending);

        // Mark request as done.
        db.set_done(device, &md5sum, "xxx").await.unwrap();

        // Done jobs should be hidden from pending list.
        let pending_done = db.pending().await.unwrap();
        let is_pending = pending_done.iter().any(|r| r.md5sum == md5sum && r.device == device);
        assert!(!is_pending);

        // Starting a finished build should return `Done` status.
        let request = db.add_request(device, packages).await.unwrap();
        assert_eq!(request.status, Status::Done);
    }

    #[tokio::test]
    async fn timestamp_update_trigger() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_timestamp_update_trigger".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhone;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        // Get timestamp after insert.
        db.add_request(device, packages.clone()).await.unwrap();
        let initial_timestamp = sqlx::query_scalar!(
            "SELECT updated_at FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device,
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();

        // Wait for a second since SQLite uses second precision.
        sleep(Duration::from_secs(1)).await;

        // Get timestamp after status change.
        db.set_done(device, &md5sum, "xxx").await.unwrap();
        let after_update = sqlx::query_scalar!(
            "SELECT updated_at FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device,
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();

        assert_ne!(after_update, initial_timestamp);
    }

    #[tokio::test]
    async fn cannot_update_done() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_cannot_update_done".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhonePro;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        // Mark package as done.
        db.add_request(device, packages.clone()).await.unwrap();
        db.set_done(device, &md5sum, "xxx").await.unwrap();

        // Ensure status cannot 'regress'.
        db.set_status(device, &md5sum, Status::Pending).await.unwrap();
        let pending_done = db.pending().await.unwrap();
        let is_pending = pending_done.iter().any(|r| r.md5sum == md5sum && r.device == device);
        assert!(!is_pending);
    }

    #[tokio::test]
    async fn cannot_build_twice() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_cannot_build_twice".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhone;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        db.add_request(device, packages.clone()).await.unwrap();
        db.set_status(device, &md5sum, Status::Building).await.unwrap();

        let result = db.set_status(device, &md5sum, Status::Building).await;
        assert!(matches!(result, Err(Error::StatusConflict)));
    }

    #[tokio::test]
    async fn separate_devices_no_conflict() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_separate_devices_no_conflict".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1", md5sum)
            .execute(&db.pool)
            .await
            .unwrap();

        // Put PP into pending state.
        db.add_request(Device::PinePhone, packages.clone()).await.unwrap();

        // Put PPP into building state.
        db.add_request(Device::PinePhonePro, packages.clone()).await.unwrap();
        db.set_status(Device::PinePhonePro, &md5sum, Status::Building).await.unwrap();

        // Reinsert PPP to ensure it's still building.
        let ppp_status = db.status(Device::PinePhonePro, &md5sum).await.unwrap().unwrap();
        assert_eq!(ppp_status.status, Status::Building);

        // Reinsert PP to ensure it's still pending.
        let pp_status = db.status(Device::PinePhone, &md5sum).await.unwrap().unwrap();
        assert_eq!(pp_status.status, Status::Pending);
    }

    #[tokio::test]
    async fn delete_done() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_delete_done".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhone;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        // Pending requests aren't deleted.
        db.add_request(device, packages.clone()).await.unwrap();
        db.delete_done().await.unwrap();
        let has_request = sqlx::query!(
            "SELECT * FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap()
        .is_some();
        assert!(has_request);

        // Done requests are deleted.
        db.set_done(device, &md5sum, "xxx").await.unwrap();
        db.delete_done().await.unwrap();
        let has_request = sqlx::query!(
            "SELECT * FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap()
        .is_some();
        assert!(!has_request);
    }

    #[tokio::test]
    async fn slow_requests() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_slow_requests".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhone;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        // Create new building request.
        db.add_request(device, packages.clone()).await.unwrap();
        db.set_status(device, &md5sum, Status::Building).await.unwrap();
        let slow_requests = db.slow_requests().await.unwrap();
        assert_eq!(slow_requests, Vec::new());

        // Manually timewarp request to 1 hour ago.
        sqlx::query!(
            r#"
                UPDATE requests
                SET updated_at = datetime('1970-01-01')
                WHERE md5sum = $1 AND device = $2
            "#,
            md5sum,
            device
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Ensure request is marked as slow now.
        let slow_requests = db.slow_requests().await.unwrap();
        assert_eq!(slow_requests, vec![Request {
            packages: packages.join(" "),
            md5sum,
            device,
            updated_at: DateTime::UNIX_EPOCH.naive_utc(),
            status: Status::Building,
        }],);
    }

    #[tokio::test]
    async fn increment_download_count() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_increment_download_count".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhone;

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device)
            .execute(&db.pool)
            .await
            .unwrap();

        // Create new request.
        db.add_request(device, packages.clone()).await.unwrap();

        // Increment download count by one.
        db.increment_downloads(device, &md5sum).await.unwrap();

        // Ensure count is actually 1.
        let download_count = sqlx::query_scalar!(
            "SELECT downloads FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(download_count, 1);
    }

    #[tokio::test]
    async fn set_status_done() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_set_status_done".into()];
        let combined_packages = &packages[0];
        let packages_md5sum = format!("{:x}", md5::compute(combined_packages));
        let img_md5sum = format!("{:x}", md5::compute("img content"));
        let device = Device::PinePhone;

        // Cleanup old test data.
        sqlx::query!(
            "DELETE FROM requests WHERE md5sum = $1 AND device = $2",
            packages_md5sum,
            device
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Create new building request.
        db.add_request(device, packages.clone()).await.unwrap();
        db.set_status(device, &packages_md5sum, Status::Building).await.unwrap();
        let status = db.status(device, &packages_md5sum).await.unwrap().unwrap();
        assert_eq!(status.status, Status::Building);

        // Ensure done without content checksum is ignored.
        db.set_status(device, &packages_md5sum, Status::Done).await.unwrap();
        let status = db.status(device, &packages_md5sum).await.unwrap().unwrap();
        assert_eq!(status.status, Status::Building);

        // Mark request as done.
        db.set_done(device, &packages_md5sum, &img_md5sum).await.unwrap();
        let status = db.status(device, &packages_md5sum).await.unwrap().unwrap();
        assert_eq!(status.img_md5sum, Some(img_md5sum));
        assert_eq!(status.status, Status::Done);
    }

    #[tokio::test]
    async fn devices() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_devices".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1", md5sum)
            .execute(&db.pool)
            .await
            .unwrap();

        let test_device = async |device| {
            let status = db.status(device, &md5sum).await.unwrap();
            assert_eq!(status, None);

            db.add_request(device, packages.clone()).await.unwrap();

            let status = db.status(device, &md5sum).await.unwrap().unwrap();
            assert_eq!(status.status, Status::Pending);
        };

        test_device(Device::PinePhone).await;
        test_device(Device::PinePhonePro).await;
        test_device(Device::Fairphone5).await;
    }
}
