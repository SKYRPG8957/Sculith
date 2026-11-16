//! Local media compatibility and bounded direct-peer UDP endpoint policy.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::UdpSocket;

const MAX_INTERFACE_ADDRESSES: usize = 16;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMedia {
    /// Inclusive local UDP range; NAT forwarding uses the same public ports.
    pub ports: [u16; 2],
    #[serde(default)]
    pub advertised_ip: Option<IpAddr>,
}

fn unicast(ip: IpAddr) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && ip != IpAddr::V4(Ipv4Addr::BROADCAST)
        && match ip {
            IpAddr::V6(v6) => v6.to_canonical() == ip && !v6.is_unicast_link_local(),
            _ => true,
        }
}

impl PublicMedia {
    pub fn validate(&self, bind: IpAddr, capacity: u32) -> Result<IpAddr> {
        let [first, last] = self.ports;
        ensure!(
            first != 0 && first <= last && u32::from(last) - u32::from(first) < 64,
            "NetherNet media ports must be an ordered inclusive range of 1 to 64 nonzero UDP ports"
        );
        ensure!(
            u32::from(last) - u32::from(first) + 1 >= capacity,
            "NetherNet media port range must cover server.max_players"
        );
        ensure!(
            !bind.is_loopback(),
            "public NetherNet media requires a non-loopback signaling bind"
        );
        let advertised = self.advertised_ip.unwrap_or(bind);
        ensure!(
            unicast(advertised) && advertised.is_ipv4() == bind.is_ipv4(),
            "NetherNet media requires a unicast advertised_ip matching the signaling address family; wildcard binds need an explicit IP"
        );
        Ok(advertised)
    }
}

pub(super) struct Endpoint {
    bind: IpAddr,
    addresses: [IpAddr; 1],
    ports: [u16; 2],
}

pub(super) enum Media {
    Local(Arc<LocalMedia>),
    Public(Arc<Endpoint>),
}

impl Media {
    pub fn prepare(bind: IpAddr, public: Option<&PublicMedia>) -> Result<Self> {
        if bind.is_loopback() {
            ensure!(
                public.is_none(),
                "public NetherNet media settings cannot be used with loopback signaling"
            );
            Ok(Self::Local(Arc::new(LocalMedia::detect(bind)?)))
        } else {
            let public =
                public.context("public NetherNet requires bedrock.nethernet_media settings")?;
            let advertised = public.validate(bind, 0)?;
            Ok(Self::Public(Arc::new(Endpoint {
                bind,
                addresses: [advertised],
                ports: public.ports,
            })))
        }
    }

    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public(_))
    }

    pub fn for_peer(&self, remote: Option<SocketAddr>) -> Result<Arc<PeerMedia>> {
        match self {
            Self::Local(local) => Ok(Arc::new(PeerMedia::Local(local.clone()))),
            Self::Public(endpoint) => {
                let peer = remote
                    .context("public NetherNet offer has no TCP peer address")?
                    .ip();
                ensure!(
                    unicast(peer) && peer.is_ipv4() == endpoint.bind.is_ipv4(),
                    "NetherNet TCP peer is outside the configured media address family"
                );
                Ok(Arc::new(PeerMedia::Public {
                    endpoint: endpoint.clone(),
                    peer,
                }))
            }
        }
    }
}

pub(super) enum PeerMedia {
    Local(Arc<LocalMedia>),
    Public {
        endpoint: Arc<Endpoint>,
        peer: IpAddr,
    },
}

impl PeerMedia {
    pub fn allows(&self, ip: IpAddr) -> bool {
        match self {
            Self::Local(local) => local.allows(ip),
            Self::Public { peer, .. } => *peer == ip,
        }
    }

    pub fn addresses(&self) -> &[IpAddr] {
        match self {
            Self::Local(local) => local.addresses(),
            Self::Public { endpoint, .. } => &endpoint.addresses,
        }
    }

    pub fn advertised_ip(&self, local: IpAddr) -> IpAddr {
        match self {
            Self::Local(_) => local,
            Self::Public { endpoint, .. } => endpoint.addresses[0],
        }
    }

    pub async fn bind(&self) -> std::io::Result<UdpSocket> {
        match self {
            Self::Local(local) => UdpSocket::bind(SocketAddr::new(local.bind_ip(), 0)).await,
            Self::Public { endpoint, .. } => {
                for port in endpoint.ports[0]..=endpoint.ports[1] {
                    match UdpSocket::bind(SocketAddr::new(endpoint.bind, port)).await {
                        Ok(socket) => return Ok(socket),
                        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "NetherNet media UDP port range is exhausted",
                ))
            }
        }
    }
}

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
    fn public_settings_require_bounded_ports_capacity_and_a_concrete_matching_advertisement() {
        let mut config = PublicMedia {
            ports: [20000, 20063],
            advertised_ip: Some("192.0.2.10".parse().unwrap()),
        };
        let wildcard = "0.0.0.0".parse().unwrap();
        assert_eq!(
            config.validate(wildcard, 64).unwrap(),
            "192.0.2.10".parse::<IpAddr>().unwrap()
        );
        for ports in [[0, 1], [20000, 19999], [20000, 20064], [20000, 20001]] {
            config.ports = ports;
            assert!(config.validate(wildcard, 20).is_err());
        }
        config.ports = [20000, 20063];
        for address in [
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "2001:db8::1",
        ] {
            config.advertised_ip = Some(address.parse().unwrap());
            assert!(config.validate(wildcard, 20).is_err());
        }
        config.advertised_ip = None;
        assert!(config.validate(wildcard, 20).is_err());
        assert!(config.validate("192.0.2.10".parse().unwrap(), 20).is_ok());
        config.advertised_ip = Some("2001:db8::1".parse().unwrap());
        assert!(config.validate("::".parse().unwrap(), 20).is_ok());
    }

    #[test]
    fn public_candidates_and_datagrams_are_limited_to_the_actual_tcp_peer() {
        let config = PublicMedia {
            ports: [20000, 20063],
            advertised_ip: Some("192.0.2.10".parse().unwrap()),
        };
        let policy = Media::prepare("0.0.0.0".parse().unwrap(), Some(&config)).unwrap();
        assert!(policy.for_peer(None).is_err());
        assert!(
            policy
                .for_peer(Some("127.0.0.1:1234".parse().unwrap()))
                .is_err()
        );
        assert!(
            policy
                .for_peer(Some("[2001:db8::2]:1234".parse().unwrap()))
                .is_err()
        );
        let peer = policy
            .for_peer(Some("192.0.2.44:1234".parse().unwrap()))
            .unwrap();
        assert!(peer.allows("192.0.2.44".parse().unwrap()));
        for blocked in [
            "192.0.2.45",
            "192.0.2.10",
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::ffff:192.0.2.44",
        ] {
            assert!(!peer.allows(blocked.parse().unwrap()), "{blocked}");
        }
        assert_eq!(
            peer.advertised_ip("0.0.0.0".parse().unwrap()),
            "192.0.2.10".parse::<IpAddr>().unwrap()
        );
        assert_eq!(peer.addresses(), &["192.0.2.10".parse::<IpAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn busy_udp_ports_are_preserved_and_released_ports_are_reused() {
        let occupied = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = occupied.local_addr().unwrap().port();
        let config = PublicMedia {
            ports: [port, port],
            advertised_ip: Some("192.0.2.10".parse().unwrap()),
        };
        let media = Media::prepare("0.0.0.0".parse().unwrap(), Some(&config)).unwrap();
        let peer = media
            .for_peer(Some("192.0.2.44:1234".parse().unwrap()))
            .unwrap();
        assert_eq!(
            peer.bind().await.unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        drop(occupied);
        let first = peer.bind().await.unwrap();
        assert_eq!(first.local_addr().unwrap().port(), port);
        assert_eq!(
            peer.bind().await.unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        drop(first);
        let reused = peer.bind().await.unwrap();
        assert_eq!(reused.local_addr().unwrap().port(), port);
    }

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
