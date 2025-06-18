//! SQLite requests DB.

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
    pub async fn status(&self, device: &str, md5sum: &str) -> Result<Option<Status>, Error> {
        let status = sqlx::query_scalar!(
            r#"
                SELECT status as "status: _"
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
        device: &str,
        md5sum: &str,
        status: Status,
    ) -> Result<(), Error> {
        match status {
            // Ignore transitions to pending, since it is automatic during insert.
            Status::Pending => return Ok(()),
            // Only allow this transition if 'pending' or timed out 'building'.
            Status::Building => {
                let status = status.as_str();
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
                let status = status.as_str();
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
            Status::Done => {
                let status = status.as_str();
                sqlx::query!(
                    "UPDATE requests SET status = $1 WHERE md5sum = $2 AND device = $3",
                    status,
                    md5sum,
                    device,
                )
                .execute(&self.pool)
                .await?;
            },
        }

        Ok(())
    }

    pub async unsafe fn set_status_unchecked(
        &self,
        device: &str,
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

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Building => "building",
            Self::Writing => "writing",
            Self::Done => "done",
        }
    }
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
}

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, sleep};

    use super::*;

    #[tokio::test]
    async fn build_lifecycle() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_build_lifecycle".into(), "kumo".into(), "catacomb".into()];
        let combined_packages = "__test_build_lifecycle catacomb kumo";
        let md5sum = format!("{:x}", md5::compute(combined_packages));
        let device = Device::PinePhonePro;
        let device_str = "pinephone-pro";

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device_str,)
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
        db.set_status(device_str, &md5sum, Status::Building).await.unwrap();

        // Building jobs should be hidden from pending list.
        let pending_building = db.pending().await.unwrap();
        let is_pending = pending_building.iter().any(|r| r.md5sum == md5sum && r.device == device);
        assert!(!is_pending);

        // Mark request as done.
        db.set_status(device_str, &md5sum, Status::Done).await.unwrap();

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
        let device_str = "pinephone";

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device_str)
            .execute(&db.pool)
            .await
            .unwrap();

        // Get timestamp after insert.
        db.add_request(device, packages.clone()).await.unwrap();
        let initial_timestamp = sqlx::query_scalar!(
            "SELECT updated_at FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device_str,
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();

        // Wait for a second since SQLite uses second precision.
        sleep(Duration::from_secs(1)).await;

        // Get timestamp after status change.
        db.set_status(device_str, &md5sum, Status::Done).await.unwrap();
        let after_update = sqlx::query_scalar!(
            "SELECT updated_at FROM requests WHERE md5sum = $1 AND device = $2",
            md5sum,
            device_str,
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
        let device_str = "pinephone-pro";

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device_str)
            .execute(&db.pool)
            .await
            .unwrap();

        // Mark package as done.
        db.add_request(device, packages.clone()).await.unwrap();
        db.set_status(device_str, &md5sum, Status::Done).await.unwrap();

        // Ensure status cannot 'regress'.
        db.set_status(device_str, &md5sum, Status::Pending).await.unwrap();
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
        let device_str = "pinephone";

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1 AND device = $2", md5sum, device_str)
            .execute(&db.pool)
            .await
            .unwrap();

        db.add_request(device, packages.clone()).await.unwrap();
        db.set_status(device_str, &md5sum, Status::Building).await.unwrap();

        let result = db.set_status(device_str, &md5sum, Status::Building).await;
        assert!(matches!(result, Err(Error::StatusConflict)));
    }

    #[tokio::test]
    async fn separate_devices_no_conflict() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_separate_devices_no_conflict".into()];
        let combined_packages = &packages[0];
        let md5sum = format!("{:x}", md5::compute(combined_packages));

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE md5sum = $1", md5sum,)
            .execute(&db.pool)
            .await
            .unwrap();

        // Put PP into pending state.
        db.add_request(Device::PinePhone, packages.clone()).await.unwrap();

        // Put PPP into building state.
        db.add_request(Device::PinePhonePro, packages.clone()).await.unwrap();
        db.set_status("pinephone-pro", &md5sum, Status::Building).await.unwrap();

        // Reinsert PPP to ensure it's still building.
        let ppp_status = db.status("pinephone-pro", &md5sum).await.unwrap();
        assert_eq!(ppp_status, Some(Status::Building));

        // Reinsert PP to ensure it's still pending.
        let pp_status = db.status("pinephone", &md5sum).await.unwrap();
        assert_eq!(pp_status, Some(Status::Pending));
    }
}
