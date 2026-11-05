//! Compiled by CI to prove `skyzen` without default features pulls in no driver, runtime or TLS
//! backend beyond the ones this crate asks `skyzen-services` for.

use skyzen::Route;
use skyzen_services::Db;

fn main() {
    let _ = std::mem::size_of::<Route>();
    let _ = std::mem::size_of::<Db>();
}
