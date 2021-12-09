use crate::{
    mdns::Mdns, player::Player, rtsp::listener::Listener, shutdown::Shutdown, Configuration,
};
use std::{future::Future, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc},
};
use tracing::{error, info};

pub(crate) async fn run(
    config: Arc<Configuration>,
    listener: TcpListener,
    shutdown: impl Future,
) -> crate::result::Result<()> {
    let local_addr = listener.local_addr()?;

    let (notify_shutdown, _) = broadcast::channel(1);
    let (shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel(1);
    let (player_tx, player_rx) = mpsc::channel(4);

    let mut mdns = Mdns {
        config: config.clone(),
        port: local_addr.port(),
        shutdown: Shutdown::new(notify_shutdown.subscribe()),
        _shutdown_complete: shutdown_complete_tx.clone(),
    };

    let mut player = Player {
        player_tx: player_tx.clone(),
        player_rx: player_rx,
        shutdown: Shutdown::new(notify_shutdown.subscribe()),
        _shutdown_complete: shutdown_complete_tx.clone(),
    };

    let mut server = Listener {
        config: config.clone(),
        listener,
        player_tx: player_tx.clone(),
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
      res = player.run() => {
        // If an error is received here, something happend while playing
        if let Err(err) = res {
          error!(cause = %err, "player failed");
        }
      }
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

    // Explicitly drop Mdns and Player allowing a clean exit.
    drop(player);
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
