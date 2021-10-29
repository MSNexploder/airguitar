mod base64;
mod mdns;
mod player;
mod rtsp;
mod server;
mod shutdown;

use clap::{crate_version, Parser};
use md5::{Digest, Md5};
use std::sync::Arc;
use tokio::{net::TcpListener, signal};
use tracing_subscriber;

#[tokio::main]
async fn main() -> crate::Result<()> {
    tracing_subscriber::fmt::try_init()?;

    let cli_opts = CliOpts::parse();
    let name_digest = Md5::digest(cli_opts.name.as_bytes());

    let config = Configuration {
        port: cli_opts.port,
        name: cli_opts.name,
        hw_addr: [
            name_digest[0],
            name_digest[1],
            name_digest[2],
            name_digest[3],
            name_digest[4],
            name_digest[5],
        ],
    };

    let listener = TcpListener::bind(&format!("0.0.0.0:{}", config.port)).await?;
    server::run(Arc::new(config), listener, signal::ctrl_c()).await
}

#[derive(Debug, Parser)]
#[clap(version = crate_version!(), author = "Stefan Stüben <msnexploder@gmail.com>")]
pub(crate) struct CliOpts {
    /// Listening port (0 means random free port)
    #[clap(short, long, default_value = "0")]
    port: u16,
    /// Service name to identify this player
    #[clap(short, long, default_value = "Airguitar")]
    name: String,
}

#[derive(Debug)]
pub(crate) struct Configuration {
    port: u16,
    name: String,
    hw_addr: [u8; 6],
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
pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;

pub(crate) type Result<T> = std::result::Result<T, Error>;
