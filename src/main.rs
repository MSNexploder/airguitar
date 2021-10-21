use tokio::{net::TcpListener, signal};

mod server;

mod connection;
use connection::Connection;

mod shutdown;
use shutdown::Shutdown;

mod mdns;
use mdns::Mdns;

#[tokio::main]
async fn main() -> crate::Result<()> {
    let port = 0; // don't care for now
    let listener = TcpListener::bind(&format!("0.0.0.0:{}", port)).await?;

    server::run(listener, signal::ctrl_c()).await
}

/// Error returned by most functions.
///
/// Maybe consider a specialized error handling crate or defining an error
/// type as an `enum` of causes.
/// However, for our example, using a boxed `std::error::Error` is sufficient.
///
/// For performance reasons, boxing is avoided in any hot path. For example, in
/// `parse`, a custom error `enum` is defined. This is because the error is hit
/// and handled during normal execution when a partial message is received on a
/// socket. `std::error::Error` is implemented for `parse::Error` which allows
/// it to be converted to `Box<dyn std::error::Error>`.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub(crate) type Result<T> = std::result::Result<T, Error>;
