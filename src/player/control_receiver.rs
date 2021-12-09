use super::Command;
use crate::shutdown::Shutdown;
use std::sync::Arc;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{debug, instrument, trace};

#[derive(Debug)]
pub(crate) struct ControlReceiver {
    pub(crate) player_tx: mpsc::Sender<Command>,
    pub(crate) socket: Arc<UdpSocket>,

    pub(crate) shutdown: Shutdown,
}

impl ControlReceiver {
    #[instrument(skip(self))]
    pub(crate) async fn run(&mut self) -> crate::result::Result<()> {
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
                Ok(reader) if reader.payload_type() == 84 => {
                    trace!("{:?}", reader.sequence_number());
                }
                Ok(reader) if reader.payload_type() == 86 => {
                    // rtp reader expects `SSRC` field atm and interprets original seq as `SSRC`
                    // pull out seq + audio packet data directly from our buffer
                    let seq = (buf[6] as u16) << 8 | (buf[7] as u16);
                    let packet = buf[16..length].to_vec();

                    trace!("{:?}", seq);

                    self.player_tx
                        .send(Command::PutPacket {
                            seq: seq.into(),
                            packet: packet,
                        })
                        .await?
                }
                Ok(_) => {
                    trace!("unknown payload type");
                }
                Err(e) => {
                    debug!("{:?}", e);
                }
            };
        }

        Ok(())
    }
}
