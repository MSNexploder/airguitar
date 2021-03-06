mod control_receiver;
mod control_sender;
mod frame_buffer;
mod ntp;
mod server_receiver;
mod timing_receiver;
mod timing_sender;

use crate::{
    player::{
        control_receiver::ControlReceiver,
        control_sender::{ControlSender, ControlSenderCommand},
        frame_buffer::{FrameBuffer, FrameBufferSource},
        server_receiver::ServerReceiver,
        timing_receiver::TimingReceiver,
        timing_sender::TimingSender,
    },
    result::Result,
    rtp_info::RtpInfo,
    shutdown::Shutdown,
};
use aes::{
    cipher::block_padding::ZeroPadding,
    cipher::{
        block_padding::Padding, generic_array::typenum::U16, generic_array::GenericArray,
        BlockDecryptMut,
    },
    cipher::{InnerIvInit, KeyInit},
    Aes128,
};
use alac::{Decoder, StreamInfo};
use rodio::{OutputStream, Sink};
use rtp_rs::Seq;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::{
    net::UdpSocket,
    sync::{
        broadcast::{self, Sender},
        mpsc, oneshot,
    },
};
use tracing::error;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

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
pub(crate) struct GetParameterResponse {
    pub(crate) volume: f64,
}

#[derive(Debug)]
pub(crate) enum Command {
    // RTSP
    Announce {
        payload: Announce,
        resp: oneshot::Sender<Result<()>>,
    },
    Setup {
        payload: Setup,
        resp: oneshot::Sender<Result<SetupResponse>>,
    },
    Record {
        payload: RtpInfo,
        resp: oneshot::Sender<Result<()>>,
    },
    Teardown {
        resp: oneshot::Sender<Result<()>>,
    },
    SetParameter {
        volume: f64,
        resp: oneshot::Sender<Result<()>>,
    },
    GetParameter {
        resp: oneshot::Sender<GetParameterResponse>,
    },
    Flush {
        payload: RtpInfo,
        resp: oneshot::Sender<Result<()>>,
    },

    // Internal
    PutPacket {
        seq: Seq,
        packet: Vec<u8>,
    },
}

pub(crate) struct Player {
    pub(crate) player_tx: mpsc::Sender<Command>,
    pub(crate) player_rx: mpsc::Receiver<Command>,

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
    pub(crate) async fn run(&mut self) -> crate::result::Result<()> {
        let mut airplay_volume = 0.0;
        let mut _notify_shutdown: Option<Sender<()>> = None;
        let mut encryption: Option<Encryption> = None;
        let mut cipher: Option<Aes128> = None;
        let mut alac: Option<Decoder> = None;
        let mut frame_buffer: Option<Arc<Mutex<FrameBuffer<i16>>>> = None;
        let mut control_tx: Option<mpsc::Sender<ControlSenderCommand>> = None;

        let (_stream, stream_handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&stream_handle).unwrap();

        while !self.shutdown.is_shutdown() {
            let maybe_request = tokio::select! {
                res = self.player_rx.recv() => {
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
                Command::Announce { payload, resp } => {
                    encryption = payload.encryption;
                    if let Some(ref encryption) = encryption {
                        let key = GenericArray::from_slice(&encryption.aeskey);
                        cipher = Some(Aes128::new(&key));
                    }

                    alac = StreamInfo::from_sdp_format_parameters(&payload.fmtp)
                        .and_then(|config| Ok(Decoder::new(config)))
                        .ok();

                    let _ = resp.send(Ok(()));
                }
                Command::Setup { payload, resp } => {
                    let c_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
                    let t_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
                    let s_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

                    let c_addr = SocketAddr::new(payload.ip, payload.control_port);
                    let t_addr = SocketAddr::new(payload.ip, payload.timing_port);

                    c_sock.connect(c_addr).await?;
                    t_sock.connect(t_addr).await?;

                    let c_port = c_sock.local_addr()?.port();
                    let t_port = t_sock.local_addr()?.port();
                    let s_port = s_sock.local_addr()?.port();

                    let (notify_shutdown_sender, _) = broadcast::channel(1);
                    _notify_shutdown = Some(notify_shutdown_sender.clone());
                    let mut timing_sender = TimingSender {
                        socket: t_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    let mut timing_receiver = TimingReceiver {
                        socket: t_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    let (control_server_tx, control_server_rx) = mpsc::channel(4);
                    let mut control_sender = ControlSender {
                        control_server_rx: control_server_rx,
                        socket: c_sock.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    control_tx = Some(control_server_tx);

                    let mut control_receiver = ControlReceiver {
                        socket: c_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    let mut server_receiver = ServerReceiver {
                        socket: s_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    tokio::spawn(async move {
                        // Process the connection. If an error is encountered, log it.
                        if let Err(err) = timing_sender.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    tokio::spawn(async move {
                        // Process the connection. If an error is encountered, log it.
                        if let Err(err) = timing_receiver.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    tokio::spawn(async move {
                        // Process the connection. If an error is encountered, log it.
                        if let Err(err) = control_sender.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    tokio::spawn(async move {
                        // Process the connection. If an error is encountered, log it.
                        if let Err(err) = control_receiver.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    tokio::spawn(async move {
                        // Process the connection. If an error is encountered, log it.
                        if let Err(err) = server_receiver.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    let _ = resp.send(Ok(SetupResponse {
                        control_port: c_port,
                        timing_port: t_port,
                        server_port: s_port,
                    }));
                }
                Command::Record { payload, resp } => {
                    let inner_frame_buffer =
                        Arc::new(Mutex::new(FrameBuffer::<i16>::new(payload.seq.into())));
                    let source = FrameBufferSource::new(inner_frame_buffer.clone(), 2, 44100);
                    sink.append(source);

                    frame_buffer = Some(inner_frame_buffer);

                    let _ = resp.send(Ok(()));
                }
                Command::Teardown { resp } => {
                    _notify_shutdown = None;
                    encryption = None;
                    cipher = None;
                    alac = None;

                    let _ = resp.send(Ok(()));
                }
                Command::SetParameter { volume: vol, resp } => {
                    airplay_volume = vol;
                    let _ = resp.send(Ok(()));
                }
                Command::GetParameter { resp } => {
                    let _ = resp.send(GetParameterResponse {
                        volume: airplay_volume,
                    });
                }
                Command::Flush { payload, resp } => {
                    if let Some(ref frame_buffer) = frame_buffer {
                        let mut locked_frame_buffer = frame_buffer.lock().unwrap();
                        locked_frame_buffer.flush(payload.seq.into());
                    }

                    let _ = resp.send(Ok(()));
                }
                Command::PutPacket { seq, packet } => match (encryption.take(), cipher.take()) {
                    (Some(enc), Some(ci)) => {
                        let iv = GenericArray::from_slice(&enc.aesiv);
                        let mut buffer = packet.clone();
                        buffer.extend_from_slice(&[0; 16]);
                        let len = packet.len();
                        let aeslen = len & !0xf;

                        let be = (16 * (len / 16)) + 16;
                        let decrypter = Aes128CbcDec::inner_iv_init(ci.clone(), &iv);
                        let mut result = decrypter
                            .decrypt_padded_vec_mut::<ZeroPadding>(&buffer[..be])
                            .unwrap();

                        result[aeslen..len].copy_from_slice(&packet[aeslen..len]);

                        match alac {
                            Some(ref mut decoder) => {
                                let max_samples = decoder.stream_info().max_samples_per_packet();
                                let mut out = vec![0; max_samples as usize];
                                let result = decoder.decode_packet(&result, &mut out).unwrap();

                                // trace!("decoded: {:?} - {:?}", seq, result);

                                let data = result
                                    .iter()
                                    .map(|i| (i >> 16) as i16)
                                    .collect::<Vec<i16>>();
                                if let Some(ref frame_buffer) = frame_buffer {
                                    let mut locked_frame_buffer = frame_buffer.lock().unwrap();
                                    let missing_seqs =
                                        locked_frame_buffer.add_packet(seq, data.into_iter());

                                    if !missing_seqs.is_empty() {
                                        if let Some(ref control_tx) = control_tx {
                                            let _ = control_tx
                                                .send(ControlSenderCommand::MissingSeqs {
                                                    seqs: missing_seqs,
                                                })
                                                .await?;
                                        }
                                    }
                                }
                            }
                            None => todo!(),
                        }

                        encryption = Some(enc);
                        cipher = Some(ci);
                    }
                    _ => todo!(),
                },
            }
        }

        Ok(())
    }
}
