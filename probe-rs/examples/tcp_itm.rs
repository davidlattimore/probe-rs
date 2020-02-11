
use std::net::{TcpListener, TcpStream, SocketAddr};
use std::thread::{spawn, sleep, JoinHandle};
use std::sync::mpsc::{Sender, Receiver, channel};
use probe_rs::itm::{TracePacket, ItmPublisher, UpdaterChannel};
use probe_rs::Error;
use std::io::{BufRead, BufReader};
use std::io::prelude::*;
use serde::{Serialize, Deserialize};


fn main() -> Result<(), Error> {
    pretty_env_logger::init();

    use probe_rs::Probe;

    // Get a list of all available debug probes.
    let probes = Probe::list_all();

    // Use the first probe found.
    let probe = probes[0].open()?;

    // Attach to a chip.
    let session = probe.attach("stm32f407")?;

    loop {
        let bytes = session.read_swv();

        println!("{:?}", bytes);
    }

    Ok(())
}

pub struct TcpPublisher {
    connection_string: String,
    thread_handle: Option<(JoinHandle<()>, Sender<()>)>,
}

impl TcpPublisher {
    pub fn new(connection_string: impl Into<String>) -> Self {
        Self {
            connection_string: connection_string.into(),
            thread_handle: None,
        }
    }

    /// Writes a message to all connected websockets and removes websockets that are no longer connected.
    fn write_to_all_sockets(sockets: &mut Vec<(TcpStream, SocketAddr)>, message: impl AsRef<str>) {
        let mut to_remove = vec![];
        for (i, (socket, addr)) in sockets.iter_mut().enumerate() {
            match socket.write(message.as_ref().as_bytes()) {
                Ok(_) => (),
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock  {}
                    else { log::error!("Writing to a tcp experienced an error: {:?}", err) }
                }
                Err(err) => log::error!("Writing to a websocket experienced an error: {:?}", err),
            }
        }

        // Remove all closed websockets.
        for i in to_remove.into_iter().rev() {
            sockets.swap_remove(i);
        }
    }
}

impl ItmPublisher for TcpPublisher {
    fn start<I: Serialize + Send + Sync + 'static, O: Deserialize<'static> + Send + Sync + 'static>(&mut self) -> UpdaterChannel<I, O> {
        let mut sockets = Vec::new();

        let (rx, inbound) = channel::<I>();
        let (outbound, tx) = channel::<O>();
        let (halt_tx, halt_rx) = channel::<()>();

        log::info!("Opening websocket on '{}'", self.connection_string);
        let server = TcpListener::bind(&self.connection_string).unwrap();
        server.set_nonblocking(true).unwrap();

        self.thread_handle = Some((spawn(move || {
            let mut incoming = server.incoming();
            loop {
                // If a halt was requested, cease operations.
                if halt_rx.try_recv().is_ok() {
                    return ();
                }

                // Handle new incomming connections.
                match incoming.next() {
                    Some(Ok(stream)) => {
                        // Assume we always get a peer addr, so this unwrap is fine.
                        let addr = stream.peer_addr().unwrap();
                        
                        // Make sure we operate in nonblocking mode.
                        // Is is required so read does not block forever.
                        stream.set_nonblocking(true).unwrap();
                        log::info!("Accepted a new websocket connection from {}", addr);
                        sockets.push((stream, addr));
                    },
                    Some(Err(err)) => {
                        if err.kind() == std::io::ErrorKind::WouldBlock {}
                        else { log::error!("Connecting to a websocket experienced an error: {:?}", err) }
                    },
                    None => {
                        log::error!("The TCP listener iterator was exhausted. Shutting down websocket listener.");
                        return ();
                    },
                }

                // Send at max one pending message to each socket.
                match inbound.try_recv() {
                    Ok(update) => {
                        Self::write_to_all_sockets(&mut sockets, serde_json::to_string(&update).unwrap());
                    },
                    _ => ()
                }
                
                // Pause the current thread to not use CPU for no reason.
                sleep(std::time::Duration::from_micros(100));
            }
        }), halt_tx));

        UpdaterChannel::new(rx, tx)
    }

    fn stop(&mut self) -> Result<(), ()> {
        let thread_handle = self.thread_handle.take();
        match thread_handle.map(|h| {
            // If we have a running thread, send the request to stop it and then wait for a join.
            // If this unwrap fails the thread has already been destroyed.
            // This cannot be assumed under normal operation conditions. Even with normal fault handling this should never happen.
            // So this unwarp is fine.
            h.1.send(()).unwrap();
            h.0.join()
        }) {
            Some(Err(err)) => {
                log::error!("An error occured during thread execution: {:?}", err);
                Err(())
            }
            _ => Ok(()),
        }
    }
}
