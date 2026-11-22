use core::future::{ready, Future};
use core::iter::FusedIterator;
use std::convert::Infallible;
use std::fmt;

use http_kit::{HttpError, Request, StatusCode};
use skyzen_core::Extractor;
use smallvec::SmallVec;

/// Inline UTF-8 capacity matching `size_of::<Box<str>>()` stack footprint so
/// ordinary route parameter names and values avoid heap allocation.
const INLINE_CAP: usize = 23;

#[derive(Clone)]
pub enum ParamStr {
    Inline { buf: [u8; INLINE_CAP], len: u8 },
    Heap(Box<str>),
}

impl ParamStr {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "guarded by `bytes.len() <= INLINE_CAP` (23)"
    )]
    pub fn from_str_slice(value: &str) -> Self {
        let bytes = value.as_bytes();
        if bytes.len() <= INLINE_CAP {
            let mut buf = [0u8; INLINE_CAP];
            buf[..bytes.len()].copy_from_slice(bytes);
            Self::Inline {
                buf,
                len: bytes.len() as u8,
            }
        } else {
            Self::Heap(value.into())
        }
    }

    pub fn from_string(value: String) -> Self {
        if value.len() <= INLINE_CAP {
            Self::from_str_slice(&value)
        } else {
            Self::Heap(value.into_boxed_str())
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Inline { buf, len } => {
                core::str::from_utf8(&buf[..usize::from(*len)]).expect("ParamStr holds UTF-8")
            }
            Self::Heap(s) => s,
        }
    }
}

impl fmt::Debug for ParamStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Captured route parameters for a matched request.
#[derive(Debug, Clone, Default)]
pub struct Params(SmallVec<[(ParamStr, ParamStr); 2]>);

/// Borrowed iterator over `(name, value)` pairs in [`Params`].
#[derive(Debug, Clone)]
pub struct ParamsIter<'a>(core::slice::Iter<'a, (ParamStr, ParamStr)>);

impl<'a> Iterator for ParamsIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for ParamsIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0
            .next_back()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl ExactSizeIterator for ParamsIter<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl FusedIterator for ParamsIter<'_> {}

/// Error returned when attempting to read a missing route parameter.
#[derive(Debug, Clone)]
pub struct MissingParam {
    name: String,
}

impl MissingParam {
    /// Create an error naming the parameter that was missing.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl fmt::Display for MissingParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Missing param `{}`", self.name)
    }
}

impl std::error::Error for MissingParam {}

impl HttpError for MissingParam {
    fn status(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl Params {
    pub(super) fn empty() -> Self {
        Self(SmallVec::new())
    }

    pub(super) fn push(&mut self, name: ParamStr, value: ParamStr) {
        self.0.push((name, value));
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let mut params = Self::empty();
        for (name, value) in pairs {
            params.push(
                ParamStr::from_str_slice(name),
                ParamStr::from_str_slice(value),
            );
        }
        params
    }

    /// Whether no route parameters were captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of captured route parameters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The captured parameters, in the order the route pattern declares them.
    ///
    /// [`Path<T>`](crate::extract::Path) deserializes from this iterator; reach for it directly
    /// only when parameter names are not known until runtime.
    #[must_use]
    pub fn pairs(&self) -> ParamsIter<'_> {
        ParamsIter(self.0.iter())
    }

    /// Get the route parameter by name.
    ///
    /// # Errors
    ///
    /// Returns [`MissingParam`] if the requested parameter is not present.
    pub fn get(&self, name: &str) -> Result<&str, MissingParam> {
        self.0
            .iter()
            .find_map(|(k, v)| {
                if k.as_str() == name {
                    Some(v.as_str())
                } else {
                    None
                }
            })
            .ok_or_else(|| MissingParam::new(name))
    }
}

impl Extractor for Params {
    type Error = Infallible;
    // Reading the params back out of the extensions is a synchronous clone, so the future is ready
    // on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        // Clone rather than remove so the params survive repeated extraction.
        ready(Ok(request
            .extensions()
            .get::<Self>()
            .cloned()
            .unwrap_or_else(Self::empty)))
    }

    // Individual path parameters are derived from the route's `{name}` segments when the OpenAPI
    // document is built, so the whole-map `Params` extractor contributes no schema of its own.
}
