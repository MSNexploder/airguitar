use crate::{Connection, Mdns, Shutdown};
use rtsp_types::{Message, Method};
use std::{future::Future, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc},
    time,
};

#[derive(Debug)]
struct Listener {
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

pub async fn run(listener: TcpListener, shutdown: impl Future) -> crate::Result<()> {
    let local_addr = listener.local_addr()?;

    let (notify_shutdown, _) = broadcast::channel(1);
    let (shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel(1);

    let mut mdns = Mdns {
        port: local_addr.port(),
        shutdown: Shutdown::new(notify_shutdown.subscribe()),
        _shutdown_complete: shutdown_complete_tx.clone(),
    };

    let mut server = Listener {
        listener,
        notify_shutdown,
        shutdown_complete_tx,
        shutdown_complete_rx,
    };

    println!("Lets rock on {}!", local_addr);

    tokio::select! {
      res = server.run() => {
          // If an error is received here, accepting connections from the TCP
          // listener failed multiple times and the server is giving up and
          // shutting down.
          //
          // Errors encountered when handling individual connections do not
          // bubble up to this point.
          if let Err(_err) = res {
              // TODO error logging
          }
      },
      res = mdns.run() => {
        // If an error is received here, something happend while sending
        // mdns advertisements
          if let Err(_err) = res {
            // TODO error logging
        }
      },
      _ = shutdown => {
          // The shutdown signal has been received.
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
                // Initialize the connection state.
                connection: Connection::new(socket),

                // Receive shutdown notifications.
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),

                // Notifies the receiver half once all clones are
                // dropped.
                _shutdown_complete: self.shutdown_complete_tx.clone(),
            };

            // Spawn a new task to process the connections. Tokio tasks are like
            // asynchronous green threads and are executed concurrently.
            tokio::spawn(async move {
                // Process the connection. If an error is encountered, log it.
                if let Err(_err) = handler.run().await {
                    // TODO error logging
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
                Some(_) => unimplemented!(),
                None => return Ok(()),
            };

            match request.method() {
                Method::Describe => todo!(),
                Method::GetParameter => todo!(),
                Method::Options => todo!(),
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

        Ok(())
    }
}
