//! End-to-end behaviour of the extractor surface, driven through a real router.

use serde::{Deserialize, Serialize};
use skyzen::{
    extract::{Path, Query},
    routing::{CreateRouteNode, Route},
    utils::Json,
    Body, RequestBodyLimit, Result, StatusCode, ToSchema,
};
use skyzen_test::TestContext;

/// The filter shape `examples/openapi.rs` documents: a repeated query parameter collected into a
/// list.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct TaskFilter {
    tags: Option<Vec<String>>,
    limit: Option<u32>,
}

async fn list_tasks(Query(filter): Query<TaskFilter>) -> Result<Json<TaskFilter>> {
    Ok(Json(filter))
}

#[tokio::test]
async fn a_repeated_query_parameter_reaches_the_handler_as_a_list() {
    let router = Route::new(("/tasks".at(list_tasks),)).build();
    let response = TestContext::new()
        .client(router)
        .get("/tasks?tags=intro&tags=rust&limit=5")
        .send()
        .await;

    response.assert_status_success();
    let filter: TaskFilter = response.json();
    assert_eq!(
        filter.tags.as_deref(),
        Some(["intro".to_owned(), "rust".to_owned()].as_slice())
    );
    assert_eq!(filter.limit, Some(5));
}

#[tokio::test]
async fn an_absent_repeated_parameter_is_still_none() {
    let router = Route::new(("/tasks".at(list_tasks),)).build();
    let response = TestContext::new().client(router).get("/tasks").send().await;

    response.assert_status_success();
    let filter: TaskFilter = response.json();
    assert!(filter.tags.is_none());
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct ProjectTask {
    project: String,
    task: u32,
}

#[tokio::test]
async fn a_typed_path_reaches_the_handler_and_rejects_a_bad_segment() {
    async fn show(Path(path): Path<ProjectTask>) -> Result<Json<ProjectTask>> {
        Ok(Json(path))
    }

    let router = Route::new(("/projects/{project}/tasks/{task}".at(show),)).build();
    let client = TestContext::new().client(router);

    let response = client.get("/projects/apollo/tasks/17").send().await;
    response.assert_status_success();
    let path: ProjectTask = response.json();
    assert_eq!(path.project, "apollo");
    assert_eq!(path.task, 17);

    let response = client.get("/projects/apollo/tasks/soon").send().await;
    response.assert_status(400);
    response.assert_body_contains("task");
}

#[tokio::test]
async fn a_second_body_extractor_fails_loudly_instead_of_reading_nothing() {
    async fn twice(
        first: skyzen::http_kit::utils::Bytes,
        second: skyzen::http_kit::utils::Bytes,
    ) -> Result<String> {
        Ok(format!("{} {}", first.len(), second.len()))
    }

    let router = Route::new(("/echo".post(twice),)).build();
    let response = TestContext::new()
        .client(router)
        .post("/echo")
        .body("hello")
        .send()
        .await;

    // A silent empty second read would have answered `5 0` with a 200.
    response.assert_status(500);
    assert!(
        !response.body_text().contains("5 0"),
        "the second read must not have quietly succeeded: {}",
        response.body_text()
    );
}

#[tokio::test]
async fn an_oversized_body_is_rejected_with_413() {
    async fn accept(body: skyzen::http_kit::utils::Bytes) -> Result<String> {
        Ok(body.len().to_string())
    }

    let router = Route::new(("/upload".post(accept),))
        .with(skyzen::middleware::BodyLimit::max(8))
        .build();

    let response = TestContext::new()
        .client(router)
        .post("/upload")
        .body("far more than eight bytes")
        .send()
        .await;

    response.assert_status(413);
}

#[tokio::test]
async fn a_chunked_body_with_no_declared_length_is_still_capped() {
    async fn accept(body: skyzen::http_kit::utils::Bytes) -> Result<String> {
        Ok(body.len().to_string())
    }

    // Built from a stream, so the body reports no length and no `Content-Length` is sent: only
    // the running cap can catch this one.
    let chunks =
        futures_util::stream::iter((0..8).map(|_| Ok::<_, skyzen::BodyError>("0123456789abcdef")));
    let router = Route::new(("/upload".post(accept),))
        .with(skyzen::middleware::BodyLimit::max(32))
        .build();

    let response = TestContext::new()
        .client(router)
        .post("/upload")
        .body(Body::from_stream(chunks))
        .send()
        .await;

    response.assert_status(413);
}

#[tokio::test]
async fn lifting_the_limit_lets_a_large_body_through() {
    async fn accept(body: skyzen::http_kit::utils::Bytes) -> Result<String> {
        Ok(body.len().to_string())
    }

    let router = Route::new(("/upload".post(accept),))
        .with(skyzen::middleware::BodyLimit::disabled())
        .build();

    let payload = "x".repeat(RequestBodyLimit::DEFAULT + 1);
    let response = TestContext::new()
        .client(router)
        .post("/upload")
        .body(payload.clone())
        .send()
        .await;

    response.assert_status_success();
    assert_eq!(response.body_text(), payload.len().to_string());
}

#[tokio::test]
async fn a_status_and_a_body_compose_in_a_tuple() {
    #[derive(Debug, Serialize, ToSchema)]
    struct Created {
        id: u32,
    }

    async fn create() -> Result<(StatusCode, Json<Created>)> {
        Ok((StatusCode::CREATED, Json(Created { id: 7 })))
    }

    let router = Route::new(("/articles".post(create),)).build();
    let response = TestContext::new()
        .client(router)
        .post("/articles")
        .send()
        .await;

    response.assert_status(201);
    response.assert_json_path("id", &serde_json::json!(7));
}

#[tokio::test]
async fn headers_are_readable_from_a_handler() {
    async fn agent(headers: skyzen::header::HeaderMap) -> Result<String> {
        Ok(headers
            .get(skyzen::header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .to_owned())
    }

    let router = Route::new(("/agent".at(agent),)).build();
    let response = TestContext::new()
        .client(router)
        .get("/agent")
        .header("user-agent", "skyzen-test/1")
        .send()
        .await;

    response.assert_status_success();
    assert_eq!(response.body_text(), "skyzen-test/1");
}
