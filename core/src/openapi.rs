//! `OpenAPI` primitives shared by the core traits.

use alloc::collections::BTreeMap;
use core::fmt;

use http_kit::StatusCode;
use utoipa::openapi::{
    schema::{Schema, SchemaType, Type},
    RefOr,
};

/// `OpenAPI` schema reference type alias.
pub type SchemaRef = RefOr<Schema>;

/// Schema information captured for an extractor argument.
#[derive(Clone)]
pub struct ExtractorSchema {
    /// Content type associated with the extractor, if any.
    pub content_type: Option<&'static str>,
    /// JSON schema describing the extractor payload.
    pub schema: Option<SchemaRef>,
}

/// Schema information captured for a responder.
#[derive(Clone)]
pub struct ResponseSchema {
    /// HTTP status code returned by the responder (or [`StatusCode::OK`] by default).
    pub status: Option<StatusCode>,
    /// Description associated with the response.
    pub description: Option<&'static str>,
    /// JSON schema describing the response payload.
    pub schema: Option<SchemaRef>,
    /// Content type returned by the responder, if known.
    pub content_type: Option<&'static str>,
}

impl fmt::Debug for ExtractorSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtractorSchema")
            .field("content_type", &self.content_type)
            .field("has_schema", &self.schema.is_some())
            .finish()
    }
}

impl fmt::Debug for ResponseSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseSchema")
            .field("status", &self.status)
            .field("description", &self.description)
            .field("content_type", &self.content_type)
            .field("has_schema", &self.schema.is_some())
            .finish()
    }
}

/// Minimal string schema helper for responder payloads.
#[must_use]
pub fn plain_string_schema() -> SchemaRef {
    RefOr::T(Schema::Object(
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(SchemaType::from(Type::String))
            .build(),
    ))
}

/// Function pointer used to register schemas in the `OpenAPI` components section.
pub type SchemaCollector = fn(&mut BTreeMap<String, SchemaRef>);
