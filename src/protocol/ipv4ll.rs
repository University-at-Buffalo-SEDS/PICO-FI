//! Bounded IPv4 link-local candidate generation and Ethernet ARP validation.
/// RFC 3927 candidate; probing and conflict detection are required before claiming it.
pub fn link_local_candidate(mac: [u8; 6], attempt: u32) -> [u8; 4] {
    let mut hash = attempt.wrapping_mul(0x9e3779b9);
    for byte in mac {
        hash = hash.wrapping_mul(16777619) ^ u32::from(byte);
    }
    [169, 254, 1 + (hash % 254) as u8, (hash >> 8) as u8]
}
/// Ethernet ARP probe/announcement, including an explicit hardware/protocol shape.
pub fn arp_probe(mac: [u8; 6], ip: [u8; 4], announce: bool) -> [u8; 42] {
    let mut frame = [0; 42];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&mac);
    frame[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
    frame[22..28].copy_from_slice(&mac);
    if announce {
        frame[28..32].copy_from_slice(&ip);
    }
    frame[38..42].copy_from_slice(&ip);
    frame
}
pub fn arp_conflict(frame: &[u8], mac: [u8; 6], ip: [u8; 4]) -> bool {
    frame.len() >= 42
        && frame[12..20] == [8, 6, 0, 1, 8, 0, 6, 4]
        && matches!(&frame[20..22], [0, 1] | [0, 2])
        && frame[22..28] != mac
        && (frame[28..32] == ip || (frame[28..32] == [0; 4] && frame[38..42] == ip))
}

#[cfg(test)]
mod tests {
    use super::super::discovery::hardware_mac;
    use super::*;
    #[test]
    fn physical_identity_and_candidates_do_not_depend_on_static_network() {
        let a = hardware_mac(&[1; 8]);
        let b = hardware_mac(&[2; 8]);
        assert_ne!(a, b);
        assert_eq!(a[0] & 3, 2);
        /* Candidates are exercised by the IPv4LL driver tests. */
        let mut previous = None;
        for attempt in 0..1000 {
            let ip = link_local_candidate(a, attempt);
            assert_eq!(&ip[..2], &[169, 254]);
            assert!((1..=254).contains(&ip[2]));
            assert_ne!(previous, Some(ip));
            previous = Some(ip);
        }
    }
    #[test]
    fn arp_conflicts_include_other_probers_but_not_self_or_other_addresses() {
        let mac = [2, 1, 2, 3, 4, 5];
        let other = [2, 6, 7, 8, 9, 10];
        let ip = [169, 254, 100, 20];
        assert!(!arp_conflict(&arp_probe(mac, ip, false), mac, ip));
        assert!(arp_conflict(&arp_probe(other, ip, false), mac, ip));
        assert!(arp_conflict(&arp_probe(other, ip, true), mac, ip));
        assert!(!arp_conflict(
            &arp_probe(other, [169, 254, 100, 21], false),
            mac,
            ip
        ));
        for length in 0..42 {
            assert!(!arp_conflict(
                &arp_probe(other, ip, false)[..length],
                mac,
                ip
            ));
        }
        let mut malformed = arp_probe(other, ip, false);
        malformed[18] = 4;
        assert!(!arp_conflict(&malformed, mac, ip));
    }
}
