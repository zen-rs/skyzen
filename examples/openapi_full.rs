//! Comprehensive `OpenAPI` + routing showcase.
//!
//! Builds a small article API with multiple extractors, responders, and custom errors wired
//! through `#[skyzen::openapi]` so every handler is documented automatically.

#![allow(deprecated)]

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use serde::{Deserialize, Serialize};
use skyzen::{
    extract::Query,
    routing::{CreateRouteNode, Params, Route, Router},
    utils::{Json, State},
    StatusCode, ToSchema,
};

type SharedStore = Arc<ArticleStore>;

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
struct Article {
    id: String,
    title: String,
    body: String,
    tags: Vec<String>,
    published: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct ArticleDraft {
    title: String,
    body: String,
    tags: Vec<String>,
    publish: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct ArticleFilter {
    tags: Option<Vec<String>>,
    search: Option<String>,
}

#[skyzen::error]
enum ApiError {
    #[error("article not found", status = StatusCode::NOT_FOUND)]
    NotFound,
    #[error("title already exists", status = StatusCode::CONFLICT)]
    DuplicateTitle,
}

#[derive(Default)]
struct ArticleStore {
    next_id: AtomicUsize,
    items: Mutex<HashMap<String, Article>>,
}

impl ArticleStore {
    fn new() -> Self {
        let store = Self::default();
        let _ = store.insert(ArticleDraft {
            title: "Hello Skyzen".into(),
            body: "First article seeded at startup.".into(),
            tags: vec!["intro".into(), "skyzen".into()],
            publish: true,
        });
        store
    }

    fn insert(&self, draft: ArticleDraft) -> Result<Article, ApiError> {
        {
            let items = self.items.lock().expect("store poisoned");
            if items.values().any(|a| a.title == draft.title) {
                return Err(ApiError::DuplicateTitle);
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let article = Article {
            id: format!("art-{id}"),
            title: draft.title,
            body: draft.body,
            tags: draft.tags,
            published: draft.publish,
        };
        self.items
            .lock()
            .expect("store poisoned")
            .insert(article.id.clone(), article.clone());
        Ok(article)
    }

    fn get(&self, id: &str) -> Option<Article> {
        self.items.lock().expect("store poisoned").get(id).cloned()
    }

    fn list(&self, filter: &ArticleFilter) -> Vec<Article> {
        self.items
            .lock()
            .expect("store poisoned")
            .values()
            .filter(|article| match &filter.tags {
                Some(tags) if !tags.is_empty() => tags.iter().all(|tag| article.tags.contains(tag)),
                _ => true,
            })
            .filter(|article| match &filter.search {
                Some(q) if !q.is_empty() => article.title.contains(q) || article.body.contains(q),
                _ => true,
            })
            .cloned()
            .collect()
    }
}

/// List articles with optional tag and text filters.
///
/// This demonstrates query extractors, shared state, and `OpenAPI` metadata.
#[skyzen::openapi]
async fn list_articles(
    Query(filter): Query<ArticleFilter>,
    State(store): State<SharedStore>,
) -> skyzen::Result<Json<Vec<Article>>> {
    let result = store.list(&filter);
    Ok(Json(result))
}

/// Create a new article from JSON body.
///
/// Accepts a JSON payload describing the article and returns the stored record.
#[skyzen::openapi]
async fn create_article(
    State(store): State<SharedStore>,
    Json(draft): Json<ArticleDraft>,
) -> Result<Json<Article>, ApiError> {
    let article = store.insert(draft)?;
    Ok(Json(article))
}

/// Fetch a single article by id or return 404.
///
/// Uses path params and custom errors to demonstrate error mapping in `OpenAPI`.
#[skyzen::openapi]
async fn get_article(
    params: Params,
    State(store): State<SharedStore>,
) -> Result<Json<Article>, ApiError> {
    let id = params.get("id").map_err(|_| ApiError::NotFound)?;
    store.get(id).map(Json).ok_or(ApiError::NotFound)
}

/// Deprecated health check for compatibility with legacy clients.
///
/// Prefer using `GET /articles` instead. This endpoint remains for older SDKs.
#[deprecated(note = "use GET /articles instead")]
#[skyzen::openapi]
async fn legacy_ping() -> &'static str {
    "ok"
}

#[skyzen::main]
fn main() -> Router {
    let store = State(Arc::new(ArticleStore::new()));

    let api_routes = Route::new((
        "/articles"
            .at(list_articles)
            .post(create_article)
            .route(("/articles/{id}".at(get_article),)),
        "/legacy/ping".at(legacy_ping),
    ));

    let openapi = api_routes.openapi();
    let docs = openapi.redoc_route("/docs");

    Route::new((api_routes, docs)).middleware(store).build()
}
