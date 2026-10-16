//! Observe address conflicts and send bounded ARP probes through the actual driver.
use crate::protocol::ipv4ll::{arp_conflict, arp_probe};
use core::task::Context;
use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_sync::waitqueue::AtomicWaker;
use portable_atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

pub struct ProbeState {
    mac: [u8; 6],
    candidate: AtomicU32,
    command: AtomicU8,
    conflict: AtomicBool,
    wake: AtomicWaker,
}
impl ProbeState {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            candidate: AtomicU32::new(0),
            command: AtomicU8::new(0),
            conflict: AtomicBool::new(false),
            wake: AtomicWaker::new(),
        }
    }
    pub fn candidate(&self, ip: [u8; 4]) {
        self.conflict.store(false, Ordering::Release);
        self.candidate
            .store(u32::from_be_bytes(ip), Ordering::Release);
    }
    pub fn clear(&self) {
        self.command.store(0, Ordering::Release);
        self.candidate.store(0, Ordering::Release);
        self.conflict.store(false, Ordering::Release);
    }
    pub fn request(&self, announce: bool) {
        self.command
            .store(if announce { 2 } else { 1 }, Ordering::Release);
        self.wake.wake();
    }
    pub fn pending(&self) -> bool {
        self.command.load(Ordering::Acquire) != 0
    }
    pub fn conflicted(&self) -> bool {
        self.conflict.load(Ordering::Acquire)
    }
    fn inspect(&self, bytes: &[u8]) {
        let ip = self.candidate.load(Ordering::Acquire);
        if ip != 0 && arp_conflict(bytes, self.mac, ip.to_be_bytes()) {
            self.conflict.store(true, Ordering::Release);
        }
    }
}
pub struct ArpGuard<D> {
    inner: D,
    state: &'static ProbeState,
}
impl<D> ArpGuard<D> {
    pub fn new(inner: D, state: &'static ProbeState) -> Self {
        Self { inner, state }
    }
}
impl<D: Driver> ArpGuard<D> {
    fn service(&mut self, cx: &mut Context<'_>) {
        self.state.wake.register(cx.waker());
        let command = self.state.command.load(Ordering::Acquire);
        let ip = self.state.candidate.load(Ordering::Acquire);
        if command == 0 || ip == 0 || self.inner.link_state(cx) != LinkState::Up {
            return;
        }
        if let Some(tx) = self.inner.transmit(cx) {
            let frame = arp_probe(self.state.mac, ip.to_be_bytes(), command == 2);
            tx.consume(frame.len(), |bytes| bytes.copy_from_slice(&frame));
            self.state.command.store(0, Ordering::Release);
        }
    }
}
pub struct ObservedRx<'a, R> {
    inner: R,
    state: &'a ProbeState,
}
impl<R: RxToken> RxToken for ObservedRx<'_, R> {
    fn consume<T, F: FnOnce(&mut [u8]) -> T>(self, f: F) -> T {
        self.inner.consume(|bytes| {
            self.state.inspect(bytes);
            f(bytes)
        })
    }
}
impl<D: Driver> Driver for ArpGuard<D> {
    type RxToken<'a>
        = ObservedRx<'a, D::RxToken<'a>>
    where
        Self: 'a;
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;
    fn receive(&mut self, cx: &mut Context<'_>) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.service(cx);
        let (rx, tx) = self.inner.receive(cx)?;
        Some((
            ObservedRx {
                inner: rx,
                state: self.state,
            },
            tx,
        ))
    }
    fn transmit(&mut self, cx: &mut Context<'_>) -> Option<Self::TxToken<'_>> {
        self.service(cx);
        self.inner.transmit(cx)
    }
    fn link_state(&mut self, cx: &mut Context<'_>) -> LinkState {
        self.inner.link_state(cx)
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn hardware_address(&self) -> HardwareAddress {
        self.inner.hardware_address()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        task::{Wake, Waker},
        vec::Vec,
    };
    struct Fake {
        ready: bool,
        link: LinkState,
        rx: Option<Vec<u8>>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }
    struct Rx(Vec<u8>);
    struct Tx(Arc<Mutex<Vec<Vec<u8>>>>);
    impl RxToken for Rx {
        fn consume<T, F: FnOnce(&mut [u8]) -> T>(mut self, f: F) -> T {
            f(&mut self.0)
        }
    }
    impl TxToken for Tx {
        fn consume<T, F: FnOnce(&mut [u8]) -> T>(self, len: usize, f: F) -> T {
            let mut bytes = vec![0; len];
            let result = f(&mut bytes);
            self.0.lock().unwrap().push(bytes);
            result
        }
    }
    impl Driver for Fake {
        type RxToken<'a> = Rx;
        type TxToken<'a> = Tx;
        fn receive(&mut self, _: &mut Context<'_>) -> Option<(Rx, Tx)> {
            Some((Rx(self.rx.take()?), Tx(self.sent.clone())))
        }
        fn transmit(&mut self, _: &mut Context<'_>) -> Option<Tx> {
            self.ready.then(|| Tx(self.sent.clone()))
        }
        fn link_state(&mut self, _: &mut Context<'_>) -> LinkState {
            self.link
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn hardware_address(&self) -> HardwareAddress {
            HardwareAddress::Ethernet([2, 1, 2, 3, 4, 5])
        }
    }
    struct WakeCount(AtomicU32);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[test]
    fn pending_probe_waits_for_actual_tx_capacity_and_wakes_network_runner() {
        let state = Box::leak(Box::new(ProbeState::new([2, 1, 2, 3, 4, 5])));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut guard = ArpGuard::new(
            Fake {
                ready: false,
                link: LinkState::Up,
                rx: None,
                sent: sent.clone(),
            },
            state,
        );
        let counter = Arc::new(WakeCount(AtomicU32::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(guard.receive(&mut cx).is_none());
        state.candidate([169, 254, 22, 33]);
        state.request(false);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(guard.receive(&mut cx).is_none());
        assert!(state.pending());
        assert!(sent.lock().unwrap().is_empty());
        guard.inner.ready = true;
        let _ = guard.transmit(&mut cx);
        assert!(!state.pending());
        assert_eq!(
            *sent.lock().unwrap(),
            vec![arp_probe([2, 1, 2, 3, 4, 5], [169, 254, 22, 33], false).to_vec()]
        );
        state.request(true);
        guard.inner.link = LinkState::Down;
        let _ = guard.receive(&mut cx);
        assert!(state.pending());
        assert_eq!(sent.lock().unwrap().len(), 1);
        state.clear();
        guard.inner.link = LinkState::Up;
        let _ = guard.receive(&mut cx);
        assert!(!state.pending());
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
    #[test]
    fn driver_observes_duplicate_ip_without_changing_received_bytes() {
        let mac = [2, 1, 2, 3, 4, 5];
        let ip = [169, 254, 22, 33];
        let state = Box::leak(Box::new(ProbeState::new(mac)));
        state.candidate(ip);
        let frame = arp_probe([2, 6, 7, 8, 9, 10], ip, true).to_vec();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut guard = ArpGuard::new(
            Fake {
                ready: true,
                link: LinkState::Up,
                rx: Some(frame.clone()),
                sent,
            },
            state,
        );
        let waker = Waker::from(Arc::new(WakeCount(AtomicU32::new(0))));
        let mut cx = Context::from_waker(&waker);
        let (rx, _) = guard.receive(&mut cx).unwrap();
        let bytes = rx.consume(|bytes| bytes.to_vec());
        assert_eq!(bytes, frame);
        assert!(state.conflicted());
        state.clear();
        assert!(!state.conflicted());
    }
}
