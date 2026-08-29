//! Integration tests for the `#[skyzen::test]` macro.

use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Form,
    Result, ToSchema,
};
use skyzen_services::{
    durable::{Alarm, DurableDb, DurableKv},
    Kv,
};
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

async fn touch_durable(durable_kv: DurableKv, alarm: Alarm) -> Result<String> {
    durable_kv.put("visited", b"1").await?;
    alarm.set_alarm(1337).await?;
    Ok("touched".to_owned())
}

async fn record_durable_query(durable_db: DurableDb) -> Result<String> {
    durable_db.query("SELECT 1").execute().await?;
    Ok("queried".to_owned())
}

fn durable_app() -> Router {
    Route::new((
        "/durable".at(touch_durable),
        "/durable-db".at(record_durable_query),
    ))
    .build()
}

#[skyzen::test]
async fn injects_the_durable_services_into_the_test_context(
    durable_kv: DurableKv,
    durable_db: DurableDb,
    alarm: Alarm,
    ctx: TestContext,
) {
    let client = ctx.client(durable_app());

    client.get("/durable").send().await.assert_status(200);
    client.get("/durable-db").send().await.assert_status(200);

    // The same mocks the handler used are the ones the test holds.
    assert_eq!(
        durable_kv.get("visited").await.unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(alarm.get_alarm().await.unwrap(), Some(1337));
    assert_eq!(durable_db.database_size().await.unwrap(), 0);
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
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
