//! Native interface prefixes; snapshots are cached by the listener.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use windows::Win32::{
    Foundation::ERROR_BUFFER_OVERFLOW,
    NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
    Networking::WinSock::*,
};

pub fn prefixes() -> Vec<(IpAddr, u8)> {
    let mut size = 15000u32;
    for _ in 0..3 {
        if size > 1024 * 1024 {
            break;
        }
        // usize storage provides the alignment required by the API structures.
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: the aligned allocation is at least size bytes and remains
        // alive for the complete traversal of the OS-created linked lists.
        let result = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(head),
                &mut size,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW.0 {
            continue;
        }
        if result != 0 {
            break;
        }
        let mut prefixes = Vec::new();
        unsafe {
            let mut adapter = head;
            while !adapter.is_null() {
                if (*adapter).OperStatus == IfOperStatusUp {
                    let mut address = (*adapter).FirstUnicastAddress;
                    while !address.is_null() {
                        let unicast = &*address;
                        let socket = unicast.Address.lpSockaddr;
                        let bits = unicast.OnLinkPrefixLength;
                        if !socket.is_null() && bits > 0 {
                            if (*socket).sa_family == AF_INET
                                && bits <= 32
                                && unicast.Address.iSockaddrLength as usize
                                    >= size_of::<SOCKADDR_IN>()
                            {
                                let addr = &*socket.cast::<SOCKADDR_IN>();
                                prefixes.push((
                                    Ipv4Addr::from(addr.sin_addr.S_un.S_addr.to_ne_bytes()).into(),
                                    bits,
                                ));
                            } else if (*socket).sa_family == AF_INET6
                                && bits <= 128
                                && unicast.Address.iSockaddrLength as usize
                                    >= size_of::<SOCKADDR_IN6>()
                            {
                                let addr = &*socket.cast::<SOCKADDR_IN6>();
                                prefixes.push((Ipv6Addr::from(addr.sin6_addr.u.Byte).into(), bits));
                            }
                        }
                        address = unicast.Next;
                    }
                }
                adapter = (*adapter).Next;
            }
        }
        return prefixes;
    }
    Vec::new()
}
