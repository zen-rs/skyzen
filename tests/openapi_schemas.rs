//! Regression tests for the schema a generated document actually carries.
//!
//! Issue #18: `Json<T>`'s `Responder::openapi()` probed `T` from a generic context, where the
//! `ToSchema` bound can never be proved, so it always reported `None`. `#[skyzen::openapi]` has a
//! second path that destructures the return type syntactically and probes at the concrete call
//! site, which *did* work — so the defect was invisible for `-> Json<T>` and hit every handler
//! returning a wrapper the macro cannot see through. These tests document both paths.

use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    OpenApi, Request, Responder, Response, StatusCode, ToSchema,
};

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct Widget {
    id: i64,
    label: String,
}

/// The shape the issue was reported against: an application's own result wrapper, which renders
/// its error side itself and forwards the success side's documentation. `#[skyzen::openapi]`
/// cannot destructure it, so everything it reports has to come from `Responder::openapi()`.
struct Outcome<T>(T);

impl<T: Responder> Responder for Outcome<T> {
    type Error = T::Error;

    fn respond_to(
        self,
        request: &Request,
        response: &mut Response,
    ) -> core::result::Result<(), Self::Error> {
        self.0.respond_to(request, response)
    }

    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        T::openapi()
    }

    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, skyzen::openapi::SchemaRef>,
    ) {
        T::register_openapi_schemas(defs);
    }
}

/// The path that always worked: the macro sees `Json<Widget>` and probes `Widget` itself.
#[skyzen::openapi]
async fn direct() -> Json<Widget> {
    Json(Widget {
        id: 1,
        label: "direct".to_owned(),
    })
}

/// The path that did not: the macro sees `Outcome<…>`, gives up on destructuring, and takes
/// whatever `Responder::openapi()` reports — which used to be a schema-less response.
#[skyzen::openapi]
async fn wrapped() -> Outcome<Json<Widget>> {
    Outcome(Json(Widget {
        id: 2,
        label: "wrapped".to_owned(),
    }))
}

fn document() -> OpenApi {
    Route::new(("/direct".at(direct), "/wrapped".at(wrapped)))
        .build()
        .openapi()
}

/// The response body of `GET {path}`, as the serialized document renders it.
///
/// utoipa inlines a payload's schema here and *also* registers it under `components`, so these
/// assertions read the inline form; `the_payload_schema_reaches_the_components_map` covers the
/// other half.
fn response_content(spec: &serde_json::Value, path: &str) -> serde_json::Value {
    spec["paths"][path]["get"]["responses"]
        .as_object()
        .and_then(|responses| responses.values().next())
        .and_then(|response| response.get("content"))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

#[test]
fn a_responder_the_macro_cannot_destructure_still_documents_its_payload() {
    let spec = serde_json::to_value(document().to_utoipa_spec()).expect("the spec serializes");

    let wrapped = response_content(&spec, "/wrapped");
    assert!(
        !wrapped.is_null(),
        "issue #18: the wrapped response documented no content at all: {spec:#}"
    );
    let schema = &wrapped["application/json"]["schema"];
    assert!(
        schema["properties"].get("label").is_some(),
        "the wrapped response should describe the payload's fields: {schema:#}"
    );
}

#[test]
fn the_wrapped_and_direct_responses_document_the_same_payload() {
    let spec = serde_json::to_value(document().to_utoipa_spec()).expect("the spec serializes");
    assert_eq!(
        response_content(&spec, "/wrapped"),
        response_content(&spec, "/direct"),
        "a wrapper that forwards `openapi()` should document what it forwards to"
    );
}

#[test]
fn the_payload_schema_reaches_the_components_map() {
    let spec = serde_json::to_value(document().to_utoipa_spec()).expect("the spec serializes");
    let widget = &spec["components"]["schemas"]["Widget"];
    assert!(
        widget.is_object(),
        "the payload's own schema should be registered: {spec:#}"
    );
    assert!(
        widget["properties"].get("label").is_some(),
        "the registered schema should describe the payload's fields: {widget:#}"
    );
}

/// `StatusCode` keeps its meaning alongside a payload: the pair is a responder too.
#[skyzen::openapi]
async fn created() -> (StatusCode, Json<Widget>) {
    (
        StatusCode::CREATED,
        Json(Widget {
            id: 3,
            label: "created".to_owned(),
        }),
    )
}

#[test]
fn a_tuple_responder_documents_its_payload_too() {
    let router: Router = Route::new(("/created".at(created),)).build();
    let spec = serde_json::to_value(router.openapi().to_utoipa_spec()).expect("serializes");
    let content = response_content(&spec, "/created");
    assert!(
        content["application/json"]["schema"]["properties"]
            .get("label")
            .is_some(),
        "{content:#}"
    );
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct DtoA {
    alpha: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct DtoB {
    beta: i32,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct DtoC {
    gamma: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct DtoD {
    delta: u64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct NestedChild {
    leaf: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct DtoE {
    epsilon: String,
    child: NestedChild,
}

#[skyzen::openapi]
async fn list_a() -> Json<Vec<DtoA>> {
    Json(Vec::new())
}

#[skyzen::openapi]
async fn list_b() -> Json<Vec<DtoB>> {
    Json(Vec::new())
}

#[skyzen::openapi]
async fn list_c() -> skyzen::responder::PrettyJson<Vec<DtoC>> {
    skyzen::responder::PrettyJson(Vec::new())
}

#[skyzen::openapi]
async fn create_d(Json(items): Json<Vec<DtoD>>) -> Json<Vec<DtoD>> {
    Json(items)
}

#[skyzen::openapi]
async fn list_e() -> Json<Option<Vec<DtoE>>> {
    Json(None)
}

#[skyzen::openapi]
async fn list_strings() -> Json<Vec<String>> {
    Json(Vec::new())
}

#[skyzen::openapi]
async fn list_nested_vecs() -> Json<Vec<Vec<DtoA>>> {
    Json(Vec::new())
}

#[skyzen::openapi]
async fn list_wrapped_a() -> Outcome<Json<Vec<DtoA>>> {
    Outcome(Json(Vec::new()))
}

#[test]
fn vec_routes_register_element_schemas_and_ref_them_without_vec_collision() {
    let router: Router = Route::new((
        "/a".at(list_a),
        "/b".at(list_b),
        "/c".at(list_c),
        "/d".post(create_d),
        "/e".at(list_e),
        "/strings".at(list_strings),
        "/nested".at(list_nested_vecs),
        "/wrapped-a".at(list_wrapped_a),
    ))
    .build();
    let spec = serde_json::to_value(router.openapi().to_utoipa_spec()).expect("serializes");
    let schemas = spec["components"]["schemas"]
        .as_object()
        .expect("components.schemas should be an object");

    for expected in ["DtoA", "DtoB", "DtoC", "DtoD", "DtoE", "NestedChild"] {
        assert!(
            schemas.contains_key(expected),
            "expected {expected} in components.schemas: {schemas:#?}"
        );
    }

    for forbidden in ["Vec", "Option", "Json", "PrettyJson", "String"] {
        assert!(
            !schemas.contains_key(forbidden),
            "did not expect {forbidden} in components.schemas: {schemas:#?}"
        );
    }

    assert_eq!(
        response_content(&spec, "/a")["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/components/schemas/DtoA" }
        })
    );
    assert_eq!(
        response_content(&spec, "/b")["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/components/schemas/DtoB" }
        })
    );
    assert_eq!(
        response_content(&spec, "/c")["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/components/schemas/DtoC" }
        })
    );
    assert_eq!(
        spec["paths"]["/d"]["post"]["requestBody"]["content"]["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/components/schemas/DtoD" }
        })
    );
    assert_eq!(
        response_content(&spec, "/e")["application/json"]["schema"],
        serde_json::json!({
            "oneOf": [
                { "type": "null" },
                {
                    "type": "array",
                    "items": { "$ref": "#/components/schemas/DtoE" }
                }
            ]
        })
    );
    assert_eq!(
        response_content(&spec, "/strings")["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": { "type": "string" }
        })
    );
    assert_eq!(
        response_content(&spec, "/nested")["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/DtoA" }
            }
        })
    );
    assert_eq!(
        response_content(&spec, "/wrapped-a")["application/json"]["schema"],
        serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/components/schemas/DtoA" }
        })
    );
}

#[test]
fn generic_containers_register_inner_dtos_without_container_names() {
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    let mut defs = BTreeMap::new();
    skyzen::openapi::register_schema_for::<BTreeMap<String, DtoA>>(&mut defs);
    skyzen::openapi::register_schema_for::<HashMap<String, DtoB>>(&mut defs);
    skyzen::openapi::register_schema_for::<BTreeSet<DtoC>>(&mut defs);
    skyzen::openapi::register_schema_for::<HashSet<DtoD>>(&mut defs);
    skyzen::openapi::register_schema_for::<Box<DtoE>>(&mut defs);

    for expected in ["DtoA", "DtoB", "DtoC", "DtoD", "DtoE", "NestedChild"] {
        assert!(
            defs.contains_key(expected),
            "expected {expected} in registered schemas: {:?}",
            defs.keys().collect::<Vec<_>>()
        );
    }

    for forbidden in ["BTreeMap", "HashMap", "BTreeSet", "HashSet", "Box"] {
        assert!(
            !defs.contains_key(forbidden),
            "did not expect {forbidden} in registered schemas"
        );
    }

    let map_schema = serde_json::to_value(
        skyzen::openapi::schema_of::<BTreeMap<String, DtoA>>().expect("map schema"),
    )
    .expect("serializes");
    assert_eq!(
        map_schema["additionalProperties"],
        serde_json::json!({ "$ref": "#/components/schemas/DtoA" })
    );
}
