use db::Db;

mod db;

#[tokio::main]
async fn main() {
    let db = Db::new().await.unwrap();
}
