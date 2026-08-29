//! Typed access to the route's captured `{name}` segments.

use std::fmt::Display;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde::forward_to_deserialize_any;

use crate::{extract::Extractor, routing::Params, Request, StatusCode};

/// Deserializes the route's path parameters into `T`.
///
/// The captured `{name}` segments are matched to `T` the way a query string is matched to a
/// struct: a struct or map by parameter name, a tuple in the order the route declares them, and a
/// bare primitive when the route captures exactly one.
///
/// ```rust
/// use skyzen::{extract::Path, routing::{CreateRouteNode, Route}, Result};
/// use serde::Deserialize;
///
/// async fn show(Path(id): Path<u64>) -> Result<String> {
///     Ok(format!("item {id}"))
/// }
///
/// #[derive(Deserialize)]
/// struct Post {
///     user: String,
///     post: u32,
/// }
///
/// async fn post(Path(Post { user, post }): Path<Post>) -> Result<String> {
///     Ok(format!("{user}/{post}"))
/// }
///
/// let route = Route::new((
///     "/items/{id}".at(show),
///     "/users/{user}/posts/{post}".at(post),
/// ));
/// ```
///
/// The parameter names still come from the route pattern, so renaming `{id}` without renaming the
/// field is a `400` at request time rather than a compile error — but the *type* is now checked
/// once, here, instead of in every handler. Reach for [`Params`] when the names are only known at
/// runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path<T>(pub T);

impl_deref!(Path);

/// Raised when the captured path parameters do not fit the requested type.
///
/// The message names the parameter and what went wrong with it, so the `400` tells the caller
/// which segment of the URL to fix.
#[skyzen::error(message = "Invalid path parameters: {0}", status = StatusCode::BAD_REQUEST)]
pub struct PathError(String);

impl de::Error for PathError {
    fn custom<T: Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Path<T> {
    type Error = PathError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let params = Params::extract(request)
            .await
            .unwrap_or_else(|infallible| match infallible {});
        let value = T::deserialize(PathDeserializer::new(params.pairs()))?;
        Ok(Self(value))
    }

    /// `Path<T>` names no parameters of its own — the route pattern does — so this reports where
    /// the values come from and nothing about their types.
    ///
    /// The types are supplied by `#[skyzen::openapi]`, which probes `T` at the handler's own call
    /// site with [`SchemaProbe`](crate::openapi::SchemaProbe), where `T` is spelled out and the
    /// `ToSchema` bound is therefore provable. That indirection is what lets `Path<(String, u32)>`
    /// keep working: a tuple has no `ToSchema` and never will, and a multi-segment route is a
    /// perfectly ordinary thing to write. Unlike the body extractors, this one cannot simply
    /// require the bound.
    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Path,
            content_type: None,
            schema: None,
        })
    }
}

/// Deserializes the whole capture list: by name for structs and maps, by position for sequences,
/// and as the single captured value for a primitive.
struct PathDeserializer<'de> {
    params: &'de [(String, String)],
}

impl<'de> PathDeserializer<'de> {
    const fn new(params: &'de [(String, String)]) -> Self {
        Self { params }
    }

    /// The one captured parameter a primitive target needs, or an error naming the mismatch.
    fn single(&self) -> Result<ParamDeserializer<'de>, PathError> {
        match self.params {
            [(name, value)] => Ok(ParamDeserializer { name, value }),
            other => Err(PathError(format!(
                "the route captures {} parameters, but the handler asks for a single value",
                other.len()
            ))),
        }
    }
}

/// Forward a self-describing request to the single captured parameter.
macro_rules! forward_to_single {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                self.single()?.$method(visitor)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for PathDeserializer<'de> {
    type Error = PathError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // Without a target type to steer by, the capture list is a map of name to value.
        self.deserialize_map(visitor)
    }

    forward_to_single!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_identifier,
    );

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.params.is_empty() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_seq(SeqParams {
            params: self.params,
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if self.params.len() != len {
            return Err(PathError(format!(
                "the route captures {} parameters, but the handler asks for {len}",
                self.params.len()
            )));
        }
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_map(MapParams {
            params: self.params,
            value: None,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.single()?.deserialize_enum(name, variants, visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }
}

/// Walks the captures in the order the route declares them, for a tuple or sequence target.
struct SeqParams<'de> {
    params: &'de [(String, String)],
}

impl<'de> SeqAccess<'de> for SeqParams<'de> {
    type Error = PathError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some(((name, value), rest)) = self.params.split_first() else {
            return Ok(None);
        };
        self.params = rest;
        seed.deserialize(ParamDeserializer { name, value })
            .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.params.len())
    }
}

/// Walks the captures as name/value pairs, for a struct or map target.
struct MapParams<'de> {
    params: &'de [(String, String)],
    value: Option<&'de (String, String)>,
}

impl<'de> MapAccess<'de> for MapParams<'de> {
    type Error = PathError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((pair, rest)) = self.params.split_first() else {
            return Ok(None);
        };
        self.params = rest;
        self.value = Some(pair);
        seed.deserialize(pair.0.as_str().into_deserializer())
            .map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let (name, value) = self
            .value
            .take()
            .expect("serde calls next_value_seed only after next_key_seed yielded a key");
        seed.deserialize(ParamDeserializer { name, value })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.params.len())
    }
}

/// Deserializes one captured parameter, reporting its name in every parse failure.
#[derive(Clone, Copy)]
struct ParamDeserializer<'de> {
    name: &'de str,
    value: &'de str,
}

impl ParamDeserializer<'_> {
    fn parse<T: std::str::FromStr>(self, expected: &str) -> Result<T, PathError>
    where
        T::Err: Display,
    {
        self.value.parse().map_err(|error| {
            PathError(format!(
                "path parameter `{}` is not a valid {expected}: {error}",
                self.name
            ))
        })
    }
}

/// Parse the captured string into `$ty` and hand it to the visitor.
macro_rules! parse_param {
    ($($method:ident => $visit:ident($ty:ty)),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                visitor.$visit(self.parse::<$ty>(stringify!($ty))?)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for ParamDeserializer<'de> {
    type Error = PathError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // A captured segment is always text; the target type decides how to read it.
        visitor.visit_str(self.value)
    }

    parse_param!(
        deserialize_bool => visit_bool(bool),
        deserialize_i8 => visit_i8(i8),
        deserialize_i16 => visit_i16(i16),
        deserialize_i32 => visit_i32(i32),
        deserialize_i64 => visit_i64(i64),
        deserialize_i128 => visit_i128(i128),
        deserialize_u8 => visit_u8(u8),
        deserialize_u16 => visit_u16(u16),
        deserialize_u32 => visit_u32(u32),
        deserialize_u64 => visit_u64(u64),
        deserialize_u128 => visit_u128(u128),
        deserialize_f32 => visit_f32(f32),
        deserialize_f64 => visit_f64(f64),
        deserialize_char => visit_char(char),
    );

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_bytes(self.value.as_bytes())
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_bytes(self.value.as_bytes())
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_enum(UnitVariant { value: self.value })
    }

    forward_to_deserialize_any! {
        str string identifier unit unit_struct seq tuple tuple_struct map struct ignored_any
    }
}

/// A captured segment naming a unit variant, e.g. `/sort/{order}` into an `Order` enum.
struct UnitVariant<'de> {
    value: &'de str,
}

impl<'de> EnumAccess<'de> for UnitVariant<'de> {
    type Error = PathError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let variant = seed.deserialize(self.value.into_deserializer())?;
        Ok((variant, self))
    }
}

impl UnitVariant<'_> {
    /// A URL segment names a variant and nothing more, so any payload-carrying variant is out of
    /// reach whatever shape it has.
    fn carries_data(&self) -> PathError {
        PathError(format!(
            "path parameter value `{}` names a variant that carries data, which a single URL \
             segment cannot supply",
            self.value
        ))
    }
}

impl<'de> VariantAccess<'de> for UnitVariant<'de> {
    type Error = PathError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        _seed: T,
    ) -> Result<T::Value, Self::Error> {
        Err(self.carries_data())
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(self.carries_data())
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(self.carries_data())
    }
}

#[cfg(test)]
mod tests {
    use super::Path;
    use crate::{routing::Params, Body, Request, StatusCode};
    use http_kit::HttpError;
    use serde::Deserialize;
    use skyzen_core::Extractor;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Post {
        user: String,
        post: u32,
    }

    fn request_with(params: &[(&str, &str)]) -> Request {
        let mut request = Request::new(Body::empty());
        request.extensions_mut().insert(Params::new(
            params
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        ));
        request
    }

    #[tokio::test]
    async fn deserializes_a_single_primitive() {
        let mut request = request_with(&[("id", "42")]);
        let Path(id) = Path::<u64>::extract(&mut request).await.unwrap();
        assert_eq!(id, 42);
    }

    #[tokio::test]
    async fn deserializes_a_tuple_in_capture_order() {
        let mut request = request_with(&[("user", "ada"), ("post", "17")]);
        let Path((user, post)) = Path::<(String, u32)>::extract(&mut request).await.unwrap();
        assert_eq!(user, "ada");
        assert_eq!(post, 17);
    }

    #[tokio::test]
    async fn deserializes_a_named_struct() {
        let mut request = request_with(&[("user", "ada"), ("post", "17")]);
        let Path(post) = Path::<Post>::extract(&mut request).await.unwrap();
        assert_eq!(
            post,
            Post {
                user: "ada".to_owned(),
                post: 17
            }
        );
    }

    #[tokio::test]
    async fn a_type_mismatch_names_the_parameter() {
        let mut request = request_with(&[("user", "ada"), ("post", "seventeen")]);
        let error = Path::<Post>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let message = error.to_string();
        assert!(
            message.contains("`post`") && message.contains("u32"),
            "rejection should name the parameter and the type, got {message}"
        );
    }

    #[tokio::test]
    async fn a_tuple_of_the_wrong_arity_is_rejected() {
        let mut request = request_with(&[("id", "42")]);
        let error = Path::<(String, u32)>::extract(&mut request)
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(error.to_string().contains("asks for 2"), "{error}");
    }

    #[tokio::test]
    async fn a_primitive_needs_exactly_one_capture() {
        let mut request = request_with(&[("user", "ada"), ("post", "17")]);
        let error = Path::<u64>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(error.to_string().contains("single value"), "{error}");
    }
}
