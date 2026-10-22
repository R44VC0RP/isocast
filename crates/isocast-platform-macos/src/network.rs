//! Snapshot interface prefixes for remote-control's local-network UX guard.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub fn prefixes() -> Vec<(IpAddr, u8)> {
    let mut list = std::ptr::null_mut();
    let mut prefixes = Vec::new();
    // SAFETY: libc owns the returned linked list until freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return prefixes;
    }
    let mut item = list;
    unsafe {
        while !item.is_null() {
            let entry = &*item;
            if !entry.ifa_addr.is_null()
                && !entry.ifa_netmask.is_null()
                && entry.ifa_flags & libc::IFF_UP as u32 != 0
            {
                match (*entry.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let addr = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        let mask = &*(entry.ifa_netmask as *const libc::sockaddr_in);
                        let mask = u32::from_be(mask.sin_addr.s_addr);
                        let bits = mask.leading_ones();
                        if bits > 0 && mask == u32::MAX << (32 - bits) {
                            prefixes.push((
                                Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()).into(),
                                bits as u8,
                            ));
                        }
                    }
                    libc::AF_INET6 => {
                        let addr = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        let mask = &*(entry.ifa_netmask as *const libc::sockaddr_in6);
                        let mask = u128::from_be_bytes(mask.sin6_addr.s6_addr);
                        let bits = mask.leading_ones();
                        if bits > 0 && mask == u128::MAX << (128 - bits) {
                            prefixes
                                .push((Ipv6Addr::from(addr.sin6_addr.s6_addr).into(), bits as u8));
                        }
                    }
                    _ => {}
                }
            }
            item = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    prefixes
}
