use crate::{Configuration, Connection, Mdns, Shutdown};
use base64ct::{Base64Unpadded, Encoding};
use once_cell::sync::Lazy;
use rsa::{pkcs1::FromRsaPrivateKey, PaddingScheme, RsaPrivateKey};
use rtsp_types::{headers, HeaderName, Message, Method, Request, Response, StatusCode, Version};
use std::{future::Future, net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc},
    time,
};
use tracing::{debug, error, info, instrument};

#[derive(Debug)]
struct Listener {
    /// App configuration.
    config: Arc<Configuration>,

    /// TCP listener supplied by the `run` caller.
    listener: TcpListener,

    /// Broadcasts a shutdown signal to all active connections.
    ///
    /// The initial `shutdown` trigger is provided by the `run` caller. The
    /// server is responsible for gracefully shutting down active connections.
    /// When a connection task is spawned, it is passed a broadcast receiver
    /// handle. When a graceful shutdown is initiated, a `()` value is sent via
    /// the broadcast::Sender. Each active connection receives it, reaches a
    /// safe terminal state, and completes the task.
    notify_shutdown: broadcast::Sender<()>,

    /// Used as part of the graceful shutdown process to wait for client
    /// connections to complete processing.
    ///
    /// Tokio channels are closed once all `Sender` handles go out of scope.
    /// When a channel is closed, the receiver receives `None`. This is
    /// leveraged to detect all connection handlers completing. When a
    /// connection handler is initialized, it is assigned a clone of
    /// `shutdown_complete_tx`. When the listener shuts down, it drops the
    /// sender held by this `shutdown_complete_tx` field. Once all handler tasks
    /// complete, all clones of the `Sender` are also dropped. This results in
    /// `shutdown_complete_rx.recv()` completing with `None`. At this point, it
    /// is safe to exit the server process.
    shutdown_complete_rx: mpsc::Receiver<()>,
    shutdown_complete_tx: mpsc::Sender<()>,
}

#[derive(Debug)]
struct Handler {
    /// App configuration.
    config: Arc<Configuration>,

    /// The TCP connection decorated with the redis protocol encoder / decoder
    /// implemented using a buffered `TcpStream`.
    ///
    /// When `Listener` receives an inbound connection, the `TcpStream` is
    /// passed to `Connection::new`, which initializes the associated buffers.
    /// `Connection` allows the handler to operate at the "message" level and keep
    /// the byte level protocol parsing details encapsulated in `Connection`.
    connection: Connection,

    /// Listen for shutdown notifications.
    ///
    /// A wrapper around the `broadcast::Receiver` paired with the sender in
    /// `Listener`. The connection handler processes requests from the
    /// connection until the peer disconnects **or** a shutdown notification is
    /// received from `shutdown`. In the latter case, any in-flight work being
    /// processed for the peer is continued until it reaches a safe state, at
    /// which point the connection is terminated.
    shutdown: Shutdown,

    /// Not used directly. Instead, when `Handler` is dropped...
    _shutdown_complete: mpsc::Sender<()>,
}

pub(crate) async fn run(
    config: Arc<Configuration>,
    listener: TcpListener,
    shutdown: impl Future,
) -> crate::Result<()> {
    let local_addr = listener.local_addr()?;

    let (notify_shutdown, _) = broadcast::channel(1);
    let (shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel(1);

    let mut mdns = Mdns {
        config: config.clone(),
        port: local_addr.port(),
        shutdown: Shutdown::new(notify_shutdown.subscribe()),
        _shutdown_complete: shutdown_complete_tx.clone(),
    };

    let mut server = Listener {
        config: config.clone(),
        listener,
        notify_shutdown,
        shutdown_complete_tx,
        shutdown_complete_rx,
    };

    info!("Lets rock on {}!", local_addr);

    tokio::select! {
      res = server.run() => {
          // If an error is received here, accepting connections from the TCP
          // listener failed multiple times and the server is giving up and
          // shutting down.
          //
          // Errors encountered when handling individual connections do not
          // bubble up to this point.
          if let Err(err) = res {
            error!(cause = %err, "failed to accept");
          }
      },
      res = mdns.run() => {
        // If an error is received here, something happend while sending
        // mdns advertisements
        if let Err(err) = res {
          error!(cause = %err, "mdns failed");
        }
      },
      _ = shutdown => {
          // The shutdown signal has been received.
          info!("shutting down");
      },
    }

    // Extract the `shutdown_complete` receiver and transmitter
    // explicitly drop `shutdown_transmitter`. This is important, as the
    // `.await` below would otherwise never complete.
    let Listener {
        mut shutdown_complete_rx,
        shutdown_complete_tx,
        notify_shutdown,
        ..
    } = server;

    // Explicitly drop Mdns handler allowing a clean exit.
    drop(mdns);

    // When `notify_shutdown` is dropped, all tasks which have `subscribe`d will
    // receive the shutdown signal and can exit
    drop(notify_shutdown);
    // Drop final `Sender` so the `Receiver` below can complete
    drop(shutdown_complete_tx);

    // Wait for all active connections to finish processing. As the `Sender`
    // handle held by the listener has been dropped above, the only remaining
    // `Sender` instances are held by connection handler tasks. When those drop,
    // the `mpsc` channel will close and `recv()` will return `None`.
    let _ = shutdown_complete_rx.recv().await;

    Ok(())
}

impl Listener {
    /// Run the server
    ///
    /// Listen for inbound connections. For each inbound connection, spawn a
    /// task to process that connection.
    ///
    /// # Errors
    ///
    /// Returns `Err` if accepting returns an error. This can happen for a
    /// number reasons that resolve over time. For example, if the underlying
    /// operating system has reached an internal limit for max number of
    /// sockets, accept will fail.
    ///
    /// The process is not able to detect when a transient error resolves
    /// itself. One strategy for handling this is to implement a back off
    /// strategy, which is what we do here.
    async fn run(&mut self) -> crate::Result<()> {
        loop {
            // Accept a new socket. This will attempt to perform error handling.
            // The `accept` method internally attempts to recover errors, so an
            // error here is non-recoverable.
            let socket = self.accept().await?;

            // Create the necessary per-connection handler state.
            let mut handler = Handler {
                config: self.config.clone(),

                // Initialize the connection state.
                connection: Connection::new(socket)?,

                // Receive shutdown notifications.
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),

                // Notifies the receiver half once all clones are dropped.
                _shutdown_complete: self.shutdown_complete_tx.clone(),
            };

            // Spawn a new task to process the connections. Tokio tasks are like
            // asynchronous green threads and are executed concurrently.
            tokio::spawn(async move {
                // Process the connection. If an error is encountered, log it.
                if let Err(err) = handler.run().await {
                    error!(cause = ?err, "connection error");
                }
            });
        }
    }

    /// Accept an inbound connection.
    ///
    /// Errors are handled by backing off and retrying. An exponential backoff
    /// strategy is used. After the first failure, the task waits for 1 second.
    /// After the second failure, the task waits for 2 seconds. Each subsequent
    /// failure doubles the wait time. If accepting fails on the 4th try after
    /// waiting for 16 seconds, then this function returns with an error.
    async fn accept(&mut self) -> crate::Result<TcpStream> {
        let mut backoff = 1;

        // Try to accept a few times
        loop {
            // Perform the accept operation. If a socket is successfully
            // accepted, return it. Otherwise, save the error.
            match self.listener.accept().await {
                Ok((socket, _)) => return Ok(socket),
                Err(err) => {
                    if backoff > 16 {
                        // Accept has failed too many times. Return the error.
                        return Err(err.into());
                    }
                }
            }

            // Pause execution until the back off period elapses.
            time::sleep(Duration::from_secs(backoff)).await;

            // Double the back off
            backoff *= 2;
        }
    }
}

impl Handler {
    /// Process a single connection.
    ///
    /// Request messages are read from the socket and processed. Responses are
    /// written back to the socket.
    ///
    /// When the shutdown signal is received, the connection is processed until
    /// it reaches a safe state, at which point it is terminated.
    #[instrument]
    async fn run(&mut self) -> crate::Result<()> {
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

            debug!("{:?}", request);

            self.execute(&request).await?
        }

        Ok(())
    }

    async fn execute(&mut self, request: &Request<Vec<u8>>) -> crate::Result<()> {
        let mut response_builder = Response::builder(Version::V1_0, StatusCode::Ok)
            .header(headers::SERVER, "AirTunes/105.1"); // TODO check if we can use Airguitar here

        if let Some(c_seq) = request.header(&headers::CSEQ) {
            response_builder = response_builder.header(headers::CSEQ, c_seq.as_str());
        }

        if let Some(challenge) = request.header(&APPLE_CHALLENGE) {
            let challenge = challenge.as_str();
            let response = self.calculate_challenge(challenge)?;
            response_builder = response_builder.header(APPLE_RESPONSE.clone(), response);
        }

        match request.method() {
            Method::Describe => todo!(),
            Method::GetParameter => todo!(),
            Method::Options => {
                response_builder = response_builder.header(headers::PUBLIC, "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER");
                let response = response_builder.empty();
                debug!("{:?}", response);

                self.connection.write_response(&response).await?;

                Ok(())
            }
            Method::Pause => todo!(),
            Method::Play => todo!(),
            Method::PlayNotify => todo!(),
            Method::Redirect => todo!(),
            Method::Setup => todo!(),
            Method::SetParameter => todo!(),
            Method::Announce => todo!(),
            Method::Record => todo!(),
            Method::Teardown => todo!(),
            Method::Extension(_) => todo!(),
        }
    }

    fn calculate_challenge(&self, challenge: &str) -> crate::Result<String> {
        // Apple sometimes uses padded Base64 (e.g. Music App on iOS)
        // and sometimes removes the padding (e.g. Music App on macOS)
        // ¯\_(ツ)_/¯
        let actual_challenge = match challenge.find('=') {
            Some(index) => &challenge[..index],
            None => challenge,
        };

        let chall = Base64Unpadded::decode_vec(actual_challenge)?;
        let addr = match self.connection.local_addr.ip() {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let hw_addr = self.config.hw_addr.to_vec();

        let buf = [chall, addr, hw_addr].concat();
        let padding = PaddingScheme::new_pkcs1v15_sign(None);

        let challresp = RSA_KEY.sign(padding, &buf)?;
        let encoded = Base64Unpadded::encode_string(&challresp);

        Ok(encoded)
    }
}

const APPLE_CHALLENGE: Lazy<HeaderName> = Lazy::new(|| {
    HeaderName::from_static_str("Apple-Challenge").expect("HeaderName::from_static_str failed")
});
const APPLE_RESPONSE: Lazy<HeaderName> = Lazy::new(|| {
    HeaderName::from_static_str("Apple-Response").expect("HeaderName::from_static_str failed")
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
