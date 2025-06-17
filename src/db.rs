//! SQLite requests DB.

use sqlx::sqlite::{Sqlite, SqliteConnectOptions, SqlitePool};
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
}
