//! Reading the error numbers Azure SQL answers with.
//!
//! This is the job `crate::status` does for this crate's REST services, done for TDS: one place
//! that turns a backend's own fault code into the portable taxonomy, so a handler matching on "the
//! credentials were refused" or "back off" sees one shape across every backend rather than matching
//! on message text. It is a sibling rather than an arm of that module because TDS has no HTTP
//! status to read — the code here is SQL Server's error *number*.
//!
//! Only unambiguous numbers are classified. Everything else stays a [`DbError::Backend`] carrying
//! tiberius's error as its source — including the transient ones that are *not* rate limits, such
//! as `40613` (the database is coming back online after a serverless pause). Reporting those as
//! [`DbError::Throttled`] would render them as `429 Too Many Requests` and tell a caller to slow
//! down for a reason that is not true.

use core::time::Duration;

use deadpool_tiberius::tiberius::error::Error as TiberiusError;
use skyzen_services::DbError;

/// What a SQL Server error number means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlServerFault {
    /// The login was refused, or the logged-in principal lacks the privilege the statement needs.
    ///
    /// A *deployment* fault — the connection string, the SQL login, the database user or the
    /// server firewall — never something the HTTP caller did, which is why [`DbError::Unauthorized`]
    /// renders as a 500 rather than telling the caller they are unauthenticated.
    Unauthorized,
    /// The service asked the caller to slow down: a request-rate, worker or session limit.
    Throttled,
    /// The statement was chosen as a deadlock victim, so its transaction was rolled back.
    ///
    /// The remedy is to retry, which is exactly what [`DbError::Conflict`] means everywhere else in
    /// Skyzen.
    Conflict,
    /// Anything else, which stays a backend error carrying the server's own message.
    Other,
}

/// Classify one SQL Server error number.
///
/// The numbers, and why each is here:
///
/// - **`18456`** login failed for user, **`18470`** login disabled, **`4060`** cannot open the
///   requested database, **`40615`** the client IP is not allowed by the server firewall,
///   **`916`** the principal cannot access the database, and **`229`**/**`230`**/**`262`**/**`297`**
///   /**`300`** the various `permission denied` numbers — all mean "these credentials may not do
///   this", and all are fixed in a connection string or a `GRANT`, never in the statement.
/// - **`10928`**/**`10929`** a resource-governance limit was reached, **`40501`** the service is
///   busy, **`49918`**/**`49919`**/**`49920`** not enough resources to process the request — all
///   are Azure SQL telling the caller to back off and retry.
/// - **`1205`** deadlock victim.
///
/// `1105` and `9002` (the filegroup or the log is full) are deliberately absent: they are resource
/// exhaustion, but no amount of backing off fixes them, so they stay a backend error a human has to
/// read.
#[must_use]
pub const fn classify(code: u32) -> SqlServerFault {
    match code {
        229 | 230 | 262 | 297 | 300 | 916 | 4060 | 18456 | 18470 | 40615 => {
            SqlServerFault::Unauthorized
        }
        10928 | 10929 | 40501 | 49918 | 49919 | 49920 => SqlServerFault::Throttled,
        1205 => SqlServerFault::Conflict,
        _ => SqlServerFault::Other,
    }
}

/// The phrase Azure SQL uses to say how long to wait, and the only one read here.
///
/// Error `40501` documents its message as ending in `Retry the request after <N> seconds.` — TDS
/// has no `Retry-After` header, so the message is the only channel the delay can arrive on. A
/// message that does not carry the documented phrase reports `None`, "the server did not say",
/// rather than a guessed delay; this is deliberately a single fixed phrase and not a general
/// number-scraper, because every other number in a SQL Server message means something else.
pub fn retry_after(message: &str) -> Option<Duration> {
    /// The text immediately before the number of seconds.
    const PREFIX: &str = "Retry the request after ";
    /// The text immediately after it.
    const SUFFIX: &str = " seconds";

    let after_prefix = message.split_once(PREFIX)?.1;
    let seconds = after_prefix.split_once(SUFFIX)?.0;
    seconds.trim().parse().ok().map(Duration::from_secs)
}

/// Turn a tiberius error into the [`DbError`] that describes what actually happened.
///
/// `action` names what was being attempted, so the message reads as a sentence in a log line. A
/// refused login loses its message along with its source, deliberately and exactly as the AWS
/// backends do: "not authorized" is the whole diagnosis, and repeating the server's text would
/// invite a reader to look for the fault in the statement.
pub fn db_error(action: &str, error: TiberiusError) -> DbError {
    let TiberiusError::Server(server) = &error else {
        return DbError::backend_with(format!("failed to {action}: {error}"), error);
    };

    match classify(server.code()) {
        SqlServerFault::Unauthorized => {
            tracing::warn!(
                action,
                code = server.code(),
                "Azure SQL refused the request: the login, the database user or the server \
                 firewall is what has to change, not the statement",
            );
            DbError::Unauthorized
        }
        SqlServerFault::Throttled => {
            let retry_after = retry_after(server.message());
            tracing::warn!(
                action,
                code = server.code(),
                ?retry_after,
                "Azure SQL throttled the request",
            );
            DbError::Throttled { retry_after }
        }
        SqlServerFault::Conflict => {
            tracing::warn!(
                action,
                code = server.code(),
                "Azure SQL chose this statement as a deadlock victim and rolled its transaction \
                 back",
            );
            DbError::Conflict
        }
        SqlServerFault::Other => {
            DbError::backend_with(format!("failed to {action}: {error}"), error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, db_error, retry_after, SqlServerFault, TiberiusError};
    use core::time::Duration;
    use skyzen_services::DbError;

    #[test]
    fn a_refused_login_or_a_missing_grant_is_unauthorized() {
        for code in [18456, 18470, 4060, 40615, 916, 229, 230, 262, 297, 300] {
            assert_eq!(classify(code), SqlServerFault::Unauthorized, "{code}");
        }
    }

    #[test]
    fn a_resource_limit_is_throttling() {
        for code in [10928, 10929, 40501, 49918, 49919, 49920] {
            assert_eq!(classify(code), SqlServerFault::Throttled, "{code}");
        }
    }

    #[test]
    fn a_deadlock_victim_is_a_conflict() {
        assert_eq!(classify(1205), SqlServerFault::Conflict);
    }

    #[test]
    fn an_ordinary_failure_stays_a_backend_error() {
        // A syntax error, a missing object, a full log, and a database resuming from a serverless
        // pause: none of them is answered by backing off or by changing a `GRANT`.
        for code in [102, 208, 1105, 9002, 40613, 0] {
            assert_eq!(classify(code), SqlServerFault::Other, "{code}");
        }
    }

    #[test]
    fn the_documented_retry_delay_is_read_and_nothing_else_is() {
        assert_eq!(
            retry_after(
                "The service is currently busy. Retry the request after 10 seconds. Incident ID: x"
            ),
            Some(Duration::from_secs(10))
        );
        // No phrase, no guess — even though the message is full of numbers.
        assert_eq!(
            retry_after("Deadlock victim, process 52, resource 1205"),
            None
        );
        assert_eq!(retry_after("Retry the request after a while"), None);
        assert_eq!(retry_after(""), None);
    }

    #[test]
    fn a_transport_failure_keeps_its_source_and_names_the_action() {
        let error = db_error(
            "run a statement",
            TiberiusError::Tls("handshake failed".to_owned()),
        );
        assert!(
            matches!(
                &error,
                DbError::Backend {
                    source: Some(_),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("run a statement"), "{error}");
    }
}
