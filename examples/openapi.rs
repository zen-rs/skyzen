//! Demonstrates the `#[skyzen::openapi]` attribute and router introspection APIs.

#![allow(unused)]

use http::Method;
use serde::{Deserialize, Serialize};
use skyzen::{
    extract::Query,
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

/// Creates a task under a project, demonstrating multiple extractors with OpenAPI metadata.
#[skyzen::openapi]
async fn create_task(
    params: skyzen::routing::Params,
    Query(filter): Query<TaskFilter>,
    Json(draft): Json<TaskDraft>,
) -> skyzen::Result<Json<Task>> {
    // In a real handler we would persist the task; here we just echo the request back.
    let project_id = params
        .get("project_id")
        .map_or_else(|_| "unknown".to_string(), ToString::to_string);
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

fn log_openapi(spec: &OpenApi) {
    if !spec.is_enabled() {
        println!("OpenAPI instrumentation disabled (release build).");
        return;
    }
    fn schema_to_string<T: Serialize>(schema: &T) -> String {
        serde_json::to_string(schema).unwrap_or_else(|err| format!("<invalid schema: {err}>"))
    }

    for op in spec.operations() {
        println!(
            "{} {} handled by {}",
            op.method.as_str(),
            op.path,
            op.handler_type
        );

        if let Some(docs) = op.docs {
            println!("  docs: {docs}");
        }

        for param in &op.parameters {
            let schema = param
                .schema
                .schema
                .as_ref()
                .map(schema_to_string)
                .unwrap_or_else(|| "<undocumented>".to_string());
            let content_type = param.schema.content_type.unwrap_or("<unknown>");
            println!("  param {} ({}): {}", param.name, content_type, schema);
        }

        if op.responses.is_empty() {
            println!("  response: <ignored>");
        } else {
            for response in &op.responses {
                let status = response.status.unwrap_or(StatusCode::OK);
                let content_type = response.content_type.unwrap_or("<unspecified>");
                let schema = response
                    .schema
                    .as_ref()
                    .map(schema_to_string)
                    .unwrap_or_else(|| "<undocumented>".to_string());
                println!(
                    "  response {} ({}): {}",
                    status.as_u16(),
                    content_type,
                    schema
                );
            }
        }
    }

    #[cfg(debug_assertions)]
    {
        let json = serde_json::to_string_pretty(&spec.to_utoipa_spec())
            .unwrap_or_else(|err| format!("<failed to serialize spec: {err}>"));
        println!("\nFull OpenAPI document:\n{json}");
    }
}

#[skyzen::main]
fn main() -> Router {
    let redoc_endpoint = Route::new(("/hello".at(hello),)).openapi().redoc();
    let router = Route::new((
        "/hello".at(hello),
        "/projects/{project_id}/tasks".at(create_task),
        // Serve interactive docs at GET /docs via utoipa-redoc.
        "/docs".endpoint(Method::GET, redoc_endpoint),
    ))
    .build();
    let openapi = router.openapi();
    println!("OpenAPI enabled: {}", openapi.is_enabled());
    println!("ReDoc endpoint mounted at GET /docs");
    log_openapi(&openapi);
    router
}
