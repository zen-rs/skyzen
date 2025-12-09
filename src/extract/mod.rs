pub use skyzen_core::Extractor;

#[cfg(feature = "form")]
mod query;
#[cfg(feature = "form")]
pub use query::Query;

pub mod client_ip;
pub use client_ip::{ClientIp, PeerAddr};
