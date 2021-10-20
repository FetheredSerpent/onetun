use crate::config::{PortForwardConfig, PortProtocol};
use crate::virtual_iface::tcp::TcpVirtualInterface;
use crate::virtual_iface::{VirtualInterfacePoll, VirtualPort};
use crate::wg::WireGuardTunnel;
use anyhow::Context;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

use std::ops::Range;

use rand::seq::SliceRandom;
use rand::thread_rng;

const MAX_PACKET: usize = 65536;
const MIN_PORT: u16 = 1000;
const MAX_PORT: u16 = 60999;
const PORT_RANGE: Range<u16> = MIN_PORT..MAX_PORT;

/// Starts the server that listens on TCP connections.
pub async fn tcp_proxy_server(
    port_forward: PortForwardConfig,
    port_pool: TcpPortPool,
    wg: Arc<WireGuardTunnel>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(port_forward.source)
        .await
        .with_context(|| "Failed to listen on TCP proxy server")?;

    loop {
        let wg = wg.clone();
        let port_pool = port_pool.clone();
        let (socket, peer_addr) = listener
            .accept()
            .await
            .with_context(|| "Failed to accept connection on TCP proxy server")?;

        // Assign a 'virtual port': this is a unique port number used to route IP packets
        // received from the WireGuard tunnel. It is the port number that the virtual client will
        // listen on.
        let virtual_port = match port_pool.next().await {
            Ok(port) => port,
            Err(e) => {
                error!(
                    "Failed to assign virtual port number for connection [{}]: {:?}",
                    peer_addr, e
                );
                continue;
            }
        };

        info!("[{}] Incoming connection from {}", virtual_port, peer_addr);

        tokio::spawn(async move {
            let port_pool = port_pool.clone();
            let result =
                handle_tcp_proxy_connection(socket, virtual_port, port_forward, wg.clone()).await;

            if let Err(e) = result {
                error!(
                    "[{}] Connection dropped un-gracefully: {:?}",
                    virtual_port, e
                );
            } else {
                info!("[{}] Connection closed by client", virtual_port);
            }

            // Release port when connection drops
            wg.release_virtual_interface(VirtualPort(virtual_port, PortProtocol::Tcp));
            port_pool.release(virtual_port).await;
        });
    }
}

/// Handles a new TCP connection with its assigned virtual port.
async fn handle_tcp_proxy_connection(
    socket: TcpStream,
    virtual_port: u16,
    port_forward: PortForwardConfig,
    wg: Arc<WireGuardTunnel>,
) -> anyhow::Result<()> {
    // Abort signal for stopping the Virtual Interface
    let abort = Arc::new(AtomicBool::new(false));

    // Signals that the Virtual Client is ready to send data
    let (virtual_client_ready_tx, virtual_client_ready_rx) = tokio::sync::oneshot::channel::<()>();

    // data_to_real_client_(tx/rx): This task reads the data from this mpsc channel to send back
    // to the real client.
    let (data_to_real_client_tx, mut data_to_real_client_rx) = tokio::sync::mpsc::channel(1_000);

    // data_to_real_server_(tx/rx): This task sends the data received from the real client to the
    // virtual interface (virtual server socket).
    let (data_to_virtual_server_tx, data_to_virtual_server_rx) = tokio::sync::mpsc::channel(1_000);

    // Spawn virtual interface
    {
        let abort = abort.clone();
        let virtual_interface = TcpVirtualInterface::new(
            virtual_port,
            port_forward,
            wg,
            abort.clone(),
            data_to_real_client_tx,
            data_to_virtual_server_rx,
            virtual_client_ready_tx,
        );

        tokio::spawn(async move {
            virtual_interface.poll_loop().await.unwrap_or_else(|e| {
                error!("Virtual interface poll loop failed unexpectedly: {}", e);
                abort.store(true, Ordering::Relaxed);
            })
        });
    }

    // Wait for virtual client to be ready.
    virtual_client_ready_rx
        .await
        .with_context(|| "Virtual client dropped before being ready.")?;
    trace!("[{}] Virtual client is ready to send data", virtual_port);

    loop {
        tokio::select! {
            readable_result = socket.readable() => {
                match readable_result {
                    Ok(_) => {
                        // Buffer for the individual TCP segment.
                        let mut buffer = Vec::with_capacity(MAX_PACKET);
                        match socket.try_read_buf(&mut buffer) {
                            Ok(size) if size > 0 => {
                                let data = &buffer[..size];
                                debug!(
                                    "[{}] Read {} bytes of TCP data from real client",
                                    virtual_port, size
                                );
                                if let Err(e) = data_to_virtual_server_tx.send(data.to_vec()).await {
                                    error!(
                                        "[{}] Failed to dispatch data to virtual interface: {:?}",
                                        virtual_port, e
                                    );
                                }
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                continue;
                            }
                            Err(e) => {
                                error!(
                                    "[{}] Failed to read from client TCP socket: {:?}",
                                    virtual_port, e
                                );
                                break;
                            }
                            _ => {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("[{}] Failed to check if readable: {:?}", virtual_port, e);
                        break;
                    }
                }
            }
            data_recv_result = data_to_real_client_rx.recv() => {
                match data_recv_result {
                    Some(data) => match socket.try_write(&data) {
                        Ok(size) => {
                            debug!(
                                "[{}] Wrote {} bytes of TCP data to real client",
                                virtual_port, size
                            );
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if abort.load(Ordering::Relaxed) {
                                break;
                            } else {
                                continue;
                            }
                        }
                        Err(e) => {
                            error!(
                                "[{}] Failed to write to client TCP socket: {:?}",
                                virtual_port, e
                            );
                        }
                    },
                    None => {
                        if abort.load(Ordering::Relaxed) {
                            break;
                        } else {
                            continue;
                        }
                    },
                }
            }
        }
    }

    trace!("[{}] TCP socket handler task terminated", virtual_port);
    abort.store(true, Ordering::Relaxed);
    Ok(())
}

/// A pool of virtual ports available for TCP connections.
#[derive(Clone)]
pub struct TcpPortPool {
    inner: Arc<tokio::sync::RwLock<TcpPortPoolInner>>,
}

impl Default for TcpPortPool {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpPortPool {
    /// Initializes a new pool of virtual ports.
    pub fn new() -> Self {
        let mut inner = TcpPortPoolInner::default();
        let mut ports: Vec<u16> = PORT_RANGE.collect();
        ports.shuffle(&mut thread_rng());
        ports
            .into_iter()
            .for_each(|p| inner.queue.push_back(p) as ());
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(inner)),
        }
    }

    /// Requests a free port from the pool. An error is returned if none is available (exhaused max capacity).
    pub async fn next(&self) -> anyhow::Result<u16> {
        let mut inner = self.inner.write().await;
        let port = inner
            .queue
            .pop_front()
            .with_context(|| "Virtual port pool is exhausted")?;
        inner.taken.insert(port);
        Ok(port)
    }

    /// Releases a port back into the pool.
    pub async fn release(&self, port: u16) {
        let mut inner = self.inner.write().await;
        inner.queue.push_back(port);
        inner.taken.remove(&port);
    }
}

/// Non thread-safe inner logic for TCP port pool.
#[derive(Debug, Clone, Default)]
struct TcpPortPoolInner {
    /// Remaining ports in the pool.
    queue: VecDeque<u16>,
    /// Ports taken out of the pool.
    taken: HashSet<u16>,
}
