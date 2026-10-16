//! Ethernet-only compatible-host discovery. Healthy TCP sessions send no queries.
use crate::config::{BridgeConfig, BridgeMode};
use crate::protocol::discovery::{Kind, Message, PORT, Party, SIZE};
use embassy_futures::select::{Either, select};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Address, Stack};
use embassy_time::{Instant, Timer};
use portable_atomic::{AtomicU64, Ordering};

static PEER: AtomicU64 = AtomicU64::new(0);
static EXPIRES: AtomicU64 = AtomicU64::new(0);
pub fn clear_peer() {
    EXPIRES.store(0, Ordering::Release);
    PEER.store(0, Ordering::Release);
}
pub fn peer() -> Option<(Ipv4Address, u16)> {
    if Instant::now().as_millis() >= EXPIRES.load(Ordering::Acquire) {
        return None;
    }
    let peer = PEER.load(Ordering::Acquire);
    let ip = ((peer >> 16) as u32).to_be_bytes();
    (peer != 0).then_some((Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]), peer as u16))
}

#[embassy_executor::task]
pub async fn discovery_task(stack: Stack<'static>, config: BridgeConfig) {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx_buffer = [0; SIZE * 4];
    let mut tx_buffer = [0; SIZE];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    if socket.bind(PORT).is_err() {
        return;
    }
    let mut bytes = [0; SIZE + 1];
    let mut next_query = 0;
    let mut query = None;
    let mut settle_until = 0;
    let mut best: Option<([u8; 6], [u8; 4], u16)> = None;
    let mut response_window = 0;
    let mut responses = 0;
    loop {
        let now = Instant::now().as_millis();
        if now.saturating_sub(response_window) >= 1000 {
            response_window = now;
            responses = 0;
        }
        let connected = crate::LINK_ACTIVE.load(Ordering::Acquire);
        let client = matches!(config.bridge_mode,BridgeMode::TcpClient {host,..} if host==[0;4]);
        if !stack.is_config_up() {
            clear_peer();
            Timer::after_millis(100).await;
            continue;
        }
        if client && !connected && now >= next_query {
            let seed = u64::from_le_bytes([
                config.mac_address[0],
                config.mac_address[1],
                config.mac_address[2],
                config.mac_address[3],
                config.mac_address[4],
                config.mac_address[5],
                0x51,
                0x9e,
            ]);
            let q = Message {
                kind: Kind::Query,
                party: Party::Pico,
                port: 0,
                nonce: (seed ^ now).max(1),
                identity: config.mac_address,
            };
            let _ = socket
                .send_to(&q.encode(), (Ipv4Address::BROADCAST, PORT))
                .await;
            query = Some(q);
            best = None;
            settle_until = now + 400;
            next_query = now + 1000;
        }
        if client && !connected && settle_until != 0 && now >= settle_until {
            if let Some((_, ip, port)) = best.take() {
                PEER.store(
                    (u64::from(u32::from_be_bytes(ip)) << 16) | u64::from(port),
                    Ordering::Release,
                );
                EXPIRES.store(now + 2500, Ordering::Release);
            }
            settle_until = 0;
        }
        let response = match select(socket.recv_from(&mut bytes), Timer::after_millis(50)).await {
            Either::First(Ok(response)) => response,
            _ => continue,
        };
        let (n, remote) = response;
        let Some(message) = Message::decode(&bytes[..n]) else {
            continue;
        };
        let embassy_net::IpAddress::Ipv4(ip) = remote.endpoint.addr;
        let ip = ip.octets();
        if ip == [0; 4] || ip[0] >= 224 {
            continue;
        }
        match config.bridge_mode {
            BridgeMode::TcpServer { port }
                if message.kind == Kind::Query
                    && message.party == Party::GroundStation
                    && message.identity != config.mac_address
                    && responses < 4 =>
            {
                let offer = Message {
                    kind: Kind::Offer,
                    party: Party::Pico,
                    port,
                    nonce: message.nonce,
                    identity: config.mac_address,
                };
                if socket
                    .send_to(&offer.encode(), remote.endpoint)
                    .await
                    .is_ok()
                {
                    responses += 1;
                }
            }
            BridgeMode::TcpClient { host, .. }
                if host == [0; 4]
                    && !connected
                    && settle_until != 0
                    && Instant::now().as_millis() <= settle_until =>
            {
                if query.is_some_and(|q| message.compatible_offer(q))
                    && best
                        .as_ref()
                        .is_none_or(|(id, _, _)| message.identity < *id)
                {
                    best = Some((message.identity, ip, message.port));
                }
            }
            _ => {}
        }
    }
}
