use std::{
    io::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::UdpSocket;

/// Public UDP socket used by Bedrock's `NetherNet` ICE agent.
#[derive(Clone, Debug)]
pub struct IceSocket {
    socket: Arc<UdpSocket>,
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
        Ok(Self { socket })
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
        Ok(Self {
            socket: Arc::new(UdpSocket::from_std(socket)?),
        })
    }

    pub async fn recv_from(&self, buffer: &mut [u8]) -> Result<(usize, SocketAddr), Error> {
        self.socket.recv_from(buffer).await
    }

    pub(super) fn try_recv_from(&self, buffer: &mut [u8]) -> Result<(usize, SocketAddr), Error> {
        self.socket.try_recv_from(buffer)
    }

    pub(super) fn try_send_to(&self, buffer: &[u8], target: SocketAddr) -> Result<usize, Error> {
        self.socket.try_send_to(buffer, target)
    }

    pub async fn send_to(&self, buffer: &[u8], target: SocketAddr) -> Result<usize, Error> {
        self.socket.send_to(buffer, target).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
