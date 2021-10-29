use super::Command;
use crate::shutdown::Shutdown;
use std::sync::Arc;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{debug, instrument, trace};

#[derive(Debug)]
pub(crate) struct TimingReceiver {
    pub(crate) player_tx: mpsc::Sender<Command>,
    pub(crate) socket: Arc<UdpSocket>,

    pub(crate) shutdown: Shutdown,
}

impl TimingReceiver {
    #[instrument(skip(self))]
    pub(crate) async fn run(&mut self) -> crate::Result<()> {
        let mut buf = [0; 32];
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

            // we expect message with exactly 32 bytes
            if length != 32 {
                continue;
            }

            match rtp_rs::RtpReader::new(&buf[..length]) {
                Ok(reader) => {
                    // we expect only messages with payload type 83
                    if reader.payload_type() != 83 {
                        continue;
                    }

                    trace!("{:?}", reader);
                    let seq = reader.sequence_number();
                    // rtp reader expects `SSRC` field atm and interprets half of the first timestamp as `SSRC`
                    // pull out timestamp data directly from our buffer
                    let origin = Timestamp {
                        sec: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
                        frac: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
                    };
                    let receive = Timestamp {
                        sec: u32::from_be_bytes(buf[16..20].try_into().unwrap()),
                        frac: u32::from_be_bytes(buf[20..24].try_into().unwrap()),
                    };
                    let transmit = Timestamp {
                        sec: u32::from_be_bytes(buf[24..28].try_into().unwrap()),
                        frac: u32::from_be_bytes(buf[28..32].try_into().unwrap()),
                    };

                    trace!("{:?} - {:?}-{:?}-{:?}", seq, origin, receive, transmit,);
                }
                Err(e) => {
                    debug!("{:?}", e);
                }
            };
        }

        Ok(())
    }
}

#[derive(Debug)]
struct Timestamp {
    sec: u32,
    frac: u32,
}
