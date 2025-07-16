use std::ops::{Deref, DerefMut};

use http_kit::{Middleware, Request, Response, Result};
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

impl<T: Send + Sync + Clone + 'static> Middleware for State<T> {
    async fn handle(
        &mut self,
        request: &mut Request,
        mut next: impl http_kit::Endpoint,
    ) -> Result<Response> {
        request.insert_extension(self.clone());
        next.respond(request).await
    }
}

impl_error!(
    StateNotExist,
    "This state does not exist",
    "Error occurs if cannot extract state"
);
