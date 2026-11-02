#![forbid(unsafe_code)]
use embedded_io_async::Read;
const CAPACITY: usize = 4100;
const SCAN_BUDGET: usize = 64;
const CRC_BUDGET: usize = 4096;
#[derive(Default, Debug)]
pub struct Metrics {
    pub rejected_data: usize,
    pub quarantined_command_headers: usize,
    pub yields: usize,
    pub peak_buffered: usize,
    pub peak_scans_per_quantum: usize,
    pub peak_crc_bytes_per_quantum: usize,
    pub scanned_headers: usize,
    pub crc_bytes: usize,
    pub compaction_bytes: usize,
}
/// Explicit SedsnetCrc32 UART Data profile; Opaque FrameReader is untouched.
/// IEEE CRC integrity is not SDK admission, schema validation or authentication.
/// Persist this reader across cancellation. In recovery, local Command frames
/// are unavailable until a CRC-valid Data frame re-establishes alignment.
pub struct StrictReader {
    bytes: [u8; CAPACITY],
    len: usize,
    tainted: bool,
    cursor: usize,
    first_incomplete: Option<usize>,
    checked_through: usize,
    need_read: bool,
    pub metrics: Metrics,
}
impl Default for StrictReader {
    fn default() -> Self {
        Self {
            bytes: [0; CAPACITY],
            len: 0,
            tainted: false,
            cursor: 0,
            first_incomplete: None,
            checked_through: 0,
            need_read: true,
            metrics: Metrics::default(),
        }
    }
}
pub fn crc_valid(body: &[u8]) -> bool {
    body.len() >= 8
        && crc32fast::hash(&body[..body.len() - 4])
            == u32::from_le_bytes(body[body.len() - 4..].try_into().unwrap())
}
impl StrictReader {
    pub fn buffered(&self) -> usize {
        self.len
    }
    pub fn recovering(&self) -> bool {
        self.tainted
    }
    fn discard(&mut self, n: usize) {
        self.metrics.compaction_bytes += self.len - n;
        self.bytes.copy_within(n..self.len, 0);
        self.len -= n;
        self.checked_through = self.checked_through.saturating_sub(n);
    }
    fn commit(
        &mut self,
        at: usize,
        n: usize,
        kind: u8,
        output: &mut [u8],
    ) -> Result<(u8, usize), ()> {
        if n > output.len() {
            return Err(());
        }
        output[..n].copy_from_slice(&self.bytes[at + 4..at + 4 + n]);
        self.discard(at + 4 + n);
        self.tainted = false;
        self.cursor = 0;
        self.first_incomplete = None;
        self.checked_through = 0;
        Ok((kind, n))
    }
    /// Each I/O completion is committed before the next await. No await occurs
    /// between copying an accepted frame and retiring its exact bytes.
    pub async fn read(
        &mut self,
        input: &mut impl Read,
        output: &mut [u8],
    ) -> Result<(u8, usize), ()> {
        let mut scans = 0;
        let mut crc_bytes = 0;
        loop {
            if self.need_read {
                let count = match input.read(&mut self.bytes[self.len..]).await {
                    Ok(0) | Err(_) => {
                        self.len = 0;
                        self.cursor = 0;
                        self.first_incomplete = None;
                        self.checked_through = 0;
                        // Read errors do not make unverified embedded commands safe.
                        return Err(());
                    }
                    Ok(n) => n,
                };
                self.len += count;
                self.need_read = false;
                self.metrics.peak_buffered = self.metrics.peak_buffered.max(self.len);
            }
            if scans >= SCAN_BUDGET || crc_bytes >= CRC_BUDGET {
                self.metrics.peak_scans_per_quantum =
                    self.metrics.peak_scans_per_quantum.max(scans);
                self.metrics.peak_crc_bytes_per_quantum =
                    self.metrics.peak_crc_bytes_per_quantum.max(crc_bytes);
                self.metrics.yields += 1;
                scans = 0;
                crc_bytes = 0;
                embassy_futures::yield_now().await;
            }
            let at = if self.tainted { self.cursor } else { 0 };
            if self.len >= at + 4 {
                let kind = self.bytes[at];
                let recognized = matches!((kind, self.bytes[at + 1]), (0xa5, 0x5a) | (0xa6, 0x5b));
                scans += 1;
                self.metrics.scanned_headers += 1;
                self.metrics.peak_scans_per_quantum =
                    self.metrics.peak_scans_per_quantum.max(scans);
                if !recognized {
                    if self.tainted {
                        self.cursor += 1;
                    } else {
                        self.discard(1);
                    }
                    continue;
                }
                let n = u16::from_le_bytes([self.bytes[at + 2], self.bytes[at + 3]]) as usize;
                if n > 4096 {
                    if self.tainted {
                        self.cursor += 1;
                    } else {
                        self.tainted = true;
                        self.cursor = 1;
                        self.first_incomplete = None;
                    }
                    continue;
                }
                if self.tainted && kind == 0xa6 {
                    self.metrics.quarantined_command_headers += 1;
                    self.cursor += 1;
                    continue;
                }
                let end = at + 4 + n;
                if self.len >= end {
                    if kind == 0xa6 {
                        return self.commit(at, n, kind, output);
                    }
                    if end > self.checked_through {
                        if crc_bytes + n > CRC_BUDGET {
                            // Do not advance candidate until the next quantum.
                            scans = SCAN_BUDGET;
                            continue;
                        }
                        crc_bytes += n;
                        self.metrics.crc_bytes += n;
                        self.metrics.peak_crc_bytes_per_quantum =
                            self.metrics.peak_crc_bytes_per_quantum.max(crc_bytes);
                        if crc_valid(&self.bytes[at + 4..end]) {
                            return self.commit(at, n, kind, output);
                        }
                        self.metrics.rejected_data += 1;
                    }
                    if !self.tainted {
                        self.tainted = true;
                        self.cursor = 1;
                        self.first_incomplete = None;
                    } else {
                        self.cursor += 1;
                    }
                    continue;
                }
                if self.tainted {
                    self.first_incomplete =
                        Some(self.first_incomplete.map_or(at, |old| old.min(at)));
                    self.cursor += 1;
                    continue;
                }
            } else if self.tainted {
                // Every complete candidate in this window has been evaluated.
                self.checked_through = self.len;
                let keep_at = self.first_incomplete.unwrap_or(self.len.saturating_sub(3));
                self.discard(keep_at);
                self.cursor = 0;
                self.first_incomplete = None;
            }
            assert!(
                self.len < CAPACITY,
                "full window must have a complete candidate or discardable prefix"
            );
            self.need_read = true;
        }
    }
}
