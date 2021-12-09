use super::connection::Connection;
use crate::{
    base64::{decode_base64, encode_base64},
    player::{Announce, Command, Encryption, Setup},
    rtp_info::RtpInfo,
    shutdown::Shutdown,
    Configuration,
};
use once_cell::sync::Lazy;
use rsa::{pkcs1::FromRsaPrivateKey, PaddingScheme, RsaPrivateKey};
use rtsp_types::{
    headers::{
        self, RtpLowerTransport, RtpProfile, RtpTransport, RtpTransportParameters, Transport,
        TransportMode, Transports,
    },
    HeaderName, Message, Method, Request, Response, ResponseBuilder, StatusCode, Version,
};
use sha1::Sha1;
use std::{collections::BTreeMap, net::IpAddr, str, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tracing::{instrument, trace};

#[derive(Debug)]
pub(crate) struct Handler {
    /// App configuration.
    pub(crate) config: Arc<Configuration>,

    /// The TCP connection decorated with the redis protocol encoder / decoder
    /// implemented using a buffered `TcpStream`.
    ///
    /// When `Listener` receives an inbound connection, the `TcpStream` is
    /// passed to `Connection::new`, which initializes the associated buffers.
    /// `Connection` allows the handler to operate at the "message" level and keep
    /// the byte level protocol parsing details encapsulated in `Connection`.
    pub(crate) connection: Connection,

    /// Used to control our Player
    pub(crate) player_tx: mpsc::Sender<Command>,

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

impl Handler {
    /// Process a single connection.
    ///
    /// Request messages are read from the socket and processed. Responses are
    /// written back to the socket.
    ///
    /// When the shutdown signal is received, the connection is processed until
    /// it reaches a safe state, at which point it is terminated.
    #[instrument(skip(self))]
    pub(crate) async fn run(&mut self) -> crate::result::Result<()> {
        // As long as the shutdown signal has not been received, try to read a
        // new request message.
        while !self.shutdown.is_shutdown() {
            let maybe_request = tokio::select! {
                res = self.connection.read_message() => res?,
                _ = self.shutdown.recv() => {
                    // If a shutdown signal is received, return from `run`.
                    // This will result in the task terminating.
                    return Ok(());
                }
            };

            // If `None` is returned from `read_message()` then the peer closed
            // the socket. There is no further work to do and the task can be
            // terminated.
            let request = match maybe_request {
                Some(Message::Request(request)) => request,
                Some(_) => unreachable!(),
                None => return Ok(()),
            };

            trace!("{:?}", request);

            self.execute(&request).await?
        }

        Ok(())
    }

    // TODO on error we should send send a response anyways (e.g. with status code ParameterNotUnderstood)
    async fn execute(&mut self, request: &Request<Vec<u8>>) -> crate::result::Result<()> {
        match request.method() {
            Method::Options => {
                let response_builder = Response::builder(Version::V1_0, StatusCode::Ok);
                let response = self.add_default_headers(request, response_builder)?
                .header(headers::PUBLIC, "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER")
                .empty();

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Setup => {
                let transports = request
                    .header(&headers::TRANSPORT)
                    .map(|x| {
                        // TODO fix parsing failure / wrong data for rtsp types parsing
                        // not sure who's wrong on this one
                        // let transports = request.typed_header::<Transports>(); <-- should be enough
                        x.as_str().replace("mode=record", "mode=\"RECORD\"")
                    })
                    .map(|x| {
                        Request::builder(Method::Setup, Version::V1_0)
                            .header(headers::TRANSPORT, x)
                            .empty()
                    })
                    .map(|x| x.typed_header::<Transports>().ok().flatten())
                    .flatten();
                let transport = transports.as_ref().map(|x| x.first()).flatten();

                let ports = match transport {
                    Some(Transport::Rtp(rtp)) => {
                        let params = &rtp.params.others;
                        let maybe_control_port = params
                            .get("control_port")
                            .map(|x| x.as_ref())
                            .flatten()
                            .map(|x| x.parse().ok())
                            .flatten();
                        let maybe_timing_port = params
                            .get("timing_port")
                            .map(|x| x.as_ref())
                            .flatten()
                            .map(|x| x.parse().ok())
                            .flatten();

                        if let (Some(control_port), Some(timing_port)) =
                            (maybe_control_port, maybe_timing_port)
                        {
                            Some((control_port, timing_port))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                if let Some((control_port, timing_port)) = ports {
                    let setup = Setup {
                        ip: self.connection.peer_addr.ip(),
                        control_port: control_port,
                        timing_port: timing_port,
                    };

                    let (tx, rx) = oneshot::channel();
                    self.player_tx
                        .send(Command::Setup {
                            payload: setup,
                            resp: tx,
                        })
                        .await?;
                    let success = rx.await?;

                    let response_builder = match success {
                        Ok(res) => {
                            let mut others = BTreeMap::new();
                            others.insert(
                                "control_port".into(),
                                Some(format!("{}", res.control_port)),
                            );
                            others
                                .insert("timing_port".into(), Some(format!("{}", res.timing_port)));

                            let transport = Transport::Rtp(RtpTransport {
                                profile: RtpProfile::Avp,
                                lower_transport: Some(RtpLowerTransport::Udp),
                                params: RtpTransportParameters {
                                    unicast: true,
                                    multicast: false,
                                    server_port: Some((res.server_port, None)),
                                    interleaved: Some((0, Some(1))),
                                    mode: vec![TransportMode::Record],
                                    others: others,
                                    ..Default::default()
                                },
                            });
                            let transports: Transports = vec![transport].into();

                            Response::builder(Version::V1_0, StatusCode::Ok)
                                .header(headers::SESSION, "1")
                                .typed_header(&transports)
                        }
                        Err(_) => {
                            Response::builder(Version::V1_0, StatusCode::ParameterNotUnderstood)
                        }
                    };
                    let response = self.add_default_headers(request, response_builder)?.empty();

                    self.connection.write_response(&response).await?;
                }

                Ok(())
            }
            Method::GetParameter => {
                let response_builder = self.add_default_headers(
                    request,
                    Response::builder(Version::V1_0, StatusCode::Ok),
                )?;

                let (tx, rx) = oneshot::channel();
                self.player_tx
                    .send(Command::GetParameter { resp: tx })
                    .await?;
                let parameters = rx.await?;

                let body = str::from_utf8(request.body())?
                    .lines()
                    .filter_map({
                        |line| match line {
                            "volume" => Some(format!("volume: {:.6}", parameters.volume)),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\r\n");

                if body.is_empty() {
                    let response = response_builder.empty();
                    self.connection.write_response(&response).await?;
                } else {
                    let response = response_builder.build(body);
                    self.connection.write_response(&response).await?;
                }

                Ok(())
            }
            Method::SetParameter => {
                let response = match request.header(&headers::CONTENT_TYPE).map(|x| x.as_str()) {
                    Some("text/parameters") => {
                        // TODO build proper text/parameters parser
                        for line in str::from_utf8(request.body())?.lines() {
                            match line.split_once(":") {
                                Some(("volume", volume)) => {
                                    let vol = volume.trim().parse::<f64>()?;
                                    let (tx, rx) = oneshot::channel();
                                    self.player_tx
                                        .send(Command::SetParameter {
                                            volume: vol,
                                            resp: tx,
                                        })
                                        .await?;
                                    let _ = rx.await?;
                                }
                                _ => {}
                            }
                        }

                        self.add_default_headers(
                            request,
                            Response::builder(Version::V1_0, StatusCode::Ok),
                        )?
                        .empty()
                    }
                    _ => {
                        Response::builder(Version::V1_0, StatusCode::ParameterNotUnderstood).empty()
                    }
                };

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Announce => {
                let sdp = sdp_types::Session::parse(&request.body())?;
                trace!("{:?}", sdp);

                let media = sdp
                    .medias
                    .first()
                    .ok_or_else(|| "missing media description")?;

                let fmtp = media
                    .get_first_attribute_value("fmtp")?
                    .map({
                        |x| match x.find(char::is_whitespace) {
                            Some(index) => x[index..].into(),
                            None => x.into(),
                        }
                    })
                    .ok_or_else(|| "missing fmtp")?;

                let minimum_latency = media
                    .get_first_attribute_value("min-latency")
                    .unwrap_or_else(|_| None)
                    .map(|x| x.parse().ok())
                    .flatten()
                    .unwrap_or_else(|| 0);

                let maximum_latency = media
                    .get_first_attribute_value("max-latency")
                    .unwrap_or_else(|_| None)
                    .map(|x| x.parse().ok())
                    .flatten()
                    .unwrap_or_else(|| 0);

                let aesiv = media
                    .get_first_attribute_value("aesiv")
                    .unwrap_or_else(|_| None)
                    .map(|x| decode_base64(x).ok())
                    .flatten();

                let aeskey = media
                    .get_first_attribute_value("rsaaeskey")
                    .unwrap_or_else(|_| None)
                    .map(|x| decode_base64(x).ok())
                    .flatten()
                    .map(|x| {
                        let padding = PaddingScheme::new_oaep::<Sha1>();
                        RSA_KEY.decrypt(padding, &x).ok()
                    })
                    .flatten();

                let encryption = if let (Some(aesiv), Some(aeskey)) = (aesiv, aeskey) {
                    Some(Encryption {
                        aesiv: aesiv,
                        aeskey: aeskey,
                    })
                } else {
                    None
                };

                let announce = Announce {
                    fmtp: fmtp,
                    minimum_latency: minimum_latency,
                    maximum_latency: maximum_latency,
                    encryption: encryption,
                };

                let (tx, rx) = oneshot::channel();
                self.player_tx
                    .send(Command::Announce {
                        payload: announce,
                        resp: tx,
                    })
                    .await?;
                let success = rx.await?;

                let response_builder = if success.is_ok() {
                    Response::builder(Version::V1_0, StatusCode::Ok)
                } else {
                    Response::builder(Version::V1_0, StatusCode::NotEnoughBandwidth)
                };
                let response = self.add_default_headers(request, response_builder)?.empty();

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Record => {
                let rtp_header = request.header(&headers::RTP_INFO);
                let response_builder = Response::builder(Version::V1_0, StatusCode::Ok)
                    .header(AUDIO_LATENCY.clone(), "11025");
                let response = self.add_default_headers(request, response_builder)?.empty();

                if let Some(value) = rtp_header {
                    match RtpInfo::parse(value.as_str()) {
                        Ok((_, info)) => {
                            let (tx, rx) = oneshot::channel();
                            self.player_tx
                                .send(Command::Record {
                                    resp: tx,
                                    payload: info,
                                })
                                .await?;
                            let _ = rx.await?;
                        }
                        Err(_) => {}
                    }
                }

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Teardown => {
                let response_builder = Response::builder(Version::V1_0, StatusCode::Ok)
                    .header(headers::CONNECTION, "close");
                let response = self.add_default_headers(request, response_builder)?.empty();

                let (tx, rx) = oneshot::channel();
                self.player_tx.send(Command::Teardown { resp: tx }).await?;
                let _ = rx.await?;

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Extension(extension) => match extension.as_str() {
                "FLUSH" | "flush" => {
                    let rtp_header = request.header(&headers::RTP_INFO);
                    let response_builder = Response::builder(Version::V1_0, StatusCode::Ok);
                    let response = self.add_default_headers(request, response_builder)?.empty();

                    if let Some(value) = rtp_header {
                        match RtpInfo::parse(value.as_str()) {
                            Ok((_, info)) => {
                                let (tx, rx) = oneshot::channel();
                                self.player_tx
                                    .send(Command::Flush {
                                        resp: tx,
                                        payload: info,
                                    })
                                    .await?;
                                let _ = rx.await?;
                            }
                            Err(_) => {}
                        }
                    }

                    self.connection.write_response(&response).await?;
                    Ok(())
                }
                _ => todo!(),
            },

            Method::Describe
            | Method::Pause
            | Method::Play
            | Method::PlayNotify
            | Method::Redirect => {
                let response =
                    Response::builder(Version::V1_0, StatusCode::MethodNotAllowed).empty();

                self.connection.write_response(&response).await?;
                Ok(())
            }
        }
    }

    fn add_default_headers(
        &self,
        request: &Request<Vec<u8>>,
        mut response_builder: ResponseBuilder,
    ) -> crate::result::Result<ResponseBuilder> {
        response_builder = response_builder.header(headers::SERVER, "AirTunes/105.1"); // TODO check if we can use Airguitar here

        if let Some(c_seq) = request.header(&headers::CSEQ) {
            response_builder = response_builder.header(headers::CSEQ, c_seq.as_str());
        }

        if let Some(challenge) = request.header(&APPLE_CHALLENGE) {
            let challenge = challenge.as_str();
            let response = self.calculate_challenge(challenge)?;
            response_builder = response_builder.header(APPLE_RESPONSE.clone(), response);
        }

        Ok(response_builder)
    }

    fn calculate_challenge(&self, challenge: &str) -> crate::result::Result<String> {
        let chall = decode_base64(challenge)?;
        let addr = match self.connection.local_addr.ip() {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let hw_addr = self.config.hw_addr.to_vec();

        let buf = [chall, addr, hw_addr].concat();
        let padding = PaddingScheme::new_pkcs1v15_sign(None);

        let challresp = RSA_KEY.sign(padding, &buf)?;
        let encoded = encode_base64(&challresp);

        Ok(encoded)
    }
}

const APPLE_CHALLENGE: Lazy<HeaderName> = Lazy::new(|| {
    HeaderName::from_static_str("Apple-Challenge").expect("HeaderName::from_static_str failed")
});
const APPLE_RESPONSE: Lazy<HeaderName> = Lazy::new(|| {
    HeaderName::from_static_str("Apple-Response").expect("HeaderName::from_static_str failed")
});
const AUDIO_LATENCY: Lazy<HeaderName> = Lazy::new(|| {
    HeaderName::from_static_str("Audio-Latency").expect("HeaderName::from_static_str failed")
});
const RSA_KEY: Lazy<RsaPrivateKey> = Lazy::new(|| {
    let super_secret_key = concat!(
        "-----BEGIN RSA PRIVATE KEY-----\n",
        "MIIEpQIBAAKCAQEA59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Q\n",
        "g90u3sG/1CUtwC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6\n",
        "DZHJ2YCCLlDRKSKv6kDqnw4UwPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLv\n",
        "qr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf/+sG+NCK3eQJVxqcaJ/vEHKIVd2M\n",
        "+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/UAaHqn9JdsBWLUEp\n",
        "VviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEewIDAQABAoIBAQDl8Axy9XfWBLmk\n",
        "zkEiqoSwF0PsmVrPzH9KsnwLGH+QZlvjWd8SWYGN7u1507HvhF5N3drJoVU3O14n\n",
        "DY4TFQAaLlJ9VM35AApXaLyY1ERrN7u9ALKd2LUwYhM7Km539O4yUFYikE2nIPsc\n",
        "EsA5ltpxOgUGCY7b7ez5NtD6nL1ZKauw7aNXmVAvmJTcuPxWmoktF3gDJKK2wxZu\n",
        "NGcJE0uFQEG4Z3BrWP7yoNuSK3dii2jmlpPHr0O/KnPQtzI3eguhe0TwUem/eYSd\n",
        "yzMyVx/YpwkzwtYL3sR5k0o9rKQLtvLzfAqdBxBurcizaaA/L0HIgAmOit1GJA2s\n",
        "aMxTVPNhAoGBAPfgv1oeZxgxmotiCcMXFEQEWflzhWYTsXrhUIuz5jFua39GLS99\n",
        "ZEErhLdrwj8rDDViRVJ5skOp9zFvlYAHs0xh92ji1E7V/ysnKBfsMrPkk5KSKPrn\n",
        "jndMoPdevWnVkgJ5jxFuNgxkOLMuG9i53B4yMvDTCRiIPMQ++N2iLDaRAoGBAO9v\n",
        "//mU8eVkQaoANf0ZoMjW8CN4xwWA2cSEIHkd9AfFkftuv8oyLDCG3ZAf0vrhrrtk\n",
        "rfa7ef+AUb69DNggq4mHQAYBp7L+k5DKzJrKuO0r+R0YbY9pZD1+/g9dVt91d6LQ\n",
        "NepUE/yY2PP5CNoFmjedpLHMOPFdVgqDzDFxU8hLAoGBANDrr7xAJbqBjHVwIzQ4\n",
        "To9pb4BNeqDndk5Qe7fT3+/H1njGaC0/rXE0Qb7q5ySgnsCb3DvAcJyRM9SJ7OKl\n",
        "Gt0FMSdJD5KG0XPIpAVNwgpXXH5MDJg09KHeh0kXo+QA6viFBi21y340NonnEfdf\n",
        "54PX4ZGS/Xac1UK+pLkBB+zRAoGAf0AY3H3qKS2lMEI4bzEFoHeK3G895pDaK3TF\n",
        "BVmD7fV0Zhov17fegFPMwOII8MisYm9ZfT2Z0s5Ro3s5rkt+nvLAdfC/PYPKzTLa\n",
        "lpGSwomSNYJcB9HNMlmhkGzc1JnLYT4iyUyx6pcZBmCd8bD0iwY/FzcgNDaUmbX9\n",
        "+XDvRA0CgYEAkE7pIPlE71qvfJQgoA9em0gILAuE4Pu13aKiJnfft7hIjbK+5kyb\n",
        "3TysZvoyDnb3HOKvInK7vXbKuU4ISgxB2bB3HcYzQMGsz1qJ2gG0N5hvJpzwwhbh\n",
        "XqFKA4zaaSrw622wDniAK5MlIE0tIAKKP4yxNGjoD2QYjhBGuhvkWKY=\n",
        "-----END RSA PRIVATE KEY-----",
    );

    RsaPrivateKey::from_pkcs1_pem(super_secret_key).expect("RsaPrivateKey::from_pkcs1_pem failed")
});
