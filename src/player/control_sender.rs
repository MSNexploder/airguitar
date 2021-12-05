use crate::shutdown::Shutdown;
use rtp_rs::{IntoSeqIterator, Seq};
use std::{ops::Range, sync::Arc};
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{instrument, trace};

#[derive(Debug)]
pub(crate) enum ControlSenderCommand {
    MissingSeqs { seqs: Range<Seq> },
}

#[derive(Debug)]
pub(crate) struct ControlSender {
    pub(crate) control_server_rx: mpsc::Receiver<ControlSenderCommand>,
    pub(crate) socket: Arc<UdpSocket>,

    pub(crate) shutdown: Shutdown,
}

impl ControlSender {
    #[instrument(skip(self))]
    pub(crate) async fn run(&mut self) -> crate::Result<()> {
        while !self.shutdown.is_shutdown() {
            let maybe_request = tokio::select! {
              res = self.control_server_rx.recv() => {
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

            // trace!("{:?}", request);
            match request {
                ControlSenderCommand::MissingSeqs { seqs } => {
                    trace!("missing seqs: {:?}", seqs);

                    let message = [
                        [0x80, (0x55 | 0x80)],
                        1_u16.to_be_bytes(),
                        u16::from(seqs.start).to_be_bytes(),
                        (seqs.seq_iter().count() as u16).to_be_bytes(),
                    ]
                    .concat();

                    let _ = self.socket.send(&message).await;
                }
            }
        }

        Ok(())
    }
}
