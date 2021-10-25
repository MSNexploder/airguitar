use crate::{shutdown::Shutdown, Result};
use std::net::{IpAddr, SocketAddr};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
};
use tracing::debug;

#[derive(Debug)]
pub(crate) struct Encryption {
    pub(crate) aesiv: Vec<u8>,
    pub(crate) aeskey: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct Announce {
    pub(crate) fmtp: String,
    pub(crate) minimum_latency: u32,
    pub(crate) maximum_latency: u32,
    pub(crate) encryption: Option<Encryption>,
}

#[derive(Debug)]
pub(crate) struct Setup {
    pub(crate) ip: IpAddr,
    pub(crate) control_port: u16,
    pub(crate) timing_port: u16,
}

#[derive(Debug)]
pub(crate) struct SetupResponse {
    pub(crate) control_port: u16,
    pub(crate) timing_port: u16,
    pub(crate) server_port: u16,
}

#[derive(Debug)]
pub(crate) enum Command {
    Announce {
        payload: Announce,
        resp: oneshot::Sender<Result<()>>,
    },
    Setup {
        payload: Setup,
        resp: oneshot::Sender<Result<SetupResponse>>,
    },
}

pub(crate) struct Player {
    pub(crate) receiver: mpsc::Receiver<Command>,

    /// Listen for shutdown notifications.
    ///
    /// A wrapper around the `broadcast::Receiver` paired with the sender in
    /// `Listener`. The connection handler processes requests from the
    /// connection until the peer disconnects **or** a shutdown notification is
    /// received from `shutdown`. In the latter case, any in-flight work being
    /// processed for the peer is continued until it reaches a safe state, at
    /// which point the connection is terminated.
    pub(crate) shutdown: Shutdown,

    /// Not used directly. Instead, when `Handler` is dropped...
    pub(crate) _shutdown_complete: mpsc::Sender<()>,
}

impl Player {
    pub(crate) async fn run(&mut self) -> crate::Result<()> {
        while !self.shutdown.is_shutdown() {
            let maybe_request = tokio::select! {
                res = self.receiver.recv() => {
                  res
                },
                _ = self.shutdown.recv() => {
                    // If a shutdown signal is received, return from `run`.
                    // This will result in the task terminating.
                    return Ok(());
                }
            };

            let request = match maybe_request {
                Some(request) => request,
                None => return Ok(()),
            };

            debug!("{:?}", request);
            match request {
                Command::Announce { payload: _, resp } => {
                    let _ = resp.send(Ok(()));
                }
                Command::Setup { payload, resp } => {
                    let c_sock = UdpSocket::bind("0.0.0.0:0").await?;
                    let t_sock = UdpSocket::bind("0.0.0.0:0").await?;
                    let s_sock = UdpSocket::bind("0.0.0.0:0").await?;

                    let c_addr = SocketAddr::new(payload.ip, payload.control_port);
                    let t_addr = SocketAddr::new(payload.ip, payload.timing_port);

                    c_sock.connect(c_addr).await?;
                    t_sock.connect(t_addr).await?;

                    let c_port = c_sock.local_addr()?.port();
                    let t_port = t_sock.local_addr()?.port();
                    let s_port = s_sock.local_addr()?.port();

                    let _ = resp.send(Ok(SetupResponse {
                        control_port: c_port,
                        timing_port: t_port,
                        server_port: s_port,
                    }));
                }
            }

            // TODO do something with our command
        }

        Ok(())
    }
}
