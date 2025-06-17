//! SQLite requests DB.

use sqlx::sqlite::{Sqlite, SqliteConnectOptions, SqlitePool};
use sqlx::types::chrono::NaiveDateTime;
use sqlx::{Error, Pool};

pub struct Db {
    pool: Pool<Sqlite>,
}

impl Db {
    pub async fn new() -> Result<Self, Error> {
        // Create or open the SQLite database.
        let options = SqliteConnectOptions::new().filename("db.sqlite").create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();

        // Run database migrations.
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        Ok(Self { pool })
    }

    /// Get pending build requests.
    pub async fn pending(&self) -> Result<Vec<Request>, Error> {
        let requests = sqlx::query_as!(
            Request,
            "
            SELECT * FROM requests
            WHERE status = 'pending'
                OR (status = 'building' AND updated_at < datetime('now', '-30 minutes'))"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Request a new iso to be built.
    ///
    /// This will return `true` if a new built was started.
    pub async fn build(&self, mut packages: Vec<String>) -> Result<Status, Error> {
        // Convert packages list to whitespace-separated string.
        packages.sort_unstable();
        let packages = packages.join(" ");

        // Insert new request, returning `None` if it was already present.
        let status = sqlx::query_scalar!(
            "
            INSERT INTO requests (packages)
            VALUES ($1)
            ON CONFLICT DO UPDATE
                SET packages = EXCLUDED.packages
            RETURNING status",
            packages
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap();

        Ok(status.into())
    }

    /// Update the status of a build.
    pub async fn set_status(&self, packages: &str, status: Status) -> Result<(), Error> {
        let status = status.as_str();
        sqlx::query!("UPDATE requests SET status = $1 WHERE packages = $2", status, packages)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// ISO build request.
#[derive(PartialEq, Eq, Debug)]
pub struct Request {
    /// Whitespace separated list of packages.
    pub packages: String,
    /// Build request status.
    pub status: Status,
    /// Last request change.
    pub updated_at: NaiveDateTime,
}

/// ISO build request status.
#[derive(PartialEq, Eq, Debug)]
pub enum Status {
    /// ISO build was requested.
    Pending,
    /// Worker is currently building the ISO.
    Building,
    /// A built ISO is present.
    Done,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Building => "building",
            Self::Done => "done",
        }
    }
}

impl From<String> for Status {
    fn from(status: String) -> Self {
        match &*status {
            "building" => Self::Building,
            "pending" => Self::Pending,
            "done" => Self::Done,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, sleep};

    use super::*;

    #[tokio::test]
    async fn build_lifecycle() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_build_lifecycle".into(), "kumo".into(), "catacomb".into()];
        let packages_combined = "__test_build_lifecycle catacomb kumo";

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE packages = $1", packages_combined,)
            .execute(&db.pool)
            .await
            .unwrap();

        // Ensure requests are initially empty.
        let pending_empty = db.pending().await.unwrap();
        let is_pending = pending_empty.iter().any(|r| r.packages == packages_combined);
        assert!(!is_pending);

        // Create a new build request.
        let status = db.build(packages.clone()).await.unwrap();
        assert_eq!(status, Status::Pending);

        // Validate updated pending builds.
        let pending_new = db.pending().await.unwrap();
        let is_pending = pending_new.iter().any(|r| r.packages == packages_combined);
        assert!(is_pending);

        // Mark request as building.
        db.set_status(packages_combined, Status::Building).await.unwrap();

        // Building jobs should be hidden from pending list.
        let pending_building = db.pending().await.unwrap();
        let is_pending = pending_building.iter().any(|r| r.packages == packages_combined);
        assert!(!is_pending);

        // Mark request as done.
        db.set_status(packages_combined, Status::Done).await.unwrap();

        // Done jobs should be hidden from pending list.
        let pending_done = db.pending().await.unwrap();
        let is_pending = pending_done.iter().any(|r| r.packages == packages_combined);
        assert!(!is_pending);

        // Starting a finished build should return `Done` status.
        let status = db.build(packages).await.unwrap();
        assert_eq!(status, Status::Done);
    }

    #[tokio::test]
    async fn timestamp_update_trigger() {
        let db = Db::new().await.unwrap();

        let packages = vec!["__test_timestamp_update_trigger".into()];
        let combined_packages = &packages[0];

        // Cleanup old test data.
        sqlx::query!("DELETE FROM requests WHERE packages = $1", combined_packages)
            .execute(&db.pool)
            .await
            .unwrap();

        // Get timestamp after insert.
        db.build(packages.clone()).await.unwrap();
        let initial_timestamp = sqlx::query_scalar!(
            "SELECT updated_at FROM requests WHERE packages = $1",
            combined_packages,
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap()
        .unwrap();

        // Wait for a second since SQLite uses second precision.
        sleep(Duration::from_secs(1)).await;

        // Get timestamp after status change.
        db.set_status(combined_packages, Status::Done).await.unwrap();
        let after_update = sqlx::query_scalar!(
            "SELECT updated_at FROM requests WHERE packages = $1",
            combined_packages,
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap()
        .unwrap();

        assert_ne!(after_update, initial_timestamp);
    }
}
