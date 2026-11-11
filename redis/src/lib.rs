//! Redis implementation of [`KeyValueStore`] for the Skyzen framework.
//!
//! This crate provides a [`Redis`] type that implements the [`KeyValueStore`] trait
//! from `skyzen-services`, enabling Redis as a key-value backend.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_redis::Redis;
//! use skyzen_services::Kv;
//!
//! let redis = Redis::from_env().await?; // or Redis::connect("redis://127.0.0.1/")
//! let kv = Kv::new(redis);
//! kv.put("key", b"value").await?;
//! ```
//!
//! # Runtime
//!
//! The connection runs on redis's smol runtime, whose sockets and timers are async-io's, so a
//! [`Redis`] works under any executor — compio's thread-per-core runtime included — and needs no
//! tokio context.
//!
//! # Feature Flags
//!
//! - `tls` — `rediss://` URLs, through rustls

use std::sync::Arc;

use redis::aio::ConnectionManager;
use redis::{
    AsyncCommands, Client, ErrorKind, ExistenceCheck, RedisError, Script, ServerErrorKind,
    SetOptions,
};
use skyzen_services::kv::{KeyValueStore, KvError, KvListOptions, KvListResult};

/// How many keys one `SCAN` step asks Redis to examine.
///
/// `COUNT` is a hint about work done per step, not a result count; 100 is Redis' own default
/// trade-off between round trips and how long a single step blocks the server.
const SCAN_COUNT: usize = 100;

/// The environment variable [`Redis::from_env`] reads its URL from.
const URL_ENV: &str = "REDIS_URL";

/// The compare-and-swap script, kept in its own file so the Lua stays readable and reviewable.
const COMPARE_AND_SWAP: &str = include_str!("compare_and_swap.lua");

/// The `INCRBY` + `PEXPIRE` pair [`KeyValueStore::increment_with_ttl`] runs as one script.
const INCREMENT_WITH_TTL: &str = include_str!("increment_with_ttl.lua");

/// The reply code [`INCREMENT_WITH_TTL`] raises on a stored negative or non-integer value.
const CORRUPT_COUNTER_CODE: &str = "CORRUPT";

/// The [`COMPARE_AND_SWAP`] flag meaning "the key must hold the expected bytes".
const EXPECTS_VALUE: &str = "1";

/// The [`COMPARE_AND_SWAP`] flag meaning "the key must be absent".
const EXPECTS_ABSENT: &str = "0";

/// What [`COMPARE_AND_SWAP`] returns when it applied the swap.
const SWAP_APPLIED: i64 = 1;

/// Redis' error code for a command applied to a key holding another type.
const WRONG_TYPE_CODE: &str = "WRONGTYPE";

/// Details Redis returns from `INCRBY` when the key is not usable as a counter.
///
/// Redis gives neither its own error kind: a non-numeric string comes back as a plain `ERR` and an
/// out-of-range result as another, so the detail text is the only thing separating "this is not a
/// counter" from a genuine backend failure.
const NOT_A_COUNTER_DETAILS: [&str; 2] = ["not an integer", "would overflow"];

/// A Redis-backed key-value store.
///
/// Wraps a [`ConnectionManager`] that automatically reconnects on connection failures.
/// Cloning is cheap — it shares the underlying connection.
///
/// # Connection model
///
/// One auto-reconnecting *multiplexed* connection, not a pool: every operation pipelines onto the
/// same socket. That is correct for the commands this trait issues, none of which hold the
/// connection, but it means there is no pool size to tune and no path to a blocking command or a
/// `WATCH`/`MULTI` transaction — [`compare_and_swap`](KeyValueStore::compare_and_swap) uses a
/// script for exactly that reason.
///
/// # Out of scope
///
/// Redis Cluster and Sentinel are **not** supported. This type speaks to a single endpoint, so a
/// cluster URL reaches whichever node it names and any command whose key hashes to another slot
/// comes back as a `MOVED` error rather than being followed. Use a cluster-aware proxy in front,
/// or an implementation built on the upstream cluster client.
#[derive(Debug, Clone)]
pub struct Redis {
    conn: ConnectionManager,
    /// Shared so cloning a `Redis` per request does not re-hash the script body each time; the
    /// script is immutable, so an `Arc` is all the sharing it needs.
    compare_and_swap: Arc<Script>,
    /// Same sharing story as [`Self::compare_and_swap`].
    increment_with_ttl: Arc<Script>,
}

impl Redis {
    /// Connect to Redis using a URL (e.g. `redis://127.0.0.1/`).
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Backend`] if the connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self, KvError> {
        let client = Client::open(url).map_err(backend_error)?;
        install_tls_provider(&client);
        let conn = client
            .get_connection_manager()
            .await
            .map_err(backend_error)?;
        Ok(Self::from_connection_manager(conn))
    }

    /// Connect to Redis using the URL in `REDIS_URL`.
    ///
    /// Matches the `from_env` constructor every other Skyzen backend offers, so a deployment
    /// configures Redis the same way it configures S3 or `DynamoDB`. Credentials and TLS travel in
    /// the URL, as `rediss://user:password@host:6379/0` — a `rediss://` URL needs the `tls`
    /// feature, which is what turns the scheme into an actual rustls handshake.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Backend`] if `REDIS_URL` is unset or the connection cannot be
    /// established.
    pub async fn from_env() -> Result<Self, KvError> {
        let url = std::env::var(URL_ENV).map_err(|error| {
            KvError::backend_with(
                format!(
                    "{URL_ENV} is not set; it must hold a Redis URL such as `redis://127.0.0.1/`"
                ),
                error,
            )
        })?;
        Self::connect(&url).await
    }

    /// Create a `Redis` instance from an existing [`ConnectionManager`].
    #[must_use]
    pub fn from_connection_manager(conn: ConnectionManager) -> Self {
        Self {
            conn,
            compare_and_swap: Arc::new(Script::new(COMPARE_AND_SWAP)),
            increment_with_ttl: Arc::new(Script::new(INCREMENT_WITH_TTL)),
        }
    }
}

/// Build a `SCAN MATCH` pattern that matches exactly the keys starting with `prefix`.
///
/// Redis glob metacharacters (`*`, `?`, `[`, `]`, `\`) occurring in the prefix
/// are escaped with a backslash so they match literally.
fn scan_pattern(prefix: Option<&str>) -> String {
    prefix.map_or_else(
        || "*".to_owned(),
        |p| {
            let mut pattern = String::with_capacity(p.len() + 1);
            for c in p.chars() {
                if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                    pattern.push('\\');
                }
                pattern.push(c);
            }
            pattern.push('*');
            pattern
        },
    )
}

/// Parse a caller-supplied continuation token back into a Redis `SCAN` cursor.
///
/// The token is Redis' own cursor rendered as decimal, so anything else was not produced by
/// [`KeyValueStore::list`] and is rejected rather than silently restarting the scan.
fn parse_scan_cursor(cursor: Option<&str>) -> Result<u64, KvError> {
    cursor.map_or(Ok(0), |cursor| {
        cursor.parse().map_err(|_| {
            KvError::backend(format!(
                "list cursor {cursor:?} is not a Redis SCAN cursor; pass back the cursor from the previous page"
            ))
        })
    })
}

/// Convert a TTL duration to whole milliseconds for `PSETEX`/`SET PX`.
///
/// Sub-millisecond durations are rounded up to 1 ms. Zero and overflowing
/// durations are rejected because Redis requires a positive expiry.
fn ttl_millis(ttl: core::time::Duration) -> Result<u64, KvError> {
    if ttl.is_zero() {
        return Err(KvError::backend("TTL must be greater than zero"));
    }
    let millis = ttl.as_millis().max(1);
    u64::try_from(millis)
        .map_err(|_| KvError::backend(format!("TTL of {millis} ms exceeds the supported maximum")))
}

/// Convert a TTL duration to the signed milliseconds `PEXPIRE` takes.
fn expire_millis(ttl: core::time::Duration) -> Result<i64, KvError> {
    let millis = ttl_millis(ttl)?;
    i64::try_from(millis).map_err(|_| {
        KvError::backend(format!(
            "TTL of {millis} ms exceeds the millisecond range PEXPIRE accepts"
        ))
    })
}

/// Map a Redis error onto the portable error taxonomy.
///
/// Only the unambiguous cases are narrowed. `BUSY`/`LOADING` and `TRYAGAIN` mean the server is
/// occupied and the caller should come back; a failed `AUTH` and a denied command mean the
/// credentials are wrong and retrying cannot help. Everything else keeps its message *and* its
/// source, because guessing wrong about retryability turns a hard failure into a retry storm.
fn backend_error(error: RedisError) -> KvError {
    match error.kind() {
        ErrorKind::Server(ServerErrorKind::BusyLoading | ServerErrorKind::TryAgain) => {
            KvError::Throttled { retry_after: None }
        }
        ErrorKind::AuthenticationFailed | ErrorKind::Server(ServerErrorKind::NoPerm) => {
            KvError::Unauthorized
        }
        _ => KvError::backend_with(error.to_string(), error),
    }
}

/// Whether an `INCRBY` failure means the key does not hold a counter.
///
/// Redis says that two ways and gives neither its own [`ErrorKind`]: a `WRONGTYPE` code when the
/// key holds a list, hash or set, and a plain `ERR` detailed "value is not an integer or out of
/// range" when it holds a non-numeric string or the result would leave the `i64` range. The code
/// and the detail are therefore all that separates them from a genuine backend failure, which is
/// why this takes them as data — the classification is what the unit tests exercise, against the
/// exact strings Redis sends.
fn is_not_a_counter(code: Option<&str>, detail: Option<&str>) -> bool {
    code == Some(WRONG_TYPE_CODE)
        || code == Some(CORRUPT_COUNTER_CODE)
        || detail.is_some_and(|detail| {
            NOT_A_COUNTER_DETAILS
                .iter()
                .any(|known| detail.contains(known))
        })
}

/// Map an `INCRBY` failure, reporting "the key does not hold a counter" as a decode error.
///
/// [`KvError::Decode`] is what the trait documents for a non-integer value and what `InMemoryKv`
/// and `DynamoKv` both return, so portable code sees one behaviour on every backend.
fn counter_error(error: RedisError) -> KvError {
    if is_not_a_counter(error.code(), error.detail()) {
        KvError::Decode(error.to_string())
    } else {
        backend_error(error)
    }
}

impl KeyValueStore for Redis {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let mut conn = self.conn.clone();
        conn.get(key).await.map_err(backend_error)
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let mut conn = self.conn.clone();
        conn.set(key, value).await.map_err(backend_error)
    }

    async fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError> {
        let millis = ttl_millis(ttl)?;
        let mut conn = self.conn.clone();
        conn.pset_ex(key, value, millis)
            .await
            .map_err(backend_error)
    }

    /// Take the key only if nothing holds it, with `SET NX`.
    ///
    /// `SET NX` answers with the stored string when it wrote and with nil when the key was already
    /// taken, which is exactly the distributed-lock primitive this method promises.
    async fn put_if_absent(&self, key: &str, value: &[u8]) -> Result<bool, KvError> {
        let mut conn = self.conn.clone();
        let applied: Option<String> = conn
            .set_options(
                key,
                value,
                SetOptions::default().conditional_set(ExistenceCheck::NX),
            )
            .await
            .map_err(backend_error)?;
        Ok(applied.is_some())
    }

    /// Swap the value under `key` with a Lua script that compares and writes in one step.
    ///
    /// Redis has no compare-and-swap command, and a `GET` followed by a `SET` leaves a window for
    /// another client. A script closes it: Redis runs it to completion before serving anyone else.
    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: &[u8],
    ) -> Result<bool, KvError> {
        let mut conn = self.conn.clone();
        let mut invocation = self.compare_and_swap.prepare_invoke();
        invocation
            .key(key)
            .arg(if expected.is_some() {
                EXPECTS_VALUE
            } else {
                EXPECTS_ABSENT
            })
            // Unread by the script when the flag says the key must be absent, but Redis needs the
            // argument to exist either way.
            .arg(expected.unwrap_or_default())
            .arg(new);

        let result: i64 = invocation
            .invoke_async(&mut conn)
            .await
            .map_err(backend_error)?;

        Ok(result == SWAP_APPLIED)
    }

    /// Add `delta` to a counter with `INCRBY`, which Redis applies atomically.
    ///
    /// `INCRBY` treats a missing key as zero, so the first call yields `delta`, and it keeps
    /// whatever expiry the key already carried — a rate-limit window still closes on schedule.
    ///
    /// # Errors
    ///
    /// [`KvError::Decode`] when the key holds something that is not a counter, or when the result
    /// would leave the `i64` range.
    async fn increment(&self, key: &str, delta: i64) -> Result<i64, KvError> {
        let mut conn = self.conn.clone();
        conn.incr(key, delta).await.map_err(counter_error)
    }

    /// `INCRBY` + `PEXPIRE` as one script — a counter is born carrying its deadline, so a
    /// transient failure between the two plain commands can never leave a TTL-less record.
    ///
    /// # Errors
    ///
    /// [`KvError::Decode`] when the key holds something that is not a counter, or when the
    /// result would leave the `i64` range.
    async fn increment_with_ttl(
        &self,
        key: &str,
        delta: i64,
        ttl: core::time::Duration,
    ) -> Result<i64, KvError> {
        let millis = expire_millis(ttl)?;
        let mut conn = self.conn.clone();
        self.increment_with_ttl
            .key(key)
            .arg(delta)
            .arg(millis)
            .invoke_async(&mut conn)
            .await
            .map_err(counter_error)
    }

    /// Re-arm a key's expiry with `PEXPIRE`, at the same millisecond precision as
    /// [`put_with_ttl`](KeyValueStore::put_with_ttl).
    ///
    /// `PEXPIRE` answers `0` when no such key exists, which is the `false` this method reports.
    async fn expire(&self, key: &str, ttl: core::time::Duration) -> Result<bool, KvError> {
        let millis = expire_millis(ttl)?;
        let mut conn = self.conn.clone();
        conn.pexpire(key, millis).await.map_err(backend_error)
    }

    /// Answer with `EXISTS`, which never transfers the value.
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        let mut conn = self.conn.clone();
        conn.exists(key).await.map_err(backend_error)
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let mut conn = self.conn.clone();
        conn.del(key).await.map_err(backend_error)
    }

    /// List one page of keys using Redis' own `SCAN` cursor.
    ///
    /// A `SCAN` step returns "up to about `COUNT`" keys — it may return fewer, more, or none at
    /// all while still having work left — so the loop keeps stepping until the requested `limit`
    /// is reached or the cursor comes back as `0`. The cursor handed to the caller is Redis'
    /// cursor verbatim.
    ///
    /// Redis makes no de-duplication guarantee across a full `SCAN`: a key present for the whole
    /// scan is returned at least once, but keys can repeat when the keyspace is rehashed
    /// mid-scan.
    async fn list(&self, options: KvListOptions) -> Result<KvListResult, KvError> {
        let mut conn = self.conn.clone();
        let pattern = scan_pattern(options.prefix.as_deref());
        let mut cursor = parse_scan_cursor(options.cursor.as_deref())?;
        let mut keys = Vec::new();

        loop {
            let (next, page): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(SCAN_COUNT)
                .query_async(&mut conn)
                .await
                .map_err(backend_error)?;

            keys.extend(page);
            cursor = next;

            if cursor == 0 {
                return Ok(KvListResult { keys, cursor: None });
            }
            if options.limit.is_some_and(|limit| keys.len() >= limit) {
                return Ok(KvListResult {
                    keys,
                    cursor: Some(cursor.to_string()),
                });
            }
        }
    }
}

/// redis-rs builds its client config through `ClientConfig::builder()`, which
/// picks the *process-default* `CryptoProvider` — ambiguous, and therefore a
/// panic, when another dependency also enables `ring`. Pin aws-lc-rs once, and
/// only for a `rediss://` address: a plaintext connect must not decide the
/// provider for unrelated TLS clients in the process. An `Err` from
/// `install_default` only means a default already exists — the app's own
/// choice wins, which is fine.
#[cfg(feature = "tls")]
fn install_tls_provider(client: &redis::Client) {
    if matches!(
        client.get_connection_info().addr(),
        redis::ConnectionAddr::TcpTls { .. }
    ) {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let _ = rustls::crypto::CryptoProvider::install_default(
                rustls::crypto::aws_lc_rs::default_provider(),
            );
        });
    }
}

#[cfg(not(feature = "tls"))]
const fn install_tls_provider(_client: &redis::Client) {}

#[cfg(test)]
mod tests {
    use super::{
        backend_error, counter_error, expire_millis, is_not_a_counter, parse_scan_cursor,
        scan_pattern, ttl_millis, COMPARE_AND_SWAP, EXPECTS_ABSENT, EXPECTS_VALUE, SWAP_APPLIED,
        WRONG_TYPE_CODE,
    };
    use core::time::Duration;
    use redis::{ErrorKind, RedisError, Script, ServerErrorKind};
    use skyzen_services::kv::KvError;

    /// The error Redis returns for a `-ERR <detail>` reply, which is what `INCRBY` sends when the
    /// key holds a string it cannot count with.
    fn err_reply(detail: &str) -> RedisError {
        RedisError::from((
            ErrorKind::Server(ServerErrorKind::ResponseError),
            "an error was signalled by the server",
            detail.to_owned(),
        ))
    }

    #[test]
    fn absent_cursor_starts_a_fresh_scan() {
        assert_eq!(parse_scan_cursor(None).unwrap(), 0);
    }

    #[test]
    fn cursor_round_trips_through_its_decimal_rendering() {
        assert_eq!(parse_scan_cursor(Some("17408")).unwrap(), 17408);
    }

    #[test]
    fn a_cursor_from_another_backend_is_rejected_rather_than_restarting() {
        assert!(parse_scan_cursor(Some("eyJrZXkiOiJhIn0=")).is_err());
    }

    #[test]
    fn scan_pattern_without_prefix_matches_everything() {
        assert_eq!(scan_pattern(None), "*");
    }

    #[test]
    fn scan_pattern_appends_wildcard_to_plain_prefix() {
        assert_eq!(scan_pattern(Some("user:")), "user:*");
    }

    #[test]
    fn scan_pattern_escapes_glob_metacharacters() {
        assert_eq!(scan_pattern(Some("a*b")), r"a\*b*");
        assert_eq!(scan_pattern(Some("a?b")), r"a\?b*");
        assert_eq!(scan_pattern(Some("a[1]b")), r"a\[1\]b*");
        assert_eq!(scan_pattern(Some(r"a\b")), r"a\\b*");
        assert_eq!(scan_pattern(Some(r"*?[]\")), r"\*\?\[\]\\*");
    }

    #[test]
    fn ttl_millis_rejects_zero() {
        assert!(ttl_millis(Duration::ZERO).is_err());
    }

    #[test]
    fn ttl_millis_rounds_sub_millisecond_up_to_one() {
        assert_eq!(ttl_millis(Duration::from_micros(50)).unwrap(), 1);
    }

    #[test]
    fn ttl_millis_converts_whole_durations() {
        assert_eq!(ttl_millis(Duration::from_secs(2)).unwrap(), 2000);
    }

    #[test]
    fn ttl_millis_rejects_overflowing_durations() {
        assert!(ttl_millis(Duration::MAX).is_err());
    }

    #[test]
    fn expire_millis_matches_ttl_millis_inside_the_signed_range() {
        assert_eq!(expire_millis(Duration::from_secs(2)).unwrap(), 2000);
        assert_eq!(expire_millis(Duration::from_micros(50)).unwrap(), 1);
        assert!(expire_millis(Duration::ZERO).is_err());
        assert!(expire_millis(Duration::MAX).is_err());
        // Past `i64::MAX` milliseconds `ttl_millis` still succeeds, so this is the extra guard.
        let beyond_signed = Duration::from_millis(u64::MAX);
        assert!(ttl_millis(beyond_signed).is_ok());
        assert!(expire_millis(beyond_signed).is_err());
    }

    /// The script has to actually parse as Lua and use the flags the Rust side sends, otherwise
    /// every `compare_and_swap` fails on the first call against a real server.
    #[test]
    fn the_compare_and_swap_script_agrees_with_the_arguments_rust_sends() {
        // The comment block explains the argument contract in prose, so every assertion below runs
        // against the code alone — otherwise a script whose body stopped reading `ARGV[2]` would
        // still pass on the strength of its documentation.
        let code = COMPARE_AND_SWAP
            .lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(code.contains("KEYS[1]"));
        for argv in ["ARGV[1]", "ARGV[2]", "ARGV[3]"] {
            assert!(code.contains(argv), "the script body never reads {argv}");
        }
        assert!(code.contains(&format!("ARGV[1] == '{EXPECTS_VALUE}'")));
        assert!(code.contains(&format!("return {SWAP_APPLIED}")));
        assert_ne!(EXPECTS_VALUE, EXPECTS_ABSENT);

        // Balanced control flow: one `end` per `if`, and every branch returns.
        let keyword = |word: &str| {
            code.split_whitespace()
                .filter(|token| *token == word)
                .count()
        };
        assert_eq!(keyword("if"), keyword("end"));
        assert_eq!(keyword("return"), 3);

        // `Script::new` hashes the body; a stable hash is what makes EVALSHA reuse it.
        let script = Script::new(COMPARE_AND_SWAP);
        assert_eq!(
            script.get_hash(),
            Script::new(COMPARE_AND_SWAP).get_hash(),
            "the script hash must be stable across instances"
        );
        assert_eq!(script.get_hash().len(), 40);
    }

    /// The exact replies Redis 7 sends for `INCRBY` against a key it cannot count with.
    #[test]
    fn a_key_that_is_not_a_counter_is_reported_as_a_decode_error() {
        assert!(is_not_a_counter(
            Some(WRONG_TYPE_CODE),
            Some("Operation against a key holding the wrong kind of value")
        ));
        assert!(is_not_a_counter(
            Some("ERR"),
            Some("value is not an integer or out of range")
        ));
        assert!(is_not_a_counter(
            Some("ERR"),
            Some("increment or decrement would overflow")
        ));

        // A WRONGTYPE reply is classified on its code alone, whatever detail rides along.
        assert!(is_not_a_counter(Some(WRONG_TYPE_CODE), None));

        for detail in [
            "value is not an integer or out of range",
            "increment or decrement would overflow",
        ] {
            assert!(matches!(
                counter_error(err_reply(detail)),
                KvError::Decode(_)
            ));
        }
    }

    #[test]
    fn an_ordinary_command_failure_stays_a_backend_error() {
        assert!(!is_not_a_counter(
            Some("ERR"),
            Some("unknown command 'FLUX'")
        ));
        assert!(!is_not_a_counter(Some("ERR"), None));
        assert!(!is_not_a_counter(None, None));

        assert!(matches!(
            counter_error(err_reply("unknown command 'FLUX'")),
            KvError::Backend { .. }
        ));
        // A counter command that fails while the server is loading is still retryable, not a
        // decode problem.
        assert!(matches!(
            counter_error(RedisError::from((
                ErrorKind::Server(ServerErrorKind::BusyLoading),
                "loading the dataset in memory",
            ))),
            KvError::Throttled { .. }
        ));
    }

    #[test]
    fn only_the_unambiguous_server_states_are_narrowed() {
        let busy = RedisError::from((
            ErrorKind::Server(ServerErrorKind::BusyLoading),
            "loading the dataset in memory",
        ));
        assert!(matches!(backend_error(busy), KvError::Throttled { .. }));

        let try_again = RedisError::from((
            ErrorKind::Server(ServerErrorKind::TryAgain),
            "multi-key operation in progress",
        ));
        assert!(matches!(
            backend_error(try_again),
            KvError::Throttled { .. }
        ));

        let denied = RedisError::from((ErrorKind::AuthenticationFailed, "invalid password"));
        assert!(matches!(backend_error(denied), KvError::Unauthorized));

        let no_perm = RedisError::from((
            ErrorKind::Server(ServerErrorKind::NoPerm),
            "this user has no permissions to run the 'get' command",
        ));
        assert!(matches!(backend_error(no_perm), KvError::Unauthorized));

        // A moved slot is a cluster topology problem, not a rate limit — this type does not follow
        // redirects, so it must surface as a backend error rather than as a retryable throttle.
        let moved = RedisError::from((
            ErrorKind::Server(ServerErrorKind::Moved),
            "the key has moved to another node",
        ));
        assert!(matches!(backend_error(moved), KvError::Backend { .. }));

        let io = RedisError::from((ErrorKind::Io, "connection reset"));
        assert!(matches!(backend_error(io), KvError::Backend { .. }));
    }

    /// `Client::open` is where a `rediss://` URL used to die — with `tls` on,
    /// the scheme parses to a TLS connection instead of failing.
    #[test]
    #[cfg(feature = "tls")]
    fn a_rediss_url_parses_with_tls_enabled() {
        assert!(redis::Client::open("rediss://user:password@127.0.0.1:6379/0").is_ok());
    }

    /// Without `tls` the provider install is a no-op, so `builder()` — the
    /// call upstream's connect path makes — still panics in this test's
    /// dual-provider world. That is the disabled state, observable: `connect`
    /// installs nothing the caller did not ask for.
    #[test]
    #[cfg(not(feature = "tls"))]
    fn without_tls_no_provider_is_installed() {
        let client = redis::Client::open("rediss://127.0.0.1:6379/0").expect("parses");
        super::install_tls_provider(&client);
        assert!(
            std::panic::catch_unwind(rustls::ClientConfig::builder).is_err(),
            "builder() must still panic — no provider was installed"
        );
    }

    /// `builder()` resolves through the process default — exactly what redis's
    /// connect path calls — and must not panic even with two providers on the
    /// shared rustls: the dev-dependency's `ring` puts this test in that world.
    /// `connect` installs aws-lc-rs for a `rediss://` address first.
    #[test]
    #[cfg(feature = "tls")]
    fn the_process_default_provider_is_installed_for_rediss() {
        let client = redis::Client::open("rediss://127.0.0.1:6379/0").expect("parses");
        super::install_tls_provider(&client);
        let _ = rustls::ClientConfig::builder();
    }

    /// A throwaway TLS peer on a std thread: a rustls server with a self-signed cert that
    /// completes the handshake, then answers the client's RESP commands (`+OK` for each
    /// `CLIENT`, `+PONG` for each `PING`, which is all redis-rs's connection setup sends for a URL
    /// without a password or database) until the client closes, and returns the decrypted bytes
    /// it received. The handle yields `Err` if the handshake fails (the client rejecting the
    /// certificate surfaces here) or any later I/O fails; the client closing the socket after the
    /// handshake is the normal end of the session.
    #[cfg(all(feature = "tls", unix))]
    fn self_signed_peer() -> (u16, std::thread::JoinHandle<std::io::Result<Vec<u8>>>) {
        let cert =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("self-signed cert");
        let config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .expect("server config");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept()?;
            let mut conn = rustls::ServerConnection::new(std::sync::Arc::new(config))
                .map_err(std::io::Error::other)?;
            while conn.is_handshaking() {
                conn.complete_io(&mut sock)?;
            }
            serve_setup(&mut rustls::StreamOwned::new(conn, sock))
        });
        (port, handle)
    }

    /// Answer the RESP commands redis-rs's connection setup sends over `stream` (`+OK` for each
    /// `CLIENT`, `+PONG` for each `PING`) until the peer closes it, and return the bytes read.
    fn serve_setup(stream: &mut (impl std::io::Read + std::io::Write)) -> std::io::Result<Vec<u8>> {
        use std::io::ErrorKind;

        fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
            haystack
                .windows(needle.len())
                .filter(|window| *window == needle)
                .count()
        }

        let mut received = Vec::new();
        let (mut clients, mut pings) = (0, 0);
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                // A plain TCP close after a TLS handshake carries no close_notify.
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
            let seen_clients = occurrences(&received, b"CLIENT");
            let seen_pings = occurrences(&received, b"PING");
            for _ in clients..seen_clients {
                stream.write_all(b"+OK\r\n")?;
            }
            for _ in pings..seen_pings {
                stream.write_all(b"+PONG\r\n")?;
            }
            (clients, pings) = (seen_clients, seen_pings);
            stream.flush()?;
        }
        Ok(received)
    }

    /// A connection opens its socket and completes its setup on redis's smol runtime under a
    /// compio runtime, with no tokio runtime anywhere: a plain TCP peer on a std thread answers
    /// the RESP the connection manager sends on connect, and receives it.
    #[compio::test]
    async fn a_connection_writes_its_handshake_on_compio_without_a_tokio_runtime() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the test must run with no tokio runtime"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept()?;
            serve_setup(&mut socket)
        });

        // The same manager `connect` builds, with its 1 s per-attempt bound lifted: under load an
        // attempt can outlast it, and the peer serves exactly one connection. The outer 60 s
        // bound only guards against a hang.
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}/")).expect("parses");
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Some(std::time::Duration::from_secs(60)))
            .set_response_timeout(Some(std::time::Duration::from_secs(60)));
        let manager = compio::time::timeout(
            std::time::Duration::from_secs(60),
            client.get_connection_manager_with_config(config),
        )
        .await
        .expect("connect hung for 60 s")
        .expect("connect to a peer that answers its setup must succeed");
        // Closing the client ends the peer's session.
        drop(super::Redis::from_connection_manager(manager));
        let received = peer
            .join()
            .expect("peer thread panicked")
            .expect("peer I/O failed");
        assert_eq!(
            received.first(),
            Some(&b'*'),
            "the peer should receive a RESP command array, got {received:?}"
        );
    }

    /// `#insecure` turns off verification upstream, so a self-signed peer is a
    /// real handshake: `connect` must return `Ok`, and the peer must have
    /// received the decrypted RESP commands redis's connection setup sends
    /// (`CLIENT SETINFO`), proving the TLS session carried data.
    #[compio::test]
    #[cfg(all(feature = "tls", unix))]
    async fn a_rediss_connection_completes_a_tls_handshake() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the test must run with no tokio runtime"
        );
        let (port, peer) = self_signed_peer();
        let url = format!("rediss://127.0.0.1:{port}/#insecure");
        // `Redis::connect` uses redis-rs's defaults: each connection attempt (TCP, TLS handshake
        // and setup, including loading the system roots) is cut off after 1 s and retried, and the
        // peer serves exactly one connection. Under load an attempt can outlast that second, so
        // the test builds the same manager `connect` does with the attempt bound lifted; the
        // outer 60 s bound only guards against a hang.
        let client = redis::Client::open(url).expect("parses");
        super::install_tls_provider(&client);
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Some(std::time::Duration::from_secs(60)))
            .set_response_timeout(Some(std::time::Duration::from_secs(60)));
        let manager = compio::time::timeout(
            std::time::Duration::from_secs(60),
            client.get_connection_manager_with_config(config),
        )
        .await
        .expect("connect hung for 60 s")
        .expect("connect over a self-signed peer with #insecure must succeed");
        let redis = super::Redis::from_connection_manager(manager);
        // Closing the client ends the peer's session.
        drop(redis);
        let decrypted = peer
            .join()
            .expect("peer thread panicked")
            .expect("peer I/O failed");
        assert!(
            decrypted.windows(6).any(|window| window == b"CLIENT"),
            "the peer should have received the setup commands, got {:?}",
            String::from_utf8_lossy(&decrypted)
        );
    }

    /// Without `#insecure` the same self-signed peer must be *rejected*:
    /// certificate verification is on and rustls-native-certs does not know
    /// it. A single-shot connect, not the manager — the manager retries a
    /// failed handshake forever, and "keeps retrying" is not the failure being
    /// asserted here.
    #[compio::test]
    #[cfg(all(feature = "tls", unix))]
    async fn a_rediss_connection_rejects_an_untrusted_certificate() {
        let (port, peer) = self_signed_peer();
        let url = format!("rediss://127.0.0.1:{port}/");
        let client = redis::Client::open(url).expect("parses");
        // Same provider install `connect` performs — a `rediss://` client
        // without it panics on `builder()` in a multi-provider world, which is
        // the failure mode being prevented, not tested.
        super::install_tls_provider(&client);
        let result = compio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_multiplexed_async_connection(),
        )
        .await;
        let outcome = result.expect("certificate rejection should be an error, not a hang");
        assert!(
            outcome.is_err(),
            "an untrusted certificate must not connect"
        );
        // The rejection must have happened in the handshake: the peer never got to read data.
        drop(client);
        assert!(
            peer.join().expect("peer thread panicked").is_err(),
            "the peer's handshake should have failed when the client rejected its certificate"
        );
    }
}
