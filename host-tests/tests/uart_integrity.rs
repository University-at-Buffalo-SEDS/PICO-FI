//! Framing integrity tests use deterministic CRC-valid bytes, not fabricated
//! claims of SDK schema/admission validity or physical UART timing.
use pico_fi_host_tests::strict_uart::StrictReader;
use embedded_io_async::{ErrorType, Read};
use std::{future::Future, task::{Context, Poll, Waker}};
struct Input { bytes: Vec<u8>, at: usize, chunk: usize }
impl ErrorType for Input { type Error = core::convert::Infallible; }
impl Read for Input {
    async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        std::future::poll_fn(|_| {
            if self.at == self.bytes.len() { return Poll::Pending; }
            let n = out.len().min(self.chunk).min(self.bytes.len()-self.at);
            out[..n].copy_from_slice(&self.bytes[self.at..self.at+n]); self.at+=n;
            Poll::Ready(Ok(n))
        }).await
    }
}
fn poll<F: Future>(f: F) -> Poll<F::Output> {
    let mut f=std::pin::pin!(f);
    f.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}
fn wire(kind: u8, body: &[u8]) -> Vec<u8> {
    let n=u16::try_from(body.len()).unwrap().to_le_bytes();
    let second=if kind==0xa5 {0x5a} else {0x5b};
    [vec![kind,second,n[0],n[1]],body.to_vec()].concat()
}
fn data(mut bytes: Vec<u8>) -> Vec<u8> {
    assert!(bytes.len()>=4);
    let crc=crc32fast::hash(&bytes).to_le_bytes(); bytes.extend(crc); bytes
}
fn drain(bytes: Vec<u8>, chunk: usize) -> (Vec<(u8,Vec<u8>)>,StrictReader) {
    let mut reader=StrictReader::default(); let mut input=Input{bytes,at:0,chunk};
    let mut frames=Vec::new(); let mut out=[0;4096]; let mut idle=0;
    for _ in 0..100000 {
        let before=input.at;
        match poll(reader.read(&mut input,&mut out)) {
            Poll::Ready(Ok((k,n)))=>{frames.push((k,out[..n].to_vec()));idle=0;},
            Poll::Ready(Err(()))=>panic!("no EOF/error injected"),
            Poll::Pending=>{if before==input.at {idle+=1;} else {idle=0;}}
        }
        if idle==256 {return(frames,reader);}
    }
    panic!("reader failed bounded progress");
}
#[test]
fn truncated_lengths_cannot_lock_onto_embedded_magic() {
    let mut suffix=Vec::new(); let mut expected=Vec::new();
    for id in 0u32..64 {
        let mut body=vec![0x31;504];body[..4].copy_from_slice(&id.to_le_bytes());
        body[64..68].copy_from_slice(&[0xa5,0x5a,0xfc,1]);
        body[256..260].copy_from_slice(&[0xa6,0x5b,0xfc,1]);
        let body=data(body);suffix.extend(wire(0xa5,&body));expected.push((0xa5,body));
    }
    for length in [1024u16,4096] {
        let mut prefix=vec![0xa5,0x5a];prefix.extend(length.to_le_bytes());prefix.extend(vec![0x31;480]);prefix.extend(&suffix);
        for chunk in [1,31,512,4100] {
            let (actual,r)=drain(prefix.clone(),chunk);assert_eq!(actual,expected);
            assert!(!r.recovering());assert!(r.metrics.rejected_data>0);
            assert!(r.metrics.peak_scans_per_quantum<=64);
            assert!(r.metrics.peak_crc_bytes_per_quantum<=4096);
        }
    }
}
#[test]
fn valid_parent_keeps_nested_frames_opaque() {
    let nested=wire(0xa5,&data(vec![7;20]));
    let mut body=vec![9;16];body.extend(nested);body.extend(wire(0xa6,b"reset"));
    let body=data(body);let (frames,_)=drain(wire(0xa5,&body),1);
    assert_eq!(frames,vec![(0xa5,body)]);
}
#[test]
fn damaged_data_quarantines_commands_until_valid_data_anchor() {
    let mut body=data(vec![1;100]);body[0]^=1;
    let anchor=data(vec![2;20]);
    let bytes=[wire(0xa6,b"status"),wire(0xa5,&body),wire(0xa6,b"reset"),wire(0xa5,&anchor),wire(0xa6,b"status")].concat();
    let (frames,r)=drain(bytes,1);
    assert_eq!(frames,vec![(0xa6,b"status".to_vec()),(0xa5,anchor),(0xa6,b"status".to_vec())]);
    assert!(r.metrics.quarantined_command_headers>0);
}
#[test]
fn cancellation_at_every_split_preserves_partial_frame() {
    let body=data(vec![3;300]);let bytes=wire(0xa5,&body);
    for split in 0..bytes.len() {
        let mut reader=StrictReader::default();let mut input=Input{bytes:bytes[..split].to_vec(),at:0,chunk:7};let mut out=[0;4096];
        assert!(poll(reader.read(&mut input,&mut out)).is_pending());
        input.bytes.extend_from_slice(&bytes[split..]);
        let mut received=None;
        for _ in 0..256 {if let Poll::Ready(value)=poll(reader.read(&mut input,&mut out)){received=Some(value.unwrap());break;}}
        assert_eq!(received,Some((0xa5,body.len())));assert_eq!(&out[..body.len()],body);
    }
}
#[test]
fn empty_pending_input_does_not_rescan_or_recheck() {
    let mut corrupt=data(vec![4;100]);corrupt[2]^=1;
    let mut reader=StrictReader::default();let mut input=Input{bytes:wire(0xa5,&corrupt),at:0,chunk:512};let mut out=[0;4096];
    for _ in 0..32 {assert!(poll(reader.read(&mut input,&mut out)).is_pending());}
    let counts=(reader.metrics.scanned_headers,reader.metrics.crc_bytes);
    for _ in 0..256 {assert!(poll(reader.read(&mut input,&mut out)).is_pending());}
    assert_eq!(counts,(reader.metrics.scanned_headers,reader.metrics.crc_bytes));
}
