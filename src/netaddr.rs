//! Address filtering and grouping.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Turn IPv4-mapped IPv6 addresses into plain IPv4, so one host has one key.
pub fn normalize(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
            None => addr,
        },
        IpAddr::V4(_) => addr,
    }
}

/// True if `addr` is worth connecting to on the public internet: a global
/// unicast IPv4 or IPv6 address with a non-zero port, and not an onion-cat
/// (Tor) address.
pub fn is_crawlable(addr: SocketAddr) -> bool {
    addr.port() != 0
        && match normalize(addr).ip() {
            IpAddr::V4(ip) => is_global_v4(ip),
            IpAddr::V6(ip) => is_global_v6(ip),
        }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        // 100.64.0.0/10 shared address space (carrier-grade NAT)
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        // 192.0.0.0/24 IETF protocol assignments
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 198.18.0.0/15 benchmarking
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        // 240.0.0.0/4 reserved
        || o[0] >= 240)
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // fc00::/7 unique local, which includes the onion-cat range fd87:d87e:eb43::/48
        || (s[0] & 0xfe00) == 0xfc00
        // fe80::/10 link local
        || (s[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32 documentation
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        // ::/96 IPv4-compatible (deprecated) and 64:ff9b::/96 NAT64 are not nodes
        || (s[0..6] == [0, 0, 0, 0, 0, 0])
        || (s[0] == 0x64 && s[1] == 0xff9b))
}

/// The subnet an address is counted under in `--group-by subnet`: the IPv4
/// /24 or the IPv6 /48 that contains it, written in CIDR notation.
pub fn subnet_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => subnet_key(IpAddr::V4(v4)),
            None => {
                let s = v6.segments();
                format!("{}/48", Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn crawlable() {
        for ok in [
            "1.2.3.4:8233",
            "[2a01:4f8::1]:8233",
            "[::ffff:1.2.3.4]:8233",
            "8.8.8.8:1",
        ] {
            assert!(is_crawlable(sa(ok)), "{ok}");
        }
        for bad in [
            "1.2.3.4:0",
            "0.0.0.0:8233",
            "127.0.0.1:8233",
            "10.1.2.3:8233",
            "172.16.0.1:8233",
            "192.168.1.1:8233",
            "169.254.1.1:8233",
            "100.64.0.1:8233",
            "198.18.0.1:8233",
            "203.0.113.1:8233",
            "224.0.0.1:8233",
            "255.255.255.255:8233",
            "[::]:8233",
            "[::1]:8233",
            "[fe80::1]:8233",
            "[fd87:d87e:eb43::1]:8233",
            "[fc00::1]:8233",
            "[2001:db8::1]:8233",
            "[ff02::1]:8233",
            "[::ffff:127.0.0.1]:8233",
            "[::1.2.3.4]:8233",
            "[64:ff9b::1.2.3.4]:8233",
        ] {
            assert!(!is_crawlable(sa(bad)), "{bad}");
        }
    }

    #[test]
    fn subnets() {
        assert_eq!(subnet_key("1.2.3.4".parse().unwrap()), "1.2.3.0/24");
        assert_eq!(subnet_key("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.0/24");
        assert_eq!(
            subnet_key("2a01:4f8:abcd:1234::1".parse().unwrap()),
            "2a01:4f8:abcd::/48"
        );
        assert_eq!(
            subnet_key("2a01:4f8:abcd:1::1".parse().unwrap()),
            subnet_key("2a01:4f8:abcd:ffff::2".parse().unwrap())
        );
    }

    #[test]
    fn normalizes_mapped() {
        assert_eq!(normalize(sa("[::ffff:1.2.3.4]:8233")), sa("1.2.3.4:8233"));
        assert_eq!(normalize(sa("[2a01::1]:8233")), sa("[2a01::1]:8233"));
    }
}
