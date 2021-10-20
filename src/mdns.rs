use crate::Shutdown;
use tokio::sync::mpsc;

pub(crate) struct Mdns {
    /// Our advertised port.
    pub(crate) port: u16,

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

impl Mdns {
    pub(crate) async fn run(&mut self) -> crate::Result<()> {
        let (responder, task) = libmdns::Responder::with_default_handle()?;
        let _service = responder.register(
            "_raop._tcp".into(),
            "3C22FBA5A3AD@Airguitar".into(),
            self.port,
            &[
                "sf=0x4",
                "fv=76400.10",
                "am=Airguitar",
                "vs=105.1",
                "tp=TCP,UDP",
                "vn=65537",
                "ss=16",
                "sr=44100",
                "da=true",
                "sv=false",
                "et=0,1",
                "ek=1",
                "cn=0,1",
                "ch=2",
                "txtvers=1",
                "pw=true",
            ],
        );

        tokio::spawn(task);

        while !self.shutdown.is_shutdown() {
            tokio::select! {
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
