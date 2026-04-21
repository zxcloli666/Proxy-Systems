use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Reject hosts that resolve (or literally point) to non-routable / internal
/// address space. Prevents accidental SSRF via a crafted `X-Target`.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d — v4-mapped; decode and validate as v4.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_v4(mapped);
            }
            is_blocked_v6(v6)
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
    {
        return true;
    }
    let o = ip.octets();
    // 0.0.0.0/8 "this network"
    if o[0] == 0 {
        return true;
    }
    // 100.64.0.0/10 CGN
    if o[0] == 100 && (64..128).contains(&o[1]) {
        return true;
    }
    // 192.0.0.0/24 IETF protocol assignments
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return true;
    }
    // 192.88.99.0/24 6to4 anycast
    if o[0] == 192 && o[1] == 88 && o[2] == 99 {
        return true;
    }
    // 198.18.0.0/15 benchmarking
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return true;
    }
    // 240.0.0.0/4 reserved
    if o[0] >= 240 {
        return true;
    }
    false
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return true;
    }
    let s = ip.segments();
    // fe80::/10 link-local
    if (s[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7 unique local (ULA)
    if (s[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // 2001:db8::/32 documentation
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return true;
    }
    // 64:ff9b:1::/48 NAT64 local-use — treat as internal
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_v4_internals() {
        for addr in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.9",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "203.0.113.1",
            "240.0.0.1",
        ] {
            assert!(is_blocked_ip(parse(addr)), "should block {}", addr);
        }
    }

    #[test]
    fn allows_v4_public() {
        for addr in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(!is_blocked_ip(parse(addr)), "should allow {}", addr);
        }
    }

    #[test]
    fn blocks_v6_internals() {
        for addr in [
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd00:dead:beef::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_blocked_ip(parse(addr)), "should block {}", addr);
        }
    }

    #[test]
    fn allows_v6_public() {
        for addr in ["2606:4700:4700::1111", "2a00:1450:4001:82b::200e"] {
            assert!(!is_blocked_ip(parse(addr)), "should allow {}", addr);
        }
    }
}
