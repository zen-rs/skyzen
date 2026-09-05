//! Demonstrates the `#[skyzen::openapi]` attribute and router introspection APIs.

#![allow(unused)]

use http::Method;
use serde::{Deserialize, Serialize};
use skyzen::{
    extract::{Path, Query},
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    OpenApi, StatusCode, ToSchema,
};

#[derive(Debug, Deserialize, ToSchema)]
struct HelloQuery {
    name: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct HelloResponse {
    message: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct TaskFilter {
    tags: Option<Vec<String>>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
enum TaskPriority {
    Low,
    Medium,
    High,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct TaskDraft {
    title: String,
    priority: TaskPriority,
    due: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
enum TaskState {
    Todo,
    InProgress,
    Done,
}

#[derive(Debug, Serialize, ToSchema)]
struct Task {
    id: String,
    project_id: String,
    title: String,
    priority: TaskPriority,
    state: TaskState,
    due: Option<String>,
    tags: Vec<String>,
}

/// Greets the caller and exposes an `OpenAPI` operation.
#[skyzen::openapi]
async fn hello(Json(query): Json<HelloQuery>) -> skyzen::Result<Json<HelloResponse>> {
    Ok(Json(HelloResponse {
        message: format!("Hello, {}!", query.name),
    }))
}

/// Creates a task under a project, demonstrating multiple extractors with `OpenAPI` metadata.
#[skyzen::openapi]
async fn create_task(
    Path(project_id): Path<String>,
    Query(filter): Query<TaskFilter>,
    Json(draft): Json<TaskDraft>,
) -> skyzen::Result<Json<Task>> {
    // In a real handler we would persist the task; here we just echo the request back.
    let task = Task {
        id: "task-123".into(),
        project_id,
        title: draft.title,
        priority: draft.priority,
        state: TaskState::Todo,
        due: draft.due,
        tags: filter.tags.unwrap_or_default(),
    };
    Ok(Json(task))
}

fn schema_to_string<T: Serialize>(schema: &T) -> String {
    serde_json::to_string(schema).unwrap_or_else(|err| format!("<invalid schema: {err}>"))
}

fn log_openapi(spec: &OpenApi) {
    if !spec.is_enabled() {
        tracing::warn!("OpenAPI support is disabled for this build");
        return;
    }

    for op in spec.operations() {
        tracing::info!(
            "{} {} handled by {}",
            op.method.as_str(),
            op.path,
            op.handler_type
        );

        if let Some(docs) = op.docs {
            tracing::info!("  docs: {docs}");
        }

        for param in &op.parameters {
            let schema = param
                .schema
                .schema
                .as_ref()
                .map_or_else(|| "<undocumented>".to_string(), schema_to_string);
            let content_type = param.schema.content_type.unwrap_or("<unknown>");
            tracing::info!("  param {} ({}): {}", param.name, content_type, schema);
        }

        if op.responses.is_empty() {
            tracing::info!("  response: <ignored>");
        } else {
            for response in &op.responses {
                let status = response.status.unwrap_or(StatusCode::OK);
                let content_type = response.content_type.unwrap_or("<unspecified>");
                let schema = response
                    .schema
                    .as_ref()
                    .map_or_else(|| "<undocumented>".to_string(), schema_to_string);
                tracing::info!(
                    "  response {} ({}): {}",
                    status.as_u16(),
                    content_type,
                    schema
                );
            }
        }
    }

    write_openapi_document(spec);
}

/// Write the generated document to stdout, as a document rather than as a log record.
///
/// This one is not a `tracing` event on purpose. The lines above describe what the router holds
/// and belong in the log; this is the artifact itself — the thing a reader redirects into a file
/// or pipes into `jq` — and a subscriber would prefix every line of it with a level and a target,
/// which is exactly what makes such a pipe useless.
fn write_openapi_document(spec: &OpenApi) {
    use std::io::Write as _;

    let json = serde_json::to_string_pretty(&spec.to_utoipa_spec())
        .unwrap_or_else(|err| format!("<failed to serialize spec: {err}>"));

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if let Err(error) = writeln!(stdout, "{json}") {
        // A closed pipe (`| head`) is the ordinary case, and is not worth a panic.
        tracing::warn!("failed to write the OpenAPI document to stdout: {error}");
    }
}

#[skyzen::main]
fn main() -> Router {
    let scalar_endpoint = Route::new(("/hello".at(hello),)).openapi().scalar();
    let router = Route::new((
        "/hello".at(hello),
        "/projects/{project_id}/tasks".at(create_task),
        // Serve interactive docs at GET /docs via Scalar.
        "/docs".endpoint(Method::GET, scalar_endpoint),
    ))
    .build();
    let openapi = router.openapi();
    tracing::info!("OpenAPI enabled: {}", openapi.is_enabled());
    tracing::info!("Scalar endpoint mounted at GET /docs");
    log_openapi(&openapi);
    router
}
