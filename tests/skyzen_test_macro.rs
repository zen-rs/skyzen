//! Integration tests for the `#[skyzen::test]` macro.

use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Form,
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

#[derive(Debug, Serialize, Deserialize)]
struct Login {
    user: String,
    remember: bool,
}

async fn submit_login(Form(login): Form<Login>) -> Result<String> {
    Ok(format!("user={};remember={}", login.user, login.remember))
}

fn form_app() -> Router {
    Route::new(("/login".post(submit_login),)).build()
}

#[skyzen::test]
async fn form_helper_drives_the_form_extractor(ctx: TestContext) {
    let response = ctx
        .client(form_app())
        .post("/login")
        .form(&Login {
            user: "amélie".to_owned(),
            remember: true,
        })
        .send()
        .await;

    response.assert_status(200);
    response.assert_body_contains("user=amélie;remember=true");
}
