use super::Command;
use crate::shutdown::Shutdown;
use std::{sync::Arc, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc, time};
use tracing::instrument;

#[derive(Debug)]
pub(crate) struct TimingSender {
    pub(crate) player_tx: mpsc::Sender<Command>,
    pub(crate) socket: Arc<UdpSocket>,

    pub(crate) shutdown: Shutdown,
}

impl TimingSender {
    #[instrument(skip(self))]
    pub(crate) async fn run(&mut self) -> crate::result::Result<()> {
        while !self.shutdown.is_shutdown() {
            tokio::select! {
                _ = time::sleep(Duration::from_secs(3)) => {
                  let message = [0x80, 0xd2, 0x0, 0x07, 0x0, 0x0, 0x0, 0x0,
                                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                                    0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                                ];

                  let _ = self.socket.send(&message).await;
                },
                _ = self.shutdown.recv() => {
                    // If a shutdown signal is received, return from `run`.
                    // This will result in the task terminating.
                    return Ok(());
                }
            };
        }

        Ok(())
    }
}
