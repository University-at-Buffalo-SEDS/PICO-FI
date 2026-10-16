//! Allocation-free DHCP / probed IPv4LL policy. Driver I/O stays in net.rs.
use super::ipv4ll::link_local_candidate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Clear,
    Dhcp,
    Candidate([u8; 4]),
    Probe,
    Claim([u8; 4]),
    Announce,
}
#[derive(Clone, Copy, Debug)]
enum Phase {
    Down,
    StartDhcp,
    Dhcp {
        until: u64,
    },
    Choose {
        after: u64,
    },
    ProbeDelay {
        until: u64,
        sent: u8,
    },
    ProbeTx {
        until: u64,
        sent: u8,
    },
    ClaimDelay {
        until: u64,
    },
    Online {
        fallback: bool,
        idle_since: u64,
        announce_at: Option<(u64, u8)>,
    },
}
pub struct AutoAddress {
    mac: [u8; 6],
    phase: Phase,
    attempt: u32,
    address: [u8; 4],
}
impl AutoAddress {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            phase: Phase::Down,
            attempt: 0,
            address: [0; 4],
        }
    }
    pub fn tick(
        &mut self,
        now: u64,
        link: bool,
        configured: bool,
        connected: bool,
        pending: bool,
        conflict: bool,
    ) -> Action {
        if !link {
            if !matches!(self.phase, Phase::Down) {
                self.phase = Phase::Down;
                return Action::Clear;
            }
            return Action::None;
        }
        match self.phase {
            Phase::Down | Phase::StartDhcp => {
                self.phase = Phase::Dhcp {
                    until: now + 10_000,
                };
                Action::Dhcp
            }
            Phase::Dhcp { until } => {
                if configured {
                    self.phase = Phase::Online {
                        fallback: false,
                        idle_since: now,
                        announce_at: None,
                    };
                } else if now >= until {
                    self.phase = Phase::Choose { after: now };
                    return Action::Clear;
                }
                Action::None
            }
            Phase::Choose { after } if now >= after => {
                self.address = link_local_candidate(self.mac, self.attempt);
                self.attempt = self.attempt.wrapping_add(1);
                self.phase = Phase::ProbeDelay {
                    until: now + u64::from(self.mac[5]) * 3,
                    sent: 0,
                };
                Action::Candidate(self.address)
            }
            Phase::ProbeDelay { .. } | Phase::ProbeTx { .. } | Phase::ClaimDelay { .. }
                if conflict =>
            {
                self.retry(now)
            }
            Phase::ProbeDelay { until, sent } if now >= until => {
                self.phase = Phase::ProbeTx {
                    until: now + 2000,
                    sent,
                };
                Action::Probe
            }
            Phase::ProbeTx { until, sent } => {
                if !pending {
                    self.phase = if sent + 1 == 3 {
                        Phase::ClaimDelay {
                            until: now + 3000 + u64::from(self.mac[4]),
                        }
                    } else {
                        Phase::ProbeDelay {
                            until: now + 1000 + u64::from(self.mac[4]),
                            sent: sent + 1,
                        }
                    };
                } else if now >= until {
                    return self.retry(now);
                }
                Action::None
            }
            Phase::ClaimDelay { until } if now >= until => {
                self.phase = Phase::Online {
                    fallback: true,
                    idle_since: now,
                    announce_at: Some((now, 2)),
                };
                Action::Claim(self.address)
            }
            Phase::Online {
                fallback,
                mut idle_since,
                announce_at,
            } => {
                if (fallback && conflict) || !configured {
                    self.phase = Phase::StartDhcp;
                    return Action::Clear;
                }
                if connected {
                    idle_since = now;
                }
                if fallback && now.saturating_sub(idle_since) >= 60_000 {
                    self.phase = Phase::StartDhcp;
                    return Action::Clear;
                }
                if let Some((at, remaining)) = announce_at
                    && now >= at
                    && !pending
                {
                    self.phase = Phase::Online {
                        fallback,
                        idle_since,
                        announce_at: (remaining > 1).then_some((now + 2000, remaining - 1)),
                    };
                    return Action::Announce;
                }
                self.phase = Phase::Online {
                    fallback,
                    idle_since,
                    announce_at,
                };
                Action::None
            }
            _ => Action::None,
        }
    }
    fn retry(&mut self, now: u64) -> Action {
        self.phase = Phase::Choose {
            after: now
                + if self.attempt.is_multiple_of(10) {
                    60_000
                } else {
                    0
                },
        };
        Action::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MAC: [u8; 6] = [2, 1, 2, 3, 4, 5];
    fn tick(
        policy: &mut AutoAddress,
        now: u64,
        configured: bool,
        pending: bool,
        conflict: bool,
    ) -> Action {
        policy.tick(now, true, configured, false, pending, conflict)
    }
    fn fallback(policy: &mut AutoAddress) -> ([u8; 4], u64) {
        assert_eq!(tick(policy, 0, false, false, false), Action::Dhcp);
        assert_eq!(tick(policy, 9999, false, false, false), Action::None);
        assert_eq!(tick(policy, 10000, false, false, false), Action::Clear);
        let Action::Candidate(ip) = tick(policy, 10020, false, false, false) else {
            panic!("candidate missing")
        };
        let mut now = 10020 + u64::from(MAC[5]) * 3;
        for sent in 0..3 {
            assert_eq!(tick(policy, now, false, false, false), Action::Probe);
            assert_eq!(tick(policy, now + 10, false, true, false), Action::None);
            assert_eq!(tick(policy, now + 20, false, false, false), Action::None);
            now += 20 + if sent == 2 { 3000 } else { 1000 } + u64::from(MAC[4]);
        }
        assert_eq!(tick(policy, now - 1, false, false, false), Action::None);
        assert_eq!(tick(policy, now, false, false, false), Action::Claim(ip));
        (ip, now)
    }
    #[test]
    fn dhcp_success_never_claims_or_probes_link_local() {
        let mut p = AutoAddress::new(MAC);
        assert_eq!(tick(&mut p, 0, false, false, false), Action::Dhcp);
        assert_eq!(tick(&mut p, 100, true, false, false), Action::None);
        assert_eq!(tick(&mut p, 100000, true, false, false), Action::None);
        assert_eq!(tick(&mut p, 100001, false, false, false), Action::Clear);
        assert_eq!(tick(&mut p, 100020, false, false, false), Action::Dhcp);
    }
    #[test]
    fn no_dhcp_requires_three_transmitted_probes_then_two_announcements() {
        let mut p = AutoAddress::new(MAC);
        let (_, now) = fallback(&mut p);
        assert_eq!(tick(&mut p, now + 20, true, false, false), Action::Announce);
        assert_eq!(tick(&mut p, now + 2019, true, false, false), Action::None);
        assert_eq!(
            tick(&mut p, now + 2020, true, false, false),
            Action::Announce
        );
        assert_eq!(tick(&mut p, now + 5000, true, false, false), Action::None);
    }
    #[test]
    fn tcp_session_prevents_dhcp_retry_but_idle_fallback_recovers() {
        let mut p = AutoAddress::new(MAC);
        let (_, now) = fallback(&mut p);
        for at in [now + 60000, now + 120000, now + 180000] {
            assert!(!matches!(
                p.tick(at, true, true, true, false, false),
                Action::Clear | Action::Dhcp
            ));
        }
        assert!(!matches!(
            tick(&mut p, now + 239999, true, false, false),
            Action::Clear
        ));
        assert_eq!(
            tick(&mut p, now + 240000, true, false, false),
            Action::Clear
        );
        assert_eq!(
            tick(&mut p, now + 240020, false, false, false),
            Action::Dhcp
        );
    }
    #[test]
    fn conflicts_or_missing_tx_never_allow_an_address_to_be_claimed() {
        let mut p = AutoAddress::new(MAC);
        tick(&mut p, 0, false, false, false);
        tick(&mut p, 10000, false, false, false);
        let first = tick(&mut p, 10020, false, false, false);
        assert_eq!(tick(&mut p, 10040, false, false, true), Action::Clear);
        let second = tick(&mut p, 10060, false, false, false);
        assert_ne!(first, second);
        assert_eq!(tick(&mut p, 10100, false, false, false), Action::Probe);
        assert_eq!(tick(&mut p, 12100, false, true, false), Action::Clear);
        assert!(matches!(
            tick(&mut p, 12120, false, false, false),
            Action::Candidate(_)
        ));
    }
    #[test]
    fn link_loss_and_live_address_conflict_relinquish_immediately() {
        let mut p = AutoAddress::new(MAC);
        let (_, now) = fallback(&mut p);
        assert_eq!(
            p.tick(now + 20, false, true, true, false, false),
            Action::Clear
        );
        assert_eq!(
            p.tick(now + 40, false, false, false, false, false),
            Action::None
        );
        assert_eq!(
            p.tick(now + 60, true, false, false, false, false),
            Action::Dhcp
        );
        let mut p = AutoAddress::new(MAC);
        let (_, now) = fallback(&mut p);
        assert_eq!(tick(&mut p, now + 20, true, false, true), Action::Clear);
        assert_eq!(tick(&mut p, now + 40, false, false, false), Action::Dhcp);
    }
    #[test]
    fn repeated_conflicts_are_rate_limited_and_never_reuse_the_previous_candidate() {
        let mut p = AutoAddress::new(MAC);
        tick(&mut p, 0, false, false, false);
        tick(&mut p, 10000, false, false, false);
        let mut now = 10020;
        let mut previous = None;
        for _ in 0..10 {
            let Action::Candidate(ip) = tick(&mut p, now, false, false, false) else {
                panic!("candidate missing")
            };
            assert_ne!(previous, Some(ip));
            previous = Some(ip);
            assert_eq!(tick(&mut p, now + 1, false, false, true), Action::Clear);
            now += 2;
        }
        assert_eq!(tick(&mut p, now + 59998, false, false, false), Action::None);
        assert!(matches!(
            tick(&mut p, now + 60000, false, false, false),
            Action::Candidate(_)
        ));
    }
}
