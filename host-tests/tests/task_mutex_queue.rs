use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};
use pico_fi_host_tests::overwrite_queue::{OverwriteQueue, QueueItem};
use pico_fi_host_tests::task_mutex::TaskSpinRawMutex;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
thread_local! { static READ_PAUSE: std::cell::RefCell<Option<Arc<Pause>>> = const { std::cell::RefCell::new(None) }; }
struct Item {
    pause: Option<Arc<Pause>>,
    bytes: [u8; 4],
}
struct Pause {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl QueueItem for Item {
    fn queued_len(&self) -> usize {
        4
    }
    fn copy_queued_slice(&self, start: usize, target: &mut [u8]) {
        if start == 0 {
            if let Some(p) = &self.pause {
                p.entered.send(()).unwrap();
                p.release.lock().unwrap().recv().unwrap();
            }
        }
        target.copy_from_slice(&self.bytes[start..start + target.len()]);
    }
    fn from_queued_slices(first: &[u8], second: &[u8]) -> Option<Self> {
        READ_PAUSE.with(|slot| {
            if let Some(p) = slot.borrow_mut().take() {
                p.entered.send(()).unwrap();
                p.release.lock().unwrap().recv().unwrap();
            }
        });
        let mut bytes = [0; 4];
        bytes[..first.len()].copy_from_slice(first);
        bytes[first.len()..].copy_from_slice(second);
        Some(Self { pause: None, bytes })
    }
}
fn copy_irq_progress<M: RawMutex + Send + Sync + 'static>(expected: bool, reading: bool) {
    let q = Arc::new(OverwriteQueue::<Item, 8, 32, M>::new());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let pause = Arc::new(Pause {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    if reading {
        assert!(q.try_push_ref(&Item {
            pause: None,
            bytes: [1, 2, 3, 4]
        }));
    }
    let producer_q = q.clone();
    let producer = std::thread::spawn(move || {
        if reading {
            READ_PAUSE.with(|slot| *slot.borrow_mut() = Some(pause));
            assert_eq!(producer_q.try_pop().unwrap().bytes, [1, 2, 3, 4]);
            true
        } else {
            producer_q.try_push_ref(&Item {
                pause: Some(pause),
                bytes: [1, 2, 3, 4],
            })
        }
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (irq_tx, irq_rx) = mpsc::channel();
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let irq = std::thread::spawn(move || {
        attempt_tx.send(()).unwrap();
        critical_section::with(|_| irq_tx.send(()).unwrap());
    });
    attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let progress = irq_rx.recv_timeout(Duration::from_millis(200)).is_ok();
    release_tx.send(()).unwrap();
    assert!(producer.join().unwrap());
    irq.join().unwrap();
    assert_eq!(progress, expected);
    if !reading {
        assert_eq!(q.try_pop().unwrap().bytes, [1, 2, 3, 4]);
    }
}
#[test]
fn payload_copy_keeps_interrupt_critical_section_available() {
    for reading in [false, true] {
        copy_irq_progress::<CriticalSectionRawMutex>(false, reading);
        copy_irq_progress::<TaskSpinRawMutex>(true, reading);
    }
}
#[test]
fn task_mutex_releases_after_unwind_and_serializes_producers() {
    let lock = TaskSpinRawMutex::INIT;
    assert!(std::panic::catch_unwind(|| lock.lock(|| panic!("injected"))).is_err());
    assert_eq!(lock.lock(|| 7), 7);
    let q = Arc::new(OverwriteQueue::<Item, 8, 32, TaskSpinRawMutex>::new());
    let mut workers = Vec::new();
    for id in 0..4 {
        let q = q.clone();
        workers.push(std::thread::spawn(move || {
            for sequence in 0..5000u16 {
                let item = Item {
                    pause: None,
                    bytes: [id, (sequence >> 8) as u8, sequence as u8, 0xa5],
                };
                while !q.try_push_ref(&item) {
                    std::thread::yield_now();
                }
            }
        }));
    }
    let mut next = [0u16; 4];
    for _ in 0..20000 {
        let item = loop {
            if let Some(item) = q.try_pop() {
                break item;
            }
            std::thread::yield_now();
        };
        let id = item.bytes[0] as usize;
        assert!(id < 4);
        assert_eq!(item.bytes[3], 0xa5);
        assert_eq!(u16::from_be_bytes([item.bytes[1], item.bytes[2]]), next[id]);
        next[id] += 1;
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(next, [5000; 4]);
    assert!(q.try_pop().is_none());
}
