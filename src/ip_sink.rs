use crate::virtual_device::VirtualIpDevice;
use crate::wg::WireGuardTunnel;
use smoltcp::iface::InterfaceBuilder;
use std::sync::Arc;
use tokio::time::Duration;

/// A repeating task that processes unroutable IP packets.
pub async fn run_ip_sink_interface(wg: Arc<WireGuardTunnel>) -> ! {
    // Initialize interface
    let device = VirtualIpDevice::new_sink(wg)
        .await
        .expect("Failed to initialize VirtualIpDevice for sink interface");

    // No sockets on sink interface
    let mut sockets: [_; 0] = Default::default();
    let mut virtual_interface = InterfaceBuilder::new(device, &mut sockets[..]).ip_addrs([]).finalize();

    loop {
        let loop_start = smoltcp::time::Instant::now();
        match virtual_interface.poll(loop_start) {
            Ok(processed) if processed => {
                trace!("[SINK] Virtual interface polled some packets to be processed",);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(e) => {
                error!("[SINK] Virtual interface poll error: {:?}", e);
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}
