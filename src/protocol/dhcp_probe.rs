//! Bounded DHCP-presence probes while a link-local TCP session is still healthy.
//! The regular network stack acquires/renews the lease after a matching offer.
const ETHER: usize = 14;
const IP: usize = 20;
const UDP: usize = 8;
const BOOTP: usize = ETHER + IP + UDP;
pub const FRAME_SIZE: usize = BOOTP + 300;
const COOKIE: [u8; 4] = [99, 130, 83, 99];
fn sum(bytes: &[u8], mut value: u32) -> u32 {
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        value += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
    }
    if let [last] = chunks.remainder() {
        value += u32::from(*last) << 8;
    }
    value
}
fn finish(mut value: u32) -> u16 {
    while value > 0xffff {
        value = (value & 0xffff) + (value >> 16);
    }
    !(value as u16)
}
/// RFC 2131 DISCOVER with broadcast replies requested, not a lease request.
pub fn discover(mac: [u8; 6], xid: u32) -> [u8; FRAME_SIZE] {
    let mut frame = [0; FRAME_SIZE];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&[8, 0]);
    frame[ETHER] = 0x45;
    frame[ETHER + 2..ETHER + 4].copy_from_slice(&((IP + UDP + 300) as u16).to_be_bytes());
    frame[ETHER + 8] = 64;
    frame[ETHER + 9] = 17;
    frame[ETHER + 16..ETHER + 20].fill(255);
    let checksum = finish(sum(&frame[ETHER..ETHER + IP], 0));
    frame[ETHER + 10..ETHER + 12].copy_from_slice(&checksum.to_be_bytes());
    frame[ETHER + IP..ETHER + IP + 2].copy_from_slice(&68u16.to_be_bytes());
    frame[ETHER + IP + 2..ETHER + IP + 4].copy_from_slice(&67u16.to_be_bytes());
    frame[ETHER + IP + 4..ETHER + IP + 6].copy_from_slice(&((UDP + 300) as u16).to_be_bytes());
    // UDP checksum zero is permitted for IPv4. No IP is claimed by this packet.
    frame[BOOTP..BOOTP + 3].copy_from_slice(&[1, 1, 6]);
    frame[BOOTP + 4..BOOTP + 8].copy_from_slice(&xid.to_be_bytes());
    frame[BOOTP + 10] = 0x80;
    frame[BOOTP + 28..BOOTP + 34].copy_from_slice(&mac);
    frame[BOOTP + 236..BOOTP + 240].copy_from_slice(&COOKIE);
    frame[BOOTP + 240..BOOTP + 246].copy_from_slice(&[53, 1, 1, 61, 7, 1]);
    frame[BOOTP + 246..BOOTP + 252].copy_from_slice(&mac);
    frame[BOOTP + 252..BOOTP + 258].copy_from_slice(&[55, 3, 1, 3, 6, 255]);
    frame
}
fn unicast(ip: &[u8]) -> bool {
    ip.len() == 4 && ip[0] != 0 && ip[0] != 127 && ip[0] < 224
}
/// Reject stale transactions, other clients, malformed lengths, fragments and
/// checksum failures before an offer can trigger an address/session migration.
pub fn is_offer(frame: &[u8], mac: [u8; 6], xid: u32) -> bool {
    if xid == 0
        || frame.len() < BOOTP + 243
        || frame[12..14] != [8, 0]
        || (frame[..6] != mac && frame[..6] != [255; 6])
    {
        return false;
    }
    let ip = &frame[ETHER..];
    let ihl = usize::from(ip[0] & 15) * 4;
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if ip[0] >> 4 != 4
        || ihl < IP
        || total < ihl + UDP + 243
        || total > ip.len()
        || ip[9] != 17
        || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0
        || !unicast(&ip[12..16])
        || finish(sum(&ip[..ihl], 0)) != 0
    {
        return false;
    }
    let udp = &ip[ihl..total];
    let length = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp[..4] != [0, 67, 0, 68] || length < UDP + 243 || length != udp.len() {
        return false;
    }
    if udp[6..8] != [0, 0] && finish(sum(udp, sum(&ip[12..20], 17 + length as u32))) != 0 {
        return false;
    }
    let bootp = &udp[UDP..];
    if bootp[..3] != [2, 1, 6]
        || bootp[4..8] != xid.to_be_bytes()
        || bootp[28..34] != mac
        || !unicast(&bootp[16..20])
        || bootp[16..18] == [169, 254]
        || bootp[236..240] != COOKIE
    {
        return false;
    }
    let mut options = &bootp[240..];
    let mut offer = false;
    let mut server = false;
    while let Some((&kind, rest)) = options.split_first() {
        options = rest;
        if kind == 255 {
            return offer && server;
        }
        if kind == 0 {
            continue;
        }
        let Some((&length, rest)) = options.split_first() else {
            return false;
        };
        let length = usize::from(length);
        if rest.len() < length {
            return false;
        }
        let value = &rest[..length];
        options = &rest[length..];
        match kind {
            53 => {
                if offer || value != [2] {
                    return false;
                }
                offer = true;
            }
            54 => {
                if server || !unicast(value) {
                    return false;
                }
                server = true;
            }
            _ => {}
        }
    }
    false
}
#[cfg(test)]
pub(crate) fn offer_fixture(mac: [u8; 6], xid: u32) -> [u8; FRAME_SIZE] {
    let mut frame = discover(mac, xid);
    frame[6..12].copy_from_slice(&[2, 9, 8, 7, 6, 5]);
    frame[ETHER + 12..ETHER + 16].copy_from_slice(&[192, 168, 30, 1]);
    frame[ETHER + 10..ETHER + 12].fill(0);
    let checksum = finish(sum(&frame[ETHER..ETHER + IP], 0));
    frame[ETHER + 10..ETHER + 12].copy_from_slice(&checksum.to_be_bytes());
    frame[ETHER + IP..ETHER + IP + 4].copy_from_slice(&[0, 67, 0, 68]);
    frame[BOOTP] = 2;
    frame[BOOTP + 16..BOOTP + 20].copy_from_slice(&[192, 168, 30, 10]);
    frame[BOOTP + 240..].fill(0);
    frame[BOOTP + 240..BOOTP + 250].copy_from_slice(&[53, 1, 2, 54, 4, 192, 168, 30, 1, 255]);
    frame
}
#[cfg(test)]
mod tests {
    use super::*;
    const MAC: [u8; 6] = [2, 1, 2, 3, 4, 5];
    const XID: u32 = 0x12345678;
    #[test]
    fn discover_is_a_real_zero_source_broadcast_with_correct_lengths() {
        let f = discover(MAC, XID);
        assert_eq!(f.len(), 342);
        assert_eq!(&f[26..30], &[0; 4]);
        assert_eq!(&f[30..34], &[255; 4]);
        assert_eq!(&f[BOOTP + 10..BOOTP + 12], &[128, 0]);
        assert_eq!(finish(sum(&f[14..34], 0)), 0);
        assert_eq!(&f[BOOTP + 4..BOOTP + 8], &XID.to_be_bytes());
        assert!(!is_offer(&f, MAC, XID));
    }
    #[test]
    fn matching_offer_only_and_every_truncation_is_bounded() {
        let f = offer_fixture(MAC, XID);
        assert!(is_offer(&f, MAC, XID));
        assert!(!is_offer(&f, MAC, XID + 1));
        assert!(!is_offer(&f, [3; 6], XID));
        for length in 0..f.len() {
            assert!(!is_offer(&f[..length], MAC, XID));
        }
    }
    #[test]
    fn rejects_bad_lengths_checksums_options_and_non_offers() {
        let original = offer_fixture(MAC, XID);
        for (offset, value) in [
            (14, 0x44),
            (16, 0xff),
            (20, 0x20),
            (23, 6),
            (24, 0xff),
            (34, 1),
            (35, 68),
            (38, 0xff),
            (40, 1),
            (BOOTP, 1),
            (BOOTP + 1, 2),
            (BOOTP + 2, 5),
            (BOOTP + 28, 0),
            (BOOTP + 236, 0),
            (BOOTP + 242, 5),
            (BOOTP + 244, 255),
            (BOOTP + 245, 0),
            (BOOTP + 249, 0),
        ] {
            let mut f = original;
            f[offset] = value;
            assert!(!is_offer(&f, MAC, XID), "accepted corruption at {offset}");
        }
    }
    #[test]
    fn nonzero_udp_checksum_is_verified() {
        let mut f = offer_fixture(MAC, XID);
        let value = finish(sum(&f[34..], sum(&f[26..34], 17 + 308)));
        f[40..42].copy_from_slice(&value.to_be_bytes());
        assert!(is_offer(&f, MAC, XID));
        f[BOOTP + 250] = 1;
        assert!(!is_offer(&f, MAC, XID));
    }
    #[test]
    fn arbitrary_frames_never_panic() {
        let mut f = [0u8; 1514];
        let mut seed = 1u32;
        for _ in 0..10000 {
            for v in &mut f {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = (seed >> 24) as u8;
            }
            let length = seed as usize % f.len();
            let _ = is_offer(&f[..length], MAC, XID);
        }
    }
}
