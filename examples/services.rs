//! Portable services example demonstrating platform-agnostic handlers.
//!
//! This example uses in-memory mocks for local development. To switch to
//! production backends, replace `InMemoryKv`/`InMemoryStorage` with:
//!
//! - `Redis::connect("redis://localhost:6379").await.unwrap()` (skyzen-redis)
//! - `S3Storage::from_env("my-bucket").await` (skyzen-s3)
//! - `DynamoKv::from_env("my-table").await` (skyzen-aws)
//! - `CosmosKv::from_env("appdb", "files").await.unwrap()` (skyzen-azure)
//!
//! The handler code stays exactly the same.
//!
//! Run with `cargo run --example services`.

use serde::{Deserialize, Serialize};
use skyzen::{
    extract::Path,
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Result as SkyResult,
};
use skyzen_services::{Kv, Storage};
use skyzen_test::mock::{InMemoryKv, InMemoryStorage};

#[derive(Debug, Serialize, Deserialize)]
struct FileMetadata {
    name: String,
    size: usize,
}

#[derive(Debug, Deserialize)]
struct UploadRequest {
    name: String,
    content: String,
}

/// List all file metadata keys stored in KV.
///
/// `list_all` drains the backend's list cursor, which suits a demo namespace; a real listing
/// endpoint should page with `kv.list(KvListOptions::new().with_prefix("file:").with_limit(n))`
/// and hand the returned cursor back to the client.
async fn list_files(kv: Kv) -> SkyResult<Json<Vec<String>>> {
    let keys = kv.list_all(Some("file:")).await?;
    Ok(Json(keys))
}

/// Get metadata for a single file from KV.
async fn get_file(kv: Kv, Path(name): Path<String>) -> SkyResult<Json<Option<FileMetadata>>> {
    let meta = kv.get_json::<FileMetadata>(&format!("file:{name}")).await?;
    Ok(Json(meta))
}

/// Upload a file: store bytes in object storage and metadata in KV.
async fn upload_file(
    kv: Kv,
    storage: Storage,
    Json(body): Json<UploadRequest>,
) -> SkyResult<&'static str> {
    let data = body.content.into_bytes();
    let meta = FileMetadata {
        name: body.name.clone(),
        size: data.len(),
    };

    // Store file bytes in object storage
    storage.put(&body.name, data).await?;

    // Store metadata in KV
    kv.put_json(&format!("file:{}", body.name), &meta).await?;

    Ok("uploaded")
}

fn build_router(kv: Kv, storage: Storage) -> Router {
    Route::new((
        "/files".at(list_files),
        "/files".post(upload_file),
        "/files".route(("/{name}".at(get_file),)),
    ))
    .with(kv)
    .with(storage)
    .build()
}

#[skyzen::main]
fn main() -> Router {
    // For local development, use in-memory mocks:
    let kv = Kv::new(InMemoryKv::new());
    let storage = Storage::new(InMemoryStorage::new());

    // For production with Redis + S3, make this `async fn main` and swap to:
    //   let redis = Redis::connect("redis://127.0.0.1:6379").await.unwrap();
    //   let kv = Kv::new(redis);
    //   let storage = Storage::new(S3Storage::from_env("my-bucket").await);

    build_router(kv, storage)
}
