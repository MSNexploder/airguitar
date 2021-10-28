use super::Command;
use crate::shutdown::Shutdown;
use std::sync::Arc;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{debug, instrument, trace};

#[derive(Debug)]
pub(crate) struct ServerReceiver {
    pub(crate) player_tx: mpsc::Sender<Command>,
    pub(crate) socket: Arc<UdpSocket>,

    pub(crate) shutdown: Shutdown,
}

impl ServerReceiver {
    #[instrument(skip(self))]
    pub(crate) async fn run(&mut self) -> crate::Result<()> {
        let mut buf = [0; 4 * 1024];
        while !self.shutdown.is_shutdown() {
            let length = tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                  trace!("{:?}", result);
                  match result {
                      Ok((length, _)) => {
                        if length == 0 {
                          return Ok(()); // connection closed
                        } else {
                          length
                        }
                      },
                      Err(e) => {
                        return Err(e.into());
                      },
                  }
                },
                _ = self.shutdown.recv() => {
                    // If a shutdown signal is received, return from `run`.
                    // This will result in the task terminating.
                    return Ok(());
                }
            };

            match rtp_rs::RtpReader::new(&buf[..length]) {
                Ok(reader) => {
                    trace!("{:?}", reader);
                    let seq = reader.sequence_number();
                    let packet = reader.payload().to_vec();

                    self.player_tx
                        .send(Command::PutPacket {
                            seq: seq,
                            packet: packet,
                        })
                        .await?
                }
                Err(e) => {
                    debug!("{:?}", e);
                }
            };
        }

        Ok(())
    }
}
