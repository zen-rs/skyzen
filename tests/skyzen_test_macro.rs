//! Integration tests for the `#[skyzen::test]` macro.

use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    Result,
};
use skyzen_services::Kv;
use skyzen_test::TestContext;

async fn read_greeting(kv: Kv) -> Result<String> {
    Ok(kv
        .get_text("greeting")
        .await?
        .expect("greeting should be preloaded by the test"))
}

fn app() -> Router {
    Route::new(("/greeting".at(read_greeting),)).build()
}

#[skyzen::test]
async fn injects_mock_kv_into_test_context(kv: Kv, ctx: TestContext) {
    kv.put("greeting", b"hello from macro").await.unwrap();

    let response = ctx.client(app()).get("/greeting").send().await;
    response.assert_status(200);
    response.assert_body_contains("hello from macro");
}
