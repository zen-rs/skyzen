//! Precompressed variants: which sibling files exist for a coding, and which representation a
//! request gets.
//!
//! A variant is the file's name plus the coding's extension — `app.js.br`, `app.js.zst`,
//! `app.js.gz` — which is what `skyzen build` writes next to every file it compresses. The choice
//! among them follows the request's `Accept-Encoding` (RFC 9110 §12.5.3):
//!
//! - the highest quality wins, and a coding the client gave `q=0`, or did not list with no `*` to
//!   cover it, is not a candidate;
//! - `identity` competes like any coding when the client lists it (or `*`); when the header does
//!   not mention it, it is the answer only if no listed coding is available;
//! - between equal qualities the smaller representation wins, then the one listed first;
//! - with nothing acceptable — `identity;q=0` and no available coding the client takes — the
//!   file is sent unencoded, because refusing the request would serve no one.
//!
//! Every response for a file that has a variant carries `Vary: Accept-Encoding`, the unencoded
//! one included, so a shared cache never hands a `br` body to a client that did not ask for it.

use http_kit::header::{HeaderMap, HeaderValue};

use crate::utils::accept_encoding::{AcceptEncoding, Identity, Preference};

/// A content coding a precompressed sibling can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Coding {
    /// Brotli, `.br`.
    Brotli,
    /// Zstandard, `.zst`.
    Zstd,
    /// gzip, `.gz`.
    Gzip,
}

impl Coding {
    /// Every coding a sibling is looked for in.
    pub(super) const ALL: [Self; 3] = [Self::Brotli, Self::Zstd, Self::Gzip];

    /// The suffix `skyzen build` appends to the file name, without the dot.
    pub(super) const fn extension(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Zstd => "zst",
            Self::Gzip => "gz",
        }
    }

    /// The `Accept-Encoding` tokens that name it.
    const fn tokens(self) -> &'static [&'static str] {
        match self {
            Self::Brotli => &["br"],
            Self::Zstd => &["zstd"],
            Self::Gzip => &["gzip", "x-gzip"],
        }
    }

    /// Its `Content-Encoding` value.
    pub(super) const fn header_value(self) -> HeaderValue {
        match self {
            Self::Brotli => HeaderValue::from_static("br"),
            Self::Zstd => HeaderValue::from_static("zstd"),
            Self::Gzip => HeaderValue::from_static("gzip"),
        }
    }
}

/// Pick the representation to send: `Some(index)` into `variants`, or `None` for the file itself.
///
/// `variants` lists each available sibling's coding and length; `identity_len` is the file's own.
pub(super) fn choose(
    headers: &HeaderMap,
    identity_len: u64,
    variants: impl IntoIterator<Item = (Coding, u64)>,
) -> Option<usize> {
    let accept = AcceptEncoding::from_headers(headers);

    // `(preference, length, index)`; `None` as the index is the unencoded file.
    let mut best = match accept.identity() {
        Identity::Listed(preference) => Some((preference, identity_len, None)),
        Identity::Implied | Identity::Refused => None,
    };
    for (index, (coding, len)) in variants.into_iter().enumerate() {
        let Some(preference) = accept.preference(coding.tokens()) else {
            continue;
        };
        let candidate = (preference, len, Some(index));
        if best.is_none_or(|current| beats(&candidate, &current)) {
            best = Some(candidate);
        }
    }
    best.and_then(|(_, _, index)| index)
}

fn beats(
    (candidate, candidate_len, _): &(Preference, u64, Option<usize>),
    (current, current_len, _): &(Preference, u64, Option<usize>),
) -> bool {
    candidate
        .quality
        .total_cmp(&current.quality)
        .then_with(|| current_len.cmp(candidate_len))
        .then_with(|| current.position.cmp(&candidate.position))
        .is_gt()
}

#[cfg(test)]
mod tests {
    use super::{choose, Coding};
    use http_kit::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING};

    const ALL: [(Coding, u64); 3] = [(Coding::Brotli, 30), (Coding::Zstd, 35), (Coding::Gzip, 40)];

    fn accepting(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn no_header_gets_the_file_itself() {
        assert_eq!(choose(&HeaderMap::new(), 100, ALL), None);
    }

    #[test]
    fn equal_qualities_pick_the_smallest() {
        assert_eq!(
            choose(&accepting("gzip, deflate, br, zstd"), 100, ALL),
            Some(0)
        );
        assert_eq!(choose(&accepting("gzip, zstd"), 100, ALL), Some(1));
    }

    #[test]
    fn quality_outranks_size() {
        assert_eq!(choose(&accepting("br;q=0.5, gzip"), 100, ALL), Some(2));
    }

    #[test]
    fn a_listed_identity_competes_and_wins_on_quality() {
        assert_eq!(choose(&accepting("br;q=0.2, identity"), 100, ALL), None);
    }

    #[test]
    fn a_wildcard_admits_every_coding_not_named() {
        assert_eq!(choose(&accepting("br;q=0, *"), 100, ALL), Some(1));
    }

    #[test]
    fn an_unavailable_coding_falls_back_to_the_file() {
        assert_eq!(
            choose(&accepting("zstd"), 100, [(Coding::Gzip, 40)]),
            None,
            "identity is implied when the header does not mention it"
        );
        assert_eq!(
            choose(&accepting("zstd, identity;q=0"), 100, [(Coding::Gzip, 40)]),
            None,
            "nothing acceptable is available, so the file goes out unencoded"
        );
    }
}
