use std::io::Error;
use tokio::{net::TcpListener, signal};

mod server;

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

pub type Result<T> = std::result::Result<T, Error>;
