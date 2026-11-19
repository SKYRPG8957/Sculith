use std::{
    io::{Error, ErrorKind, IoSliceMut},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::UdpSocket;
use webrtc::runtime::{RecvMeta, Transmit, UdpSockRef, UdpSocketState};

/// Public UDP socket used by Bedrock's `NetherNet` ICE agent.
#[derive(Clone, Debug)]
pub struct IceSocket {
    socket: Arc<UdpSocket>,
    batch: Option<Arc<UdpSocketState>>,
}

impl IceSocket {
    pub async fn bind(address: SocketAddr) -> Result<Self, Error> {
        let ip = match address.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
        };
        let socket = Arc::new(UdpSocket::bind((ip, address.port())).await?);
        // Bursts from all players arrive on this one shared listener.
        let options = socket2::SockRef::from(socket.as_ref());
        let _ = options.set_recv_buffer_size(4 * 1024 * 1024);
        let _ = options.set_send_buffer_size(4 * 1024 * 1024);
        Ok(Self {
            socket,
            batch: None,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        self.socket.local_addr()
    }

    /// Transfer the bound port to the listener's I/O driver. A caller retaining a
    /// clone keeps its handle, while the server's usual sole owner needs no duplicate.
    pub(super) fn into_std(self) -> Result<std::net::UdpSocket, Error> {
        match Arc::try_unwrap(self.socket) {
            Ok(socket) => socket.into_std(),
            Err(shared) => Ok(socket2::SockRef::from(shared.as_ref()).try_clone()?.into()),
        }
    }

    pub(super) fn from_std(socket: std::net::UdpSocket) -> Result<Self, Error> {
        let socket = Arc::new(UdpSocket::from_std(socket)?);
        #[cfg(target_os = "linux")]
        let batch = Self::enable_batch(&socket)?;
        #[cfg(not(target_os = "linux"))]
        let batch = None;
        Ok(Self { socket, batch })
    }

    #[cfg(target_os = "linux")]
    fn enable_batch(socket: &UdpSocket) -> Result<Option<Arc<UdpSocketState>>, Error> {
        use std::os::fd::AsRawFd;

        // Quinn selects its own fragmentation policy. The discovery and signaling messages
        // sent on this same socket still need the original IPv4 path MTU settings.
        let mut mtu_policy: libc::c_int = 0;
        let mut size = size_of::<libc::c_int>() as libc::socklen_t;
        let fd = socket.as_raw_fd();
        // SAFETY: fd remains open and both writable values match the IP_MTU_DISCOVER ABI.
        let read = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                (&raw mut mtu_policy).cast(),
                &raw mut size,
            )
        };
        if read != 0 {
            return Ok(None);
        }
        let batch = UdpSocketState::new(UdpSockRef::from(socket))
            .ok()
            .map(Arc::new);
        // SAFETY: fd remains open and mtu_policy contains the socket's previous c_int value.
        let restored = unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                (&raw const mtu_policy).cast(),
                size,
            )
        };
        if restored != 0 {
            return Err(Error::last_os_error());
        }
        Ok(batch)
    }

    pub async fn recv_from(&self, buffer: &mut [u8]) -> Result<(usize, SocketAddr), Error> {
        self.socket.recv_from(buffer).await
    }

    pub(super) async fn recv_datagrams(
        &self,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Result<usize, Error> {
        if let Some(batch) = &self.batch {
            return self
                .socket
                .async_io(tokio::io::Interest::READABLE, || {
                    let count =
                        batch.recv(UdpSockRef::from(self.socket.as_ref()), buffers, meta)?;
                    if count == 0 {
                        return Err(ErrorKind::WouldBlock.into());
                    }
                    Ok(count)
                })
                .await;
        }
        let (len, addr) = self.socket.recv_from(&mut buffers[0]).await?;
        meta[0].addr = addr;
        meta[0].len = len;
        meta[0].stride = len;
        let mut count = 1;
        for (buffer, info) in buffers.iter_mut().zip(meta).skip(1) {
            let Ok((len, addr)) = self.socket.try_recv_from(buffer) else {
                break;
            };
            info.addr = addr;
            info.len = len;
            info.stride = len;
            count += 1;
        }
        Ok(count)
    }

    pub(super) fn max_gso_segments(&self) -> usize {
        self.batch
            .as_ref()
            .map_or(1, |state| state.max_gso_segments())
    }

    pub(super) fn try_send_transmit(
        &self,
        transmit: &Transmit<'_>,
        target: SocketAddr,
    ) -> Result<usize, Error> {
        if transmit
            .segment_size
            .is_none_or(|size| size >= transmit.contents.len())
        {
            return self.socket.try_send_to(transmit.contents, target);
        }
        let batch = self.batch.as_ref().ok_or(ErrorKind::Unsupported)?;
        let outgoing = Transmit {
            destination: target,
            ecn: transmit.ecn,
            contents: transmit.contents,
            segment_size: transmit.segment_size,
            src_ip: None,
        };
        self.socket.try_io(tokio::io::Interest::WRITABLE, || {
            batch
                .try_send(UdpSockRef::from(self.socket.as_ref()), &outgoing)
                .map(|()| outgoing.contents.len())
        })
    }

    pub async fn send_to(&self, buffer: &[u8], target: SocketAddr) -> Result<usize, Error> {
        self.socket.send_to(buffer, target).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn segmented_transmit_preserves_datagrams_and_port() {
        let socket = || {
            IceSocket::from_std(std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap())
                .unwrap()
        };
        let sender = socket();
        if sender.max_gso_segments() <= 1 {
            return;
        }
        let receiver = socket();
        let target = receiver.local_addr().unwrap();
        let transmit = Transmit {
            destination: target,
            ecn: None,
            contents: b"abcdEFGHijk",
            segment_size: Some(4),
            src_ip: None,
        };
        sender.socket.writable().await.unwrap();
        assert_eq!(sender.try_send_transmit(&transmit, target).unwrap(), 11);

        let mut scratch = [[0; 64]; 3];
        let mut buffers = scratch.each_mut().map(|buffer| IoSliceMut::new(buffer));
        let mut meta = [RecvMeta::default(); 3];
        let mut packets = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while packets.len() < 3 {
                let count = receiver
                    .recv_datagrams(&mut buffers, &mut meta)
                    .await
                    .unwrap();
                for (data, info) in buffers.iter().zip(&meta).take(count) {
                    assert_eq!(info.addr, sender.local_addr().unwrap());
                    packets.extend(
                        data[..info.len]
                            .chunks(info.stride.max(1))
                            .map(<[u8]>::to_vec),
                    );
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            packets,
            [b"abcd".to_vec(), b"EFGH".to_vec(), b"ijk".to_vec()]
        );
    }

    #[tokio::test]
    async fn transferring_a_shared_socket_keeps_the_bound_port() {
        let original = IceSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let transferred = IceSocket::from_std(original.clone().into_std().unwrap()).unwrap();
        let port = original.local_addr().unwrap();
        assert_eq!(transferred.local_addr().unwrap(), port);
        drop(original);
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        transferred
            .send_to(b"ready", peer.local_addr().unwrap())
            .await
            .unwrap();
        let mut buffer = [0; 5];
        let (len, sender) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            peer.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(sender, port);
        assert_eq!(&buffer[..len], b"ready");
    }

    #[tokio::test]
    async fn ice_socket_sends_and_receives() {
        let server = IceSocket::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        client
            .send_to(b"ping", server.local_addr().unwrap())
            .await
            .unwrap();

        let mut buffer = [0; 16];
        let (length, address) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..length], b"ping");
        assert_eq!(address, client.local_addr().unwrap());

        server
            .send_to(b"pong", client.local_addr().unwrap())
            .await
            .unwrap();

        let (length, address) = client.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..length], b"pong");
        assert_eq!(address, server.local_addr().unwrap());
    }
}
