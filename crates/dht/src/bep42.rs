//! BEP 42 (DHT security extension): node ids tied to the external address of the node, and the
//! vote that decides what our own external address is.
//!
//! <https://www.bittorrent.org/beps/bep_0042.html>

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};

use librqbit_core::hash_id::Id20;
use rand::Rng;

/// CRC32C (Castagnoli), reflected polynomial.
const CRC32C_POLY: u32 = 0x82F6_3B78;

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC32C (Castagnoli) of `data`, as used by iSCSI (RFC 3720) and BEP 42.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc = CRC32C_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

const V4_MASK: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];
const V6_MASK: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];

/// The CRC32C whose top 21 bits a node id for `ip` must start with, for the random number `r`
/// (only its 3 low bits are used).
fn id_prefix_crc(ip: IpAddr, r: u8) -> u32 {
    let r = r & 0x07;
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let mut buf = ip.octets();
            for (b, m) in buf.iter_mut().zip(V4_MASK) {
                *b &= m;
            }
            buf[0] |= r << 5;
            crc32c(&buf)
        }
        IpAddr::V6(ip) => {
            let octets = ip.octets();
            let mut buf = [0u8; 8];
            for ((b, o), m) in buf.iter_mut().zip(octets).zip(V6_MASK) {
                *b = o & m;
            }
            buf[0] |= r << 5;
            crc32c(&buf)
        }
    }
}

/// Builds a node id for `ip` from the random byte `rand` (its 3 low bits select the prefix and
/// it becomes the last byte of the id) and the random bytes that fill the rest.
fn node_id_from_parts(ip: IpAddr, rand: u8, filler: [u8; 17]) -> Id20 {
    let crc = id_prefix_crc(ip, rand);
    let [c0, c1, c2, _] = crc.to_be_bytes();
    let mut id = [0u8; 20];
    id[0] = c0;
    id[1] = c1;
    // Only the top 5 bits of the third byte come from the crc; the rest is random.
    id[2] = (c2 & 0xf8) | (filler[0] & 0x07);
    id[3..19].copy_from_slice(&filler[1..]);
    id[19] = rand;
    Id20::new(id)
}

/// Generates a random node id that is valid for the external address `ip` according to BEP 42.
pub fn generate_node_id(ip: IpAddr) -> Id20 {
    let mut random = [0u8; 18];
    rand::rng().fill_bytes(&mut random);
    let [rand, filler @ ..] = random;
    node_id_from_parts(ip, rand, filler)
}

/// True if the first 21 bits of `id` are the ones BEP 42 prescribes for `ip`, without any
/// exemption for local addresses.
pub(crate) fn node_id_matches_ip(id: &Id20, ip: IpAddr) -> bool {
    let crc = id_prefix_crc(ip, id.0[19]);
    let [c0, c1, c2, _] = crc.to_be_bytes();
    id.0[0] == c0 && id.0[1] == c1 && (id.0[2] & 0xf8) == (c2 & 0xf8)
}

/// True for the local network addresses that BEP 42 exempts from node id verification:
/// 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16 and 127.0.0.0/8. The IPv6
/// counterparts are exempt as well: loopback, link-local (fe80::/10) and unique local
/// (fc00::/7) addresses.
pub fn is_local_address(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
        }
    }
}

/// Whether `id` is a valid node id for a node at `ip` according to BEP 42.
///
/// Nodes on local network addresses (see [`is_local_address`]) are exempt and always valid.
pub fn is_valid_node_id(id: &Id20, ip: IpAddr) -> bool {
    is_local_address(ip) || node_id_matches_ip(id, ip)
}

/// Whether `ip` can be our external address: a node id derived from anything else would be
/// meaningless to the rest of the DHT.
fn is_public_address(ip: IpAddr) -> bool {
    if is_local_address(ip) || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(ip) => !ip.is_broadcast(),
        IpAddr::V6(_) => true,
    }
}

/// The network an address belongs to: its /24 (IPv4) or /48 (IPv6). Several nodes in one
/// network count as one vote, so that a single party cannot outvote the rest of the DHT cheaply;
/// the BEP 44 store shares itself out by the same networks. IPv6 end sites usually get a /48,
/// so anything narrower would let one site count many times.
pub(crate) fn voter_network(ip: IpAddr) -> [u8; 8] {
    let mut key = [0u8; 8];
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            key[0] = 4;
            key[1..4].copy_from_slice(&ip.octets()[..3]);
        }
        IpAddr::V6(ip) => {
            key[0] = 6;
            key[1..7].copy_from_slice(&ip.octets()[..6]);
        }
    }
    key
}

/// Votes from distinct voter networks that an address needs before we adopt it, when we have
/// no address yet.
const MIN_VOTES: u32 = 4;
/// Votes an address needs to replace one we adopted already. Higher than [`MIN_VOTES`]: moving
/// costs a new node id and a rebuilt routing table, so it should take more than a handful of
/// colluding nodes.
pub(crate) const VOTES_TO_REPLACE: u32 = 10;
/// How long an adopted address stays before a later vote may replace it.
const MIN_CHANGE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// A round without a decision is dropped after this many votes, so that stale votes go away
/// and memory stays bounded.
const MAX_ROUND_VOTES: u32 = 50;

/// Decides our external address of one address family from the `ip` field of KRPC responses.
///
/// Modelled after libtorrent's `ip_voter`: each voter network (see [`voter_network`]) votes at
/// most once per round, and so does each referrer (the node whose node list led us to the
/// voter), so that a node handing out many addresses it controls gets one vote for all of them.
/// An address wins when it has at least [`MIN_VOTES`] votes ([`VOTES_TO_REPLACE`] to replace
/// an adopted one) and at least twice as many as the runner-up, and an adopted address is not
/// replaced within [`MIN_CHANGE_INTERVAL`]. A single node, or a single network, cannot move us.
#[derive(Debug)]
pub(crate) struct ExternalIpVoter {
    adopted: Option<IpAddr>,
    last_change: Option<Instant>,
    candidates: Vec<(IpAddr, u32)>,
    voters: HashSet<[u8; 8]>,
    referrers: HashSet<[u8; 8]>,
    round_votes: u32,
}

impl ExternalIpVoter {
    pub(crate) fn new() -> Self {
        Self {
            adopted: None,
            last_change: None,
            candidates: Vec::new(),
            voters: HashSet::new(),
            referrers: HashSet::new(),
            round_votes: 0,
        }
    }

    /// The adopted external address, if any.
    pub(crate) fn adopted(&self) -> Option<IpAddr> {
        self.adopted
    }

    fn reset_round(&mut self) {
        self.candidates.clear();
        self.voters.clear();
        self.referrers.clear();
        self.round_votes = 0;
    }

    /// Records that `voter` told us our address is `reported`; `referrer` is the node that
    /// listed `voter` in an answer to us, if any. Returns the new external address when this
    /// vote changed it.
    pub(crate) fn vote(
        &mut self,
        voter: IpAddr,
        referrer: Option<IpAddr>,
        reported: IpAddr,
        now: Instant,
    ) -> Option<IpAddr> {
        let voter = voter.to_canonical();
        let reported = reported.to_canonical();
        if voter.is_ipv4() != reported.is_ipv4() || !is_public_address(reported) {
            return None;
        }
        let voter_key = voter_network(voter);
        if self.voters.contains(&voter_key) {
            return None;
        }
        if let Some(referrer) = referrer
            && !self.referrers.insert(voter_network(referrer))
        {
            return None;
        }
        self.voters.insert(voter_key);
        self.round_votes += 1;
        match self.candidates.iter_mut().find(|(ip, _)| *ip == reported) {
            Some((_, votes)) => *votes += 1,
            None => self.candidates.push((reported, 1)),
        }

        let mut leader: Option<(IpAddr, u32)> = None;
        let mut runner_up = 0;
        for &(ip, votes) in &self.candidates {
            match leader {
                Some((_, best)) if votes <= best => runner_up = runner_up.max(votes),
                Some((_, best)) => {
                    runner_up = best;
                    leader = Some((ip, votes));
                }
                None => leader = Some((ip, votes)),
            }
        }

        let quorum = if self.adopted.is_some() {
            VOTES_TO_REPLACE
        } else {
            MIN_VOTES
        };
        let decided = leader.filter(|&(_, votes)| votes >= quorum && votes >= 2 * runner_up);
        let result = match decided {
            Some((ip, _)) if self.adopted == Some(ip) => {
                // Confirmed what we have; start counting afresh.
                self.reset_round();
                None
            }
            Some((ip, _)) => {
                let too_soon = self
                    .last_change
                    .is_some_and(|t| now.saturating_duration_since(t) < MIN_CHANGE_INTERVAL);
                if too_soon {
                    None
                } else {
                    self.adopted = Some(ip);
                    self.last_change = Some(now);
                    self.reset_round();
                    Some(ip)
                }
            }
            None => None,
        };
        if self.round_votes >= MAX_ROUND_VOTES {
            self.reset_round();
        }
        result
    }
}

/// The external addresses adopted so far, IPv4 and IPv6 apart.
pub(crate) fn adopted_v4(voter: &ExternalIpVoter) -> Option<Ipv4Addr> {
    match voter.adopted() {
        Some(IpAddr::V4(ip)) => Some(ip),
        _ => None,
    }
}

/// See [`adopted_v4`].
pub(crate) fn adopted_v6(voter: &ExternalIpVoter) -> Option<Ipv6Addr> {
    match voter.adopted() {
        Some(IpAddr::V6(ip)) => Some(ip),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        str::FromStr,
        time::{Duration, Instant},
    };

    use librqbit_core::hash_id::Id20;

    use super::*;

    #[test]
    fn crc32c_check_value() {
        // The check value of the CRC catalogue.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn crc32c_rfc3720_vectors() {
        // RFC 3720, appendix B.4. The RFC prints the CRC in the byte order it goes on the wire,
        // i.e. least significant byte first.
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        let increasing: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&increasing), 0x46dd_794e);
        let decreasing: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&decreasing), 0x113f_db5c);
        // The iSCSI read command PDU of the same appendix.
        let pdu: [u8; 48] = [
            0x01, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14,
            0x00, 0x00, 0x00, 0x18, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(crc32c(&pdu), 0xd9963a56);
    }

    // The test vectors of BEP 42: IP, rand, an example node id.
    const VECTORS: [(&str, u8, &str); 5] = [
        (
            "124.31.75.21",
            1,
            "5fbfbff10c5d6a4ec8a88e4c6ab4c28b95eee401",
        ),
        (
            "21.75.31.124",
            86,
            "5a3ce9c14e7a08645677bbd1cfe7d8f956d53256",
        ),
        (
            "65.23.51.170",
            22,
            "a5d43220bc8f112a3d426c84764f8c2a1150e616",
        ),
        (
            "84.124.73.14",
            65,
            "1b0321dd1bb1fe518101ceef99462b947a01ff41",
        ),
        (
            "43.213.53.83",
            90,
            "e56f6cbf5b7c4be0237986d5243b87aa6d51305a",
        ),
    ];

    #[test]
    fn bep42_vectors_generate() {
        for (ip, rand, example) in VECTORS {
            let ip = IpAddr::from_str(ip).unwrap();
            let example = Id20::from_str(example).unwrap();
            let id = node_id_from_parts(ip, rand, [0xff; 17]);
            assert_eq!(id.0[..2], example.0[..2], "{ip}");
            assert_eq!(id.0[2] & 0xf8, example.0[2] & 0xf8, "{ip}");
            assert_eq!(id.0[19], rand, "{ip}");
        }
    }

    #[test]
    fn bep42_vectors_verify() {
        for (ip, _, example) in VECTORS {
            let ip = IpAddr::from_str(ip).unwrap();
            let example = Id20::from_str(example).unwrap();
            assert!(node_id_matches_ip(&example, ip), "{ip}");
            assert!(is_valid_node_id(&example, ip), "{ip}");
            // One bit off in the prefix and it no longer matches.
            let mut bad = example;
            bad.0[1] ^= 0x01;
            assert!(!is_valid_node_id(&bad, ip), "{ip}");
            // Another random number selects another prefix.
            let mut bad = example;
            bad.0[19] ^= 0x01;
            assert!(!is_valid_node_id(&bad, ip), "{ip}");
        }
    }

    #[test]
    fn generated_ids_are_valid() {
        let ips = [
            IpAddr::from_str("124.31.75.21").unwrap(),
            IpAddr::from_str("2001:db8:1234:5678::1").unwrap(),
            IpAddr::from_str("::ffff:65.23.51.170").unwrap(),
        ];
        for ip in ips {
            for _ in 0..64 {
                let id = generate_node_id(ip);
                assert!(node_id_matches_ip(&id, ip), "{ip} {id:?}");
            }
        }
        // The IPv4-mapped form and the plain IPv4 address are the same address.
        let id = generate_node_id(IpAddr::from_str("::ffff:65.23.51.170").unwrap());
        assert!(node_id_matches_ip(
            &id,
            IpAddr::from_str("65.23.51.170").unwrap()
        ));
    }

    #[test]
    fn ipv6_uses_the_first_8_bytes_only() {
        let a = IpAddr::from_str("2001:db8:1234:5678::1").unwrap();
        let b = IpAddr::from_str("2001:db8:1234:5678:ffff:ffff:ffff:ffff").unwrap();
        let id = generate_node_id(a);
        assert!(node_id_matches_ip(&id, b));
    }

    #[test]
    fn local_addresses_are_exempt() {
        let random = Id20::new([0x42; 20]);
        for ip in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.10.10",
            "127.0.0.1",
            "127.255.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "::ffff:10.0.0.1",
        ] {
            let ip = IpAddr::from_str(ip).unwrap();
            assert!(is_local_address(ip), "{ip}");
            assert!(is_valid_node_id(&random, ip), "{ip}");
        }
        for ip in ["172.32.0.1", "8.8.8.8", "2001:db8::1", "11.0.0.1"] {
            let ip = IpAddr::from_str(ip).unwrap();
            assert!(!is_local_address(ip), "{ip}");
            assert!(!is_valid_node_id(&random, ip), "{ip}");
        }
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn voter_one_liar_cannot_move_us() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        let ours = v4(1, 2, 3, 4);
        // The same node, or several nodes of one /24, count once.
        for i in 0..20 {
            assert_eq!(voter.vote(v4(5, 5, 5, i), None, ours, now), None);
        }
        assert_eq!(voter.adopted(), None);
        // Three more networks agree: four networks in total.
        assert_eq!(voter.vote(v4(6, 6, 6, 6), None, ours, now), None);
        assert_eq!(voter.vote(v4(7, 7, 7, 7), None, ours, now), None);
        assert_eq!(voter.vote(v4(8, 8, 8, 8), None, ours, now), Some(ours));
        assert_eq!(voter.adopted(), Some(ours));

        // Much later, a liar alone does not change our mind.
        let later = now + Duration::from_secs(3600);
        for i in 0..20 {
            assert_eq!(
                voter.vote(v4(9, 9, 9, i), None, v4(66, 66, 66, 66), later),
                None
            );
        }
        assert_eq!(voter.adopted(), Some(ours));
    }

    #[test]
    fn voter_one_referrer_one_vote() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        let ours = v4(1, 2, 3, 4);
        let lie = v4(66, 66, 66, 66);
        // A node lists many nodes of its own, each in another /24, and they all lie. Only the
        // first of them counts.
        let liar = Some(v4(50, 0, 0, 1));
        for n in 100..120 {
            assert_eq!(voter.vote(v4(n, 0, 0, 1), liar, lie, now), None);
        }
        assert_eq!(voter.adopted(), None);
        // Honest nodes, found through different nodes, outvote it: four of them are four votes
        // and twice the one vote of the liars.
        for n in 10..13u8 {
            assert_eq!(
                voter.vote(v4(n, 0, 0, 1), Some(v4(n, 9, 9, 9)), ours, now),
                None
            );
        }
        assert_eq!(
            voter.vote(v4(13, 0, 0, 1), Some(v4(13, 9, 9, 9)), ours, now),
            Some(ours)
        );
    }

    #[test]
    fn voter_replacing_needs_more_votes() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        let first = v4(1, 2, 3, 4);
        let second = v4(4, 3, 2, 1);
        for n in 10..13 {
            assert_eq!(voter.vote(v4(n, 0, 0, 1), None, first, now), None);
        }
        assert_eq!(voter.vote(v4(13, 0, 0, 1), None, first, now), Some(first));

        let later = now + MIN_CHANGE_INTERVAL + Duration::from_secs(1);
        for n in 1..VOTES_TO_REPLACE as u8 {
            assert_eq!(voter.vote(v4(100 + n, 0, 0, 1), None, second, later), None);
        }
        assert_eq!(voter.adopted(), Some(first));
        assert_eq!(
            voter.vote(v4(200, 0, 0, 1), None, second, later),
            Some(second)
        );
    }

    #[test]
    fn voter_majority_moves_us_but_not_too_often() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        let first = v4(1, 2, 3, 4);
        let second = v4(4, 3, 2, 1);
        for n in 10..13 {
            assert_eq!(voter.vote(v4(n, 0, 0, 1), None, first, now), None);
        }
        assert_eq!(voter.vote(v4(13, 0, 0, 1), None, first, now), Some(first));

        // A clear majority for another address right away: too soon to change.
        let soon = now + Duration::from_secs(60);
        for n in 20..32 {
            assert_eq!(voter.vote(v4(n, 0, 0, 1), None, second, soon), None);
        }
        assert_eq!(voter.adopted(), Some(first));

        // Once the interval passed, the next vote of the majority moves us.
        let later = now + MIN_CHANGE_INTERVAL + Duration::from_secs(1);
        assert_eq!(
            voter.vote(v4(32, 0, 0, 1), None, second, later),
            Some(second)
        );
        assert_eq!(voter.adopted(), Some(second));
    }

    #[test]
    fn voter_needs_a_clear_majority() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        let a = v4(1, 2, 3, 4);
        let b = v4(4, 3, 2, 1);
        // 4 against 3 is not twice as many.
        for n in 10..13 {
            assert_eq!(voter.vote(v4(n, 0, 0, 1), None, b, now), None);
        }
        for n in 20..24 {
            assert_eq!(voter.vote(v4(n, 0, 0, 1), None, a, now), None);
        }
        assert_eq!(voter.adopted(), None);
        // 6 against 3 is.
        assert_eq!(voter.vote(v4(24, 0, 0, 1), None, a, now), None);
        assert_eq!(voter.vote(v4(25, 0, 0, 1), None, a, now), Some(a));
    }

    #[test]
    fn voter_ignores_local_and_mismatched_reports() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        for n in 10..30 {
            // Local addresses say nothing about our place in the DHT.
            voter.vote(v4(n, 0, 0, 1), None, v4(192, 168, 0, 10), now);
            voter.vote(v4(n, 0, 0, 1), None, v4(127, 0, 0, 1), now);
            voter.vote(v4(n, 0, 0, 1), None, v4(0, 0, 0, 0), now);
            // An IPv4 voter cannot vote for an IPv6 address.
            voter.vote(
                v4(n, 0, 0, 1),
                None,
                IpAddr::V6(Ipv6Addr::from_str("2001:db8::1").unwrap()),
                now,
            );
        }
        assert_eq!(voter.adopted(), None);
    }

    #[test]
    fn voter_round_is_bounded() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        // Votes spread over many addresses never decide anything; after MAX_ROUND_VOTES the
        // round starts over instead of growing.
        for n in 0..MAX_ROUND_VOTES as u8 {
            voter.vote(v4(n + 1, 0, 0, 1), None, v4(100, 0, 0, n + 1), now);
        }
        assert!(voter.candidates.is_empty());
        assert!(voter.voters.is_empty());
        assert_eq!(voter.round_votes, 0);
    }

    #[test]
    fn voter_ipv6_networks() {
        let mut voter = ExternalIpVoter::new();
        let now = Instant::now();
        let ours = IpAddr::from_str("2001:db8:aaaa::1").unwrap();
        // Many addresses, even many /64s, in a single /48 are one vote.
        for i in 1..10u16 {
            let voter_ip = IpAddr::V6(Ipv6Addr::new(0x2a00, 1, 2, i, 0, 0, 0, 1));
            assert_eq!(voter.vote(voter_ip, None, ours, now), None);
        }
        for n in 1..3u16 {
            let voter_ip = IpAddr::V6(Ipv6Addr::new(0x2a00, 1, 2 + n, 3, 0, 0, 0, 1));
            assert_eq!(voter.vote(voter_ip, None, ours, now), None);
        }
        let voter_ip = IpAddr::V6(Ipv6Addr::new(0x2a00, 9, 9, 9, 0, 0, 0, 1));
        assert_eq!(voter.vote(voter_ip, None, ours, now), Some(ours));
        assert_eq!(adopted_v6(&voter), Some(ours.to_string().parse().unwrap()));
        assert_eq!(adopted_v4(&voter), None);
    }
}
