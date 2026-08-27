//! Shared reading of the HTTP statuses Azure services answer with.
//!
//! Cosmos DB, Service Bus, Azure Storage queues and Blob Storage are four different APIs, but they
//! are all Azure REST services and they all use the same statuses to say the same four things:
//! "slow down", "your credentials are not enough", "someone else got there first", and "the
//! precondition you attached no longer holds". Reading them in one place is what lets every backend
//! in this crate report [`Throttled`](skyzen_services::kv::KvError::Throttled) and
//! [`Unauthorized`](skyzen_services::kv::KvError::Unauthorized) the same way, instead of collapsing
//! all four into a backend error whose only distinguishing feature is its message text.

/// What an Azure service's HTTP status means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AzureStatus {
    /// `429 Too Many Requests`: the caller is over a request-unit or throughput limit. Azure
    /// answers a throttled request with a `Retry-After` telling the caller how long to wait.
    Throttled,
    /// `401 Unauthorized` or `403 Forbidden`: the request's credentials were rejected or do not
    /// carry the permission the operation needs. Retrying with the same credentials cannot help.
    Unauthorized,
    /// `409 Conflict`: the resource already exists, which is what a create-if-absent race looks
    /// like on every Azure service.
    Conflict,
    /// `412 Precondition Failed`: the `If-Match` / `If-None-Match` the request carried no longer
    /// holds, which is what a lost optimistic-concurrency race looks like.
    PreconditionFailed,
    /// `404 Not Found`: the resource is not there — for a lease-settling call, that the lease has
    /// already lapsed or been settled.
    ///
    /// `410 Gone` is deliberately **not** here: Azure services use it for a resource that never
    /// existed rather than one that just went away (Service Bus answers a call against a queue that
    /// does not exist with it), and reading that as a lapsed lease would hide a misconfiguration
    /// behind a retry.
    Absent,
    /// Anything else, which stays a backend error carrying the service's own message.
    Other,
}

/// Classify one Azure HTTP status.
pub const fn classify(status: u16) -> AzureStatus {
    match status {
        401 | 403 => AzureStatus::Unauthorized,
        404 => AzureStatus::Absent,
        409 => AzureStatus::Conflict,
        412 => AzureStatus::PreconditionFailed,
        429 => AzureStatus::Throttled,
        _ => AzureStatus::Other,
    }
}

/// Read a `Retry-After` header value.
///
/// Azure services document this header as a whole number of seconds. HTTP also allows an
/// `HTTP-date`, which Azure does not send for throttling; a value in that form (or any other
/// unparsable one) reports `None` — "the backend did not say" — rather than a guessed delay.
#[cfg(any(feature = "servicebus", feature = "storage-queue", feature = "blob"))]
pub fn retry_after(value: &str) -> Option<core::time::Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(core::time::Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::{classify, AzureStatus};

    #[test]
    fn the_four_taxonomy_statuses_are_recognized() {
        assert_eq!(classify(429), AzureStatus::Throttled);
        assert_eq!(classify(401), AzureStatus::Unauthorized);
        assert_eq!(classify(403), AzureStatus::Unauthorized);
        assert_eq!(classify(409), AzureStatus::Conflict);
        assert_eq!(classify(412), AzureStatus::PreconditionFailed);
        assert_eq!(classify(404), AzureStatus::Absent);
    }

    #[test]
    fn an_ordinary_failure_stays_a_backend_error() {
        assert_eq!(classify(400), AzureStatus::Other);
        // A queue that does not exist is a misconfiguration, not a lease that lapsed.
        assert_eq!(classify(410), AzureStatus::Other);
        assert_eq!(classify(500), AzureStatus::Other);
        assert_eq!(classify(503), AzureStatus::Other);
    }

    #[cfg(any(feature = "servicebus", feature = "storage-queue", feature = "blob"))]
    #[test]
    fn retry_after_reads_whole_seconds_and_refuses_anything_else() {
        use core::time::Duration;

        assert_eq!(super::retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(super::retry_after(" 30 "), Some(Duration::from_secs(30)));
        assert_eq!(super::retry_after("Fri, 31 Dec 1999 23:59:59 GMT"), None);
        assert_eq!(super::retry_after(""), None);
    }
}
