//! Durable Object context for non-routed methods.

use skyzen_services::durable::{Alarm, DurableKv, DurableSql};

use super::connections::DurableConnections;
use super::id::DurableObjectId;

/// Provides service access in `websocket` and other non-routed contexts.
///
/// NOT an extractor — passed explicitly by the runtime to
/// [`DurableObject::websocket`](super::DurableObject::websocket).
pub struct DurableContext {
    kv: DurableKv,
    sql: DurableSql,
    alarm: Alarm,
    connections: DurableConnections,
    id: DurableObjectId,
}

impl std::fmt::Debug for DurableContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableContext")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl DurableContext {
    /// Create a new `DurableContext` with all services.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Box<dyn> fields prevent const
    pub fn new(
        kv: DurableKv,
        sql: DurableSql,
        alarm: Alarm,
        connections: DurableConnections,
        id: DurableObjectId,
    ) -> Self {
        Self {
            kv,
            sql,
            alarm,
            connections,
            id,
        }
    }

    /// Access the Durable Object's KV storage.
    #[must_use]
    pub const fn kv(&self) -> &DurableKv {
        &self.kv
    }

    /// Access the Durable Object's SQL storage.
    #[must_use]
    pub const fn sql(&self) -> &DurableSql {
        &self.sql
    }

    /// Access the Durable Object's alarm scheduler.
    #[must_use]
    pub const fn alarm(&self) -> &Alarm {
        &self.alarm
    }

    /// Access the Durable Object's WebSocket connections.
    #[must_use]
    pub const fn connections(&self) -> &DurableConnections {
        &self.connections
    }

    /// The unique identity of this Durable Object instance.
    #[must_use]
    pub const fn id(&self) -> &DurableObjectId {
        &self.id
    }
}
