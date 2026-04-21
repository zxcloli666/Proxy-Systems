use std::net::Ipv6Addr;

use rand::RngCore;

#[derive(Clone, Debug)]
pub struct Subnet {
    pub prefix: Ipv6Addr,
    pub prefix_len: u8,
}

impl Subnet {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("IPV6_SUBNET").ok()?;
        Self::parse(&raw)
    }

    pub fn parse(s: &str) -> Option<Self> {
        let (addr_str, prefix_str) = s.trim().split_once('/')?;
        let prefix_len: u8 = prefix_str.parse().ok()?;
        if prefix_len > 128 {
            return None;
        }
        let ip: Ipv6Addr = addr_str.parse().ok()?;
        Some(Subnet {
            prefix: mask_prefix(ip, prefix_len),
            prefix_len,
        })
    }

    /// Pick a random IPv6 address within this subnet.
    pub fn random_addr(&self) -> Ipv6Addr {
        let prefix_bytes = self.prefix.octets();
        let plen = self.prefix_len as u32;

        if plen == 128 {
            return self.prefix;
        }

        let mut rnd = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut rnd);

        let mut out = [0u8; 16];
        for i in 0..16 {
            let bit_start = (i as u32) * 8;
            let bit_end = bit_start + 8;
            if bit_start >= plen {
                out[i] = rnd[i];
            } else if bit_end > plen {
                let host_bits = bit_end - plen;
                let host_mask: u8 = ((1u16 << host_bits) - 1) as u8;
                out[i] = (prefix_bytes[i] & !host_mask) | (rnd[i] & host_mask);
            } else {
                out[i] = prefix_bytes[i];
            }
        }
        Ipv6Addr::from(out)
    }
}

fn mask_prefix(ip: Ipv6Addr, prefix_len: u8) -> Ipv6Addr {
    let bytes = ip.octets();
    let plen = prefix_len as u32;
    let mut out = [0u8; 16];
    for i in 0..16 {
        let bit_start = (i as u32) * 8;
        let bit_end = bit_start + 8;
        if bit_end <= plen {
            out[i] = bytes[i];
        } else if bit_start >= plen {
            out[i] = 0;
        } else {
            let host_bits = bit_end - plen;
            let net_mask: u8 = !(((1u16 << host_bits) - 1) as u8);
            out[i] = bytes[i] & net_mask;
        }
    }
    Ipv6Addr::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slash_64() {
        let s = Subnet::parse("2001:db8:abcd:1234::/64").unwrap();
        assert_eq!(s.prefix_len, 64);
        let r = s.random_addr();
        let rb = r.octets();
        let pb = s.prefix.octets();
        assert_eq!(&rb[..8], &pb[..8]);
    }

    #[test]
    fn parse_full_128() {
        let s = Subnet::parse("::1/128").unwrap();
        assert_eq!(s.random_addr(), Ipv6Addr::LOCALHOST);
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(Subnet::parse("2001:db8::/200").is_none());
        assert!(Subnet::parse("not-an-ip/64").is_none());
    }
}
