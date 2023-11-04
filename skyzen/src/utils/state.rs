use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
use http_kit::{middleware::Next, Middleware, Request, Response, Result};
use skyzen_core::Extractor;

/// Share the state of application.
#[derive(Debug, Clone)]
pub struct State<T: Send + Sync + Clone + 'static>(pub T);

impl<T: Send + Sync + Clone + 'static> Deref for State<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Send + Sync + Clone + 'static> DerefMut for State<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[async_trait]
impl<T: Send + Sync + Clone + 'static> Extractor for State<T> {
    async fn extract(request: &mut Request) -> Result<Self> {
        request
            .get_extension()
            .ok_or(StateNotExist.into())
            .map(|state| {
                let state: &Self = state;
                state.clone()
            })
    }
}

#[async_trait]
impl<T: Send + Sync + Clone + 'static> Middleware for State<T> {
    async fn call_middleware(&self, request: &mut Request, next: Next<'_>) -> Result<Response> {
        request.insert_extension(self.clone());
        next.run(request).await
    }
}

impl_error!(
    StateNotExist,
    "This state does not exist",
    "Error occurs if cannot extract state"
);
