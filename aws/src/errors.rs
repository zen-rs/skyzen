//! Shared classification of AWS service error codes.
//!
//! Every AWS SDK error carries the service's own error code through [`ProvideErrorMetadata`]. The
//! codes that mean "back off and retry" and "these credentials will never work" are AWS-wide
//! conventions rather than per-service ones, so the mapping lives here once and each service's
//! error helper turns the resulting [`AwsErrorCategory`] into its own error type. Without it a
//! handler cannot tell a `ProvisionedThroughputExceededException` (retry with backoff) from an
//! `AccessDeniedException` (fix IAM, never retry) without matching on message substrings.

use aws_smithy_types::error::metadata::ProvideErrorMetadata;

/// What an AWS service error code means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsErrorCategory {
    /// The caller is over a rate or capacity limit; retrying with backoff can succeed.
    Throttled,
    /// The request's credentials were rejected outright; retrying cannot help.
    Unauthorized,
    /// Anything else, which stays a backend error carrying the SDK's own message.
    Backend,
}

/// Codes AWS services use to say the caller is over a limit.
///
/// Deliberately narrow: only codes whose sole meaning is "you sent too much" are listed, because
/// mistaking a permanent failure for a retryable one turns a fast error into a retry storm.
const THROTTLING_CODES: [&str; 6] = [
    "ThrottlingException",
    "Throttling",
    "RequestThrottled",
    "RequestThrottledException",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
];

/// Codes AWS services use to reject the request's credentials.
const UNAUTHORIZED_CODES: [&str; 5] = [
    "AccessDenied",
    "AccessDeniedException",
    "UnrecognizedClientException",
    "InvalidClientTokenId",
    "MissingAuthenticationToken",
];

/// Classify an SDK error by the service error code it carries.
///
/// An error with no code — a connection failure, a timeout — is [`AwsErrorCategory::Backend`]: it
/// says nothing about limits or credentials.
pub fn categorize<E: ProvideErrorMetadata>(error: &E) -> AwsErrorCategory {
    match error.code() {
        Some(code) if THROTTLING_CODES.contains(&code) => AwsErrorCategory::Throttled,
        Some(code) if UNAUTHORIZED_CODES.contains(&code) => AwsErrorCategory::Unauthorized,
        _ => AwsErrorCategory::Backend,
    }
}

/// An error carrying nothing but a service error code, which is all classification reads.
///
/// Lives here rather than inside the test module so every service's error-mapping tests classify
/// the same stand-in instead of each rebuilding one. It implements [`std::error::Error`] because
/// the per-service mappers take the SDK error by value and keep it as a source.
#[cfg(test)]
#[derive(Debug)]
pub struct Coded(aws_smithy_types::error::metadata::ErrorMetadata);

#[cfg(test)]
impl Coded {
    /// An error reporting `code` as its service error code.
    pub fn new(code: &str) -> Self {
        Self(
            aws_smithy_types::error::metadata::ErrorMetadata::builder()
                .code(code)
                .build(),
        )
    }

    /// An error reporting no service error code at all, as a connection failure does.
    pub fn without_code() -> Self {
        Self(aws_smithy_types::error::metadata::ErrorMetadata::builder().build())
    }
}

#[cfg(test)]
impl core::fmt::Display for Coded {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}", self.0.code().unwrap_or("service error"))
    }
}

#[cfg(test)]
impl std::error::Error for Coded {}

#[cfg(test)]
impl ProvideErrorMetadata for Coded {
    fn meta(&self) -> &aws_smithy_types::error::metadata::ErrorMetadata {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{categorize, AwsErrorCategory, Coded, THROTTLING_CODES, UNAUTHORIZED_CODES};

    #[test]
    fn throttling_codes_are_retryable() {
        for code in THROTTLING_CODES {
            assert_eq!(
                categorize(&Coded::new(code)),
                AwsErrorCategory::Throttled,
                "{code} should be classified as throttling"
            );
        }
    }

    #[test]
    fn credential_codes_are_unauthorized() {
        for code in UNAUTHORIZED_CODES {
            assert_eq!(
                categorize(&Coded::new(code)),
                AwsErrorCategory::Unauthorized,
                "{code} should be classified as unauthorized"
            );
        }
    }

    #[test]
    fn an_unrecognized_or_absent_code_stays_a_backend_error() {
        assert_eq!(
            categorize(&Coded::new("ResourceNotFoundException")),
            AwsErrorCategory::Backend
        );
        assert_eq!(
            categorize(&Coded::new("ConditionalCheckFailedException")),
            AwsErrorCategory::Backend
        );
        assert_eq!(
            categorize(&Coded::without_code()),
            AwsErrorCategory::Backend
        );
    }

    #[test]
    fn the_two_code_sets_do_not_overlap() {
        for code in THROTTLING_CODES {
            assert!(
                !UNAUTHORIZED_CODES.contains(&code),
                "{code} is classified twice"
            );
        }
    }
}
