//! Portable services example demonstrating platform-agnostic handlers.
//!
//! This example uses in-memory mocks for local development. To switch to
//! production backends, replace `InMemoryKv`/`InMemoryStorage` with:
//!
//! - `Redis::connect("redis://localhost:6379").await.unwrap()` (skyzen-redis)
//! - `S3Storage::from_env("my-bucket")` (skyzen-s3)
//! - `DynamoKv::from_env("my-table").await` (skyzen-aws)
//!
//! The handler code stays exactly the same.
//!
//! Run with `cargo run --example services`.

use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Params, Route, Router},
    utils::{Json, State},
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
async fn list_files(State(kv): State<Kv>) -> SkyResult<Json<Vec<String>>> {
    let keys = kv
        .list(Some("file:"))
        .await
        .map_err(|e| skyzen::Error::msg(e.to_string()))?;
    Ok(Json(keys))
}

/// Get metadata for a single file from KV.
async fn get_file(State(kv): State<Kv>, params: Params) -> SkyResult<Json<Option<FileMetadata>>> {
    let name = params.get("name")?;
    let meta = kv
        .get_json::<FileMetadata>(&format!("file:{name}"))
        .await
        .map_err(|e| skyzen::Error::msg(e.to_string()))?;
    Ok(Json(meta))
}

/// Upload a file: store bytes in object storage and metadata in KV.
async fn upload_file(
    State(kv): State<Kv>,
    State(storage): State<Storage>,
    Json(body): Json<UploadRequest>,
) -> SkyResult<&'static str> {
    let data = body.content.into_bytes();
    let meta = FileMetadata {
        name: body.name.clone(),
        size: data.len(),
    };

    // Store file bytes in object storage
    storage
        .put(&body.name, data)
        .await
        .map_err(|e| skyzen::Error::msg(e.to_string()))?;

    // Store metadata in KV
    kv.put_json(&format!("file:{}", body.name), &meta)
        .await
        .map_err(|e| skyzen::Error::msg(e.to_string()))?;

    Ok("uploaded")
}

fn build_router(kv: Kv, storage: Storage) -> Router {
    Route::new((
        "/files".at(list_files),
        "/files".post(upload_file),
        "/files".route(("/{name}".at(get_file),)),
    ))
    .middleware(State(kv))
    .middleware(State(storage))
    .build()
}

#[skyzen::main]
fn main() -> Router {
    // For local development, use in-memory mocks:
    let kv = Kv::new(InMemoryKv::new());
    let storage = Storage::new(InMemoryStorage::new());

    // For production with Redis + S3, swap to:
    //   let redis = Redis::connect("redis://127.0.0.1:6379").await.unwrap();
    //   let kv = Kv::new(redis);
    //   let storage = Storage::new(S3Storage::from_env("my-bucket"));

    build_router(kv, storage)
}
