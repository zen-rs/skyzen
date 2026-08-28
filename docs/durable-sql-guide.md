# Durable Object + SQL Guide

This guide shows how to build a Skyzen Durable Object that:

- keeps per-object state
- stores relational data in `DurableDb`
- runs on Cloudflare Durable Objects
- can also be simulated on native with the same application code

The important mental model is:

- `Db` means your app-level default SQL database
- `DurableDb` means the SQL database scoped to one Durable Object instance
- a `DurableObject` is identified by object ID or object name
- each Durable Object instance gets its own `DurableDb`, `DurableKv`, alarms, and connection set

## When To Use DurableDb

Use `DurableDb` when your data is naturally scoped to one object instance, for example:

- one chat room
- one game lobby
- one collaborative document
- one shopping cart
- one tenant-local state machine

Use the top-level `Db` when the data is application-wide and should not be tied to a single object identity.

## Shared Durable Object Code

Your object code is the same on native and Cloudflare. The runtime changes, not the business logic.

```rust
use serde::{Deserialize, Serialize};
use skyzen::durable::DurableObject;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::{FromRow, Result};
use skyzen_services::durable::DurableDb;

#[derive(Default, Serialize, Deserialize)]
#[skyzen::durable_object]
struct Room;

impl DurableObject for Room {
    fn fetch(&mut self) -> Router {
        Route::new((
            "/join".post(join_room),
            "/members".get(list_members),
        ))
        .on_alarm(compact_room)
        .build()
    }
}

async fn join_room(db: DurableDb) -> Result<&'static str> {
    db.query(
        "CREATE TABLE IF NOT EXISTS members (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL
        )"
    )
    .execute()
    .await?;

    db.query("INSERT INTO members (id, name) VALUES (?, ?)")
        .bind("user-1")
        .bind("alice")
        .execute()
        .await?;

    Ok("joined")
}

#[derive(FromRow)]
struct Member {
    id: String,
    name: String,
}

async fn list_members(db: DurableDb) -> Result<skyzen::utils::Json<Vec<Member>>> {
    let members = db
        .query("SELECT id, name FROM members ORDER BY name")
        .fetch_all::<Member>()
        .await?;
    Ok(skyzen::utils::Json(members))
}

async fn compact_room(db: DurableDb) -> Result<&'static str> {
    db.query("DELETE FROM members WHERE name = ?")
        .bind("stale-user")
        .execute()
        .await?;
    Ok("ok")
}
```

## SQL Rules

`DurableDb` follows the same query-builder style as top-level `Db`:

- start with `query("...")`
- always pass values via `.bind(...)`
- use `?` placeholders
- finish with `execute`, `fetch_one`, `fetch_optional`, `fetch_all`, or — for a query selecting a
  single column — `fetch_scalar`

A row is decoded into a struct with one field per selected column, through `#[derive(FromRow)]`. A
query that selects one column needs no struct:

```rust
let count: i64 = db
    .query("SELECT COUNT(*) FROM members WHERE name = ?")
    .bind("alice")
    .fetch_scalar()
    .await?;
```

This keeps SQL injection protection in place on both native and serverless targets.

## Running On Cloudflare

On Cloudflare, your Durable Object is exported through `#[skyzen::durable_object]` and backed by `state.storage.sql`.

Typical shape:

```rust
use skyzen_cloudflare::CfDurableNamespace;

let namespace = CfDurableNamespace::from_env(&env, "ROOMS")?;
let stub = namespace.get_by_name("room:general")?;
let response = stub.fetch_url("https://room/join").await?;
```

`fetch_url` and `fetch` take and return Skyzen's own `Request` and `Response` — the same signature
the native simulator's `NativeDurableObjectStub` has — so a handler that talks to an object needs no
`web_sys` types and no `SendFuture` wrapper around itself. Both directions stream, and a `101`
answer carries its socket along in the response extensions, which is all a route has to forward to
turn a Durable Object into a websocket room.

The runtime injects:

- `DurableDb`
- `DurableKv`
- `Alarm`
- `DurableConnections`
- `WasmEnv`

for the current object instance.

If your Durable Object needs Cloudflare bindings or secrets inside `fetch` handlers or alarm handlers, extract `WasmEnv` directly:

```rust
use skyzen::runtime::wasm::WasmEnv;
use skyzen_services::durable::DurableDb;

async fn dispatch_job(env: WasmEnv, db: DurableDb) -> skyzen::Result<&'static str> {
    let env = env.into_inner();

    let github_token = skyzen_cloudflare::ffi::get_binding(&env, "GITHUB_TOKEN")
        .map_err(|error| skyzen::Error::msg(format!("missing GITHUB_TOKEN binding: {error:?}")))?;

    db.query("INSERT INTO job_log (status) VALUES (?)")
        .bind("dispatched")
        .execute()
        .await?;

    let _ = github_token;
    Ok("ok")
}
```

The same applies to `Route::on_alarm(...)` handlers inside a Durable Object router: `WasmEnv` is available there too.

## Running On Native

On native, use the built-in simulator:

- `NativeDurableNamespace<T>`
- `NativeDurableObjectStub<T>`

Each object gets:

- isolated persisted object state in memory
- isolated in-memory `DurableKv`
- isolated in-memory SQLite-backed `DurableDb`
- isolated alarm state
- its own registry of accepted WebSocket connections

Example:

```rust
use skyzen::durable::NativeDurableNamespace;
use skyzen::{Body, Method, Request};

let namespace = NativeDurableNamespace::<Room>::new();
let stub = namespace.get_by_name("room:general")?;

let mut join = Request::new(Body::empty());
*join.method_mut() = Method::POST;
*join.uri_mut() = "/join".parse().unwrap();

let response = stub.fetch(join).await?;
```

This is useful for:

- local development
- integration tests
- reproducing object-local bugs
- validating alarm logic without Cloudflare deployment

### WebSockets Off The Edge

`HibernationWebSocketUpgrade` is a responder on native too, so a room or relay object runs under
`skyzen dev` and under `cargo test` exactly as it does on Workers: forward the upgrade request into
the object with `stub.fetch(request)`, return what comes back, and the simulator drives
`DurableObject::websocket` for every frame, close and error.

Tags are real — `DurableConnections::all`, `by_tag` and the `broadcast_*` helpers see the sockets
the object actually accepted — and `set_auto_response` is answered before the handler is woken, the
way the platform answers it during hibernation. Each connection is driven by its own command
channel, and events are dispatched through the same per-object serialization as `fetch` and
`alarm`, so an object never sees two events at once.

A browser cannot put a header on a `WebSocket`, so the subprotocol list is its only in-band
credential channel. `HibernationWebSocketUpgrade::protocol` answers the offer verbatim, which
RFC 6455 §4.1 requires before the client will open the socket:

```rust
use skyzen::durable::HibernationWebSocketUpgrade;
use skyzen::websocket::{RequestedSubprotocols, WebSocketError};

async fn join(offered: RequestedSubprotocols) -> Result<HibernationWebSocketUpgrade, WebSocketError> {
    let token = offered
        .iter()
        .find_map(|protocol| protocol.strip_prefix("app.bearer."))
        .ok_or_else(|| WebSocketError::Protocol("no bearer subprotocol".to_owned()))?;
    // ... verify `token` ...

    let answer = offered
        .answer(|protocol| protocol.starts_with("app.bearer."))
        .ok_or_else(|| WebSocketError::Protocol("unanswerable subprotocol".to_owned()))?;
    Ok(HibernationWebSocketUpgrade::new().tag("room").protocol(answer))
}
```

## Object Identity

Choose object identity based on your domain:

- use `get_by_name("room:general")` when the object name is deterministic
- use `new_unique_id()` when the object must be allocated once and then referenced by ID

The key invariant is:

- requests that target the same object ID hit the same object-local database
- requests that target different object IDs get different databases

## Native + Serverless Architecture

The usual architecture is:

1. keep app-wide data in `Db`
2. keep per-object data in `DurableDb`
3. route object traffic through a namespace/stub
4. keep the actual `DurableObject` implementation shared

That gives you:

- one durable business implementation
- one SQL API style
- native simulation for development and tests
- serverless deployment for production

## Practical Pattern

A common split looks like this:

- app API receives `/rooms/{name}/join`
- app-level handler resolves the durable namespace
- handler forwards to the room object stub
- room object mutates its own `DurableDb`

This keeps:

- object-local ordering
- object-local relational state
- clean separation between app-wide and per-object data

## Limitations

The native simulator is intentionally a simulator, not a Cloudflare clone.

What it guarantees:

- same `DurableObject` trait
- same injected durable services
- same `DurableDb` query style
- per-object isolation
- alarm handler dispatch through `Route::on_alarm`
- WebSocket event dispatch, connection tags and auto-responses

What it does not try to emulate exactly:

- Cloudflare runtime internals
- hibernation itself: a native connection is a live socket held by the process, so an object is
  never evicted between frames and never restored from storage to answer one
- Cloudflare binding APIs

## Related Docs

- [Using Portable Services](services-guide.md)
- [Skyzen.toml Reference](skyzen-toml-reference.md)
- [Testing Guide](testing-guide.md)
