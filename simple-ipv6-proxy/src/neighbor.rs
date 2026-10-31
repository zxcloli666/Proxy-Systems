use std::fs;
use std::io;
use std::mem;
use std::net::Ipv6Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};

const WARN_EVERY: Duration = Duration::from_secs(60);
const NEIGHBOR_SOLICITATION: u8 = 135;
const SOURCE_LINK_LAYER: u8 = 1;

pub struct Primer {
    gateway: Ipv6Addr,
    packet: [u8; 32],
    last_warn: Mutex<Option<Instant>>,
}

impl Primer {
    pub fn from_env() -> Option<Self> {
        let default_route = default_route();
        let iface = env_text("NDP_IFACE").or(default_route.as_ref().map(|r| r.1.clone()))?;
        let gateway = env_text("NDP_GATEWAY")
            .and_then(|s| s.parse().ok())
            .or(default_route.map(|r| r.0))?;
        let mac = read_mac(&iface)?;

        info!("NDP priming: gateway {gateway} on {iface}");
        Some(Primer {
            gateway,
            packet: solicitation(gateway, mac),
            last_warn: Mutex::new(None),
        })
    }

    pub fn prime(&self, source: Ipv6Addr) {
        if let Err(e) = send(source, self.gateway, &self.packet) {
            self.warn_once_a_minute(source, e);
        }
    }

    fn warn_once_a_minute(&self, source: Ipv6Addr, error: io::Error) {
        let mut last = self.last_warn.lock().unwrap();
        if last.is_some_and(|at| at.elapsed() < WARN_EVERY) {
            return;
        }
        *last = Some(Instant::now());
        warn!("NDP priming from {source} failed: {error}");
    }
}

fn solicitation(gateway: Ipv6Addr, mac: [u8; 6]) -> [u8; 32] {
    let mut packet = [0u8; 32];
    packet[0] = NEIGHBOR_SOLICITATION;
    packet[8..24].copy_from_slice(&gateway.octets());
    packet[24] = SOURCE_LINK_LAYER;
    packet[25] = 1;
    packet[26..32].copy_from_slice(&mac);
    packet
}

fn send(source: Ipv6Addr, gateway: Ipv6Addr, packet: &[u8]) -> io::Result<()> {
    let fd = unsafe {
        libc::socket(
            libc::AF_INET6,
            libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            libc::IPPROTO_ICMPV6,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = send_on(fd, source, gateway, packet);
    unsafe { libc::close(fd) };
    result
}

fn send_on(fd: i32, source: Ipv6Addr, gateway: Ipv6Addr, packet: &[u8]) -> io::Result<()> {
    let hops: libc::c_int = 255;
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_UNICAST_HOPS,
            &hops as *const _ as *const libc::c_void,
            mem::size_of_val(&hops) as libc::socklen_t,
        )
    };
    if set < 0 {
        return Err(io::Error::last_os_error());
    }

    let from = sockaddr(source);
    let bound = unsafe {
        libc::bind(
            fd,
            &from as *const _ as *const libc::sockaddr,
            mem::size_of_val(&from) as libc::socklen_t,
        )
    };
    if bound < 0 {
        return Err(io::Error::last_os_error());
    }

    let to = sockaddr(gateway);
    let sent = unsafe {
        libc::sendto(
            fd,
            packet.as_ptr() as *const libc::c_void,
            packet.len(),
            0,
            &to as *const _ as *const libc::sockaddr,
            mem::size_of_val(&to) as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sockaddr(addr: Ipv6Addr) -> libc::sockaddr_in6 {
    let mut raw: libc::sockaddr_in6 = unsafe { mem::zeroed() };
    raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    raw.sin6_addr.s6_addr = addr.octets();
    raw
}

fn env_text(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn read_mac(iface: &str) -> Option<[u8; 6]> {
    let text = fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?;
    let mut mac = [0u8; 6];
    let mut parts = text.trim().split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    Some(mac)
}

fn default_route() -> Option<(Ipv6Addr, String)> {
    let table = fs::read_to_string("/proc/net/ipv6_route").ok()?;
    table.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let is_default = f.len() >= 10 && f[0] == "0".repeat(32) && f[1] == "00";
        let gateway = f.get(4).filter(|g| is_default && **g != "0".repeat(32))?;
        let bits = u128::from_str_radix(gateway, 16).ok()?;
        Some((Ipv6Addr::from(bits), f[9].to_string()))
    })
}
