//! The connection pool's manager: how one tiberius connection is opened and checked.
//!
//! tiberius reads and writes any `futures-io` stream, and this one is `async-net`'s TCP stream,
//! whose reactor is async-io's: a connection opens and runs under any executor, with no tokio
//! context. tiberius needs the stream to be `Send`, so it cannot be a thread-per-core driver's
//! socket.

use async_net::TcpStream;
use deadpool::managed::{self, Metrics, RecycleError, RecycleResult};
use tiberius::{error::Error, Config};

/// One connection to the database.
pub type Client = tiberius::Client<TcpStream>;

/// The pool of [`Client`]s.
pub type Pool = managed::Pool<Manager>;

/// Opens connections to the database one [`Config`] describes.
///
/// A description of how to connect, not a connection: nothing is dialled until the pool asks for
/// a connection.
#[derive(Debug)]
pub struct Manager {
    config: Config,
}

impl Manager {
    /// A manager connecting as `config` says.
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self { config }
    }

    /// Dial `config`'s address and log in over the stream.
    async fn connect(config: Config) -> Result<Client, Error> {
        let tcp = TcpStream::connect(config.get_addr()).await?;
        tcp.set_nodelay(true)?;
        Client::connect(config, tcp).await
    }
}

impl managed::Manager for Manager {
    type Type = Client;
    type Error = Error;

    async fn create(&self) -> Result<Client, Error> {
        match Self::connect(self.config.clone()).await {
            // Azure SQL's gateway may answer a login with the node that holds the database (the
            // `Redirect` connection policy), which the login must then be repeated against.
            Err(Error::Routing { host, port }) => {
                let mut config = self.config.clone();
                config.host(host);
                config.port(port);
                Self::connect(config).await
            }
            connected => connected,
        }
    }

    async fn recycle(&self, client: &mut Client, _: &Metrics) -> RecycleResult<Error> {
        client
            .simple_query("")
            .await
            .map(drop)
            .map_err(RecycleError::Backend)
    }
}
