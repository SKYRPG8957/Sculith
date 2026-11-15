//! Same-machine media policy for a loopback-only signaling endpoint.
//! WebRTC often binds host-interface sockets and does not gather loopback.
use std::net::{IpAddr, Ipv4Addr};

const MAX_INTERFACE_ADDRESSES: usize = 16;

pub(super) struct LocalMedia {
    addresses: Vec<IpAddr>,
}

impl LocalMedia {
    pub fn detect(loopback: IpAddr) -> std::io::Result<Self> {
        let addresses = if loopback.is_ipv4() {
            if_addrs::get_if_addrs()?
                .into_iter()
                .map(|interface| interface.ip())
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self::from_addresses(loopback, addresses))
    }

    pub(super) fn from_addresses(loopback: IpAddr, addresses: Vec<IpAddr>) -> Self {
        let mut owned: Vec<_> = addresses
            .into_iter()
            .filter(|ip| {
                ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
            })
            .collect();
        owned.sort_unstable();
        owned.dedup();
        owned.truncate(MAX_INTERFACE_ADDRESSES);
        let mut addresses = vec![loopback];
        if loopback.is_ipv4() {
            addresses.extend(owned);
        }
        Self { addresses }
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        ip.is_ipv4() == self.addresses[0].is_ipv4()
            && (ip.is_loopback() || self.addresses.contains(&ip))
    }

    pub fn bind_ip(&self) -> IpAddr {
        if self.addresses.len() > 1 {
            Ipv4Addr::UNSPECIFIED.into()
        } else {
            self.addresses[0]
        }
    }

    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_os_owned_addresses_and_loopback_are_allowed() {
        let policy = LocalMedia::from_addresses(
            "127.0.0.1".parse().unwrap(),
            vec!["192.0.2.10".parse().unwrap(), "192.0.2.10".parse().unwrap()],
        );
        assert!(policy.allows("127.0.0.2".parse().unwrap()));
        assert!(policy.allows("192.0.2.10".parse().unwrap()));
        assert!(!policy.allows("192.0.2.11".parse().unwrap()));
        assert!(!policy.allows("0.0.0.0".parse().unwrap()));
        assert!(!policy.allows("::1".parse().unwrap()));
        assert_eq!(policy.addresses().len(), 2);
        assert_eq!(policy.bind_ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let v6 =
            LocalMedia::from_addresses("::1".parse().unwrap(), vec!["192.0.2.10".parse().unwrap()]);
        assert_eq!(v6.addresses().len(), 1);
        assert!(!v6.allows("192.0.2.10".parse().unwrap()));
    }

    #[test]
    fn interface_enumeration_is_bounded_and_excludes_unspecified_and_multicast() {
        let mut addresses = vec![
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "224.0.0.1".parse().unwrap(),
        ];
        addresses.extend((1..=100).map(|last| IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))));
        let policy = LocalMedia::from_addresses("127.0.0.1".parse().unwrap(), addresses);
        assert_eq!(policy.addresses().len(), 1 + MAX_INTERFACE_ADDRESSES);
        assert!(!policy.addresses().contains(&"224.0.0.1".parse().unwrap()));
        assert!(!policy.addresses().contains(&"0.0.0.0".parse().unwrap()));
    }
}
