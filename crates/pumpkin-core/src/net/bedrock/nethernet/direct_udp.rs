//! Fast path for direct-IP ICE. The WebRTC peer retains its normal socket for
//! ICE probing, oversized datagrams and backpressure.
use super::ice_socket::IceSocket;
use bytes::Bytes;
use dashmap::DashMap;
use std::{
    future::Future,
    io::{self, IoSliceMut},
    net::{SocketAddr, TcpListener, UdpSocket},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::mpsc;
use webrtc::runtime::{
    AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, RecvMeta, Runtime,
    TokioRuntime, Transmit,
};

#[derive(Debug, Clone)]
pub(super) struct DirectUdp {
    public: IceSocket,
    relay: SocketAddr,
    targets: Arc<DashMap<SocketAddr, SocketAddr>>,
    inputs: Arc<DashMap<SocketAddr, mpsc::Sender<Bytes>>>,
}

impl DirectUdp {
    pub(super) fn new(public: IceSocket, relay: SocketAddr) -> Self {
        Self {
            public,
            relay,
            targets: Arc::default(),
            inputs: Arc::default(),
        }
    }

    pub(super) fn target(&self, internal: SocketAddr) -> Option<SocketAddr> {
        self.targets.get(&internal).map(|target| *target)
    }

    pub(super) fn set_target(&self, internal: SocketAddr, target: Option<SocketAddr>) {
        if let Some(target) = target {
            self.targets.insert(internal, target);
        } else {
            self.targets.remove(&internal);
        }
    }

    pub(super) fn deliver(&self, internal: SocketAddr, packet: &[u8]) -> bool {
        if packet.len() > 2048 {
            return false;
        }
        let Some(entry) = self.inputs.get(&internal) else {
            return false;
        };
        let Ok(slot) = entry.try_reserve() else {
            return false;
        };
        slot.send(Bytes::copy_from_slice(packet));
        true
    }
}

type Task<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl Runtime for DirectUdp {
    fn spawn(&self, task: Task<'static, ()>) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn(task)
    }
    fn spawn_reactor(&self, size: usize, task: Task<'static, ()>) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn_reactor(size, task)
    }
    fn wrap_udp_socket(&self, socket: UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let local = socket.local_addr()?;
        let socket = TokioRuntime.wrap_udp_socket(socket)?;
        if local.ip() != self.relay.ip() {
            return Ok(socket);
        }
        let (send, recv) = mpsc::channel(64);
        self.inputs.insert(local, send.clone());
        Ok(Arc::new(DirectSocket {
            local,
            socket,
            router: self.clone(),
            send,
            recv: Mutex::new(recv),
            prefer_socket: AtomicBool::new(false),
        }))
    }
    fn wrap_tcp_listener(&self, listener: TcpListener) -> io::Result<Arc<dyn AsyncTcpListener>> {
        TokioRuntime.wrap_tcp_listener(listener)
    }
    fn connect_tcp(&self, remote: SocketAddr) -> Task<'_, io::Result<Arc<dyn AsyncTcpStream>>> {
        TokioRuntime.connect_tcp(remote)
    }
    fn resolve_host<'a>(&'a self, host: &'a str) -> Task<'a, io::Result<Vec<SocketAddr>>> {
        TokioRuntime.resolve_host(host)
    }
    fn sleep(&self, time: Duration) -> Task<'static, ()> {
        TokioRuntime.sleep(time)
    }
    fn interval(&self, period: Duration) -> Box<dyn AsyncInterval> {
        TokioRuntime.interval(period)
    }
    fn block_on(&self, task: Pin<Box<dyn Future<Output = ()> + '_>>) {
        TokioRuntime.block_on(task);
    }
    fn yield_now(&self) -> Task<'static, ()> {
        TokioRuntime.yield_now()
    }
}

#[derive(Debug)]
struct DirectSocket {
    local: SocketAddr,
    socket: Arc<dyn AsyncUdpSocket>,
    router: DirectUdp,
    send: mpsc::Sender<Bytes>,
    recv: Mutex<mpsc::Receiver<Bytes>>,
    prefer_socket: AtomicBool,
}

impl Drop for DirectSocket {
    fn drop(&mut self) {
        self.router
            .inputs
            .remove_if(&self.local, |_, tx| tx.same_channel(&self.send));
    }
}

impl AsyncUdpSocket for DirectSocket {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
    fn max_gso_segments(&self) -> usize {
        // A transmit may still need the relay socket, which must accept the same batch.
        self.router
            .public
            .max_gso_segments()
            .min(self.socket.max_gso_segments())
    }
    fn max_gro_segments(&self) -> usize {
        self.socket.max_gro_segments()
    }

    fn poll_send(&self, cx: &mut Context<'_>, transmit: &Transmit<'_>) -> Poll<io::Result<usize>> {
        if transmit.destination == self.router.relay
            && let Some(target) = self.router.target(self.local)
            && let Ok(len) = self.router.public.try_send_transmit(transmit, target)
        {
            return Poll::Ready(Ok(len));
        }
        self.socket.poll_send(cx, transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let capacity = bufs.len().min(meta.len());
        if capacity == 0 {
            return Poll::Ready(Ok(0));
        }
        if self.prefer_socket.swap(false, Ordering::Relaxed)
            && let Poll::Ready(result) = self.socket.poll_recv(cx, bufs, meta)
        {
            return Poll::Ready(result);
        }
        let mut rx = self
            .recv
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Poll::Ready(Some(mut packet)) = rx.poll_recv(cx) {
            for i in 0..capacity {
                let len = packet.len().min(bufs[i].len());
                bufs[i][..len].copy_from_slice(&packet[..len]);
                let mut info = RecvMeta::default();
                info.addr = self.router.relay;
                info.len = len;
                info.stride = len;
                info.dst_ip = Some(self.local.ip());
                meta[i] = info;
                if i + 1 < capacity
                    && let Ok(next) = rx.try_recv()
                {
                    packet = next;
                } else {
                    self.prefer_socket.store(true, Ordering::Relaxed);
                    return Poll::Ready(Ok(i + 1));
                }
            }
        }
        drop(rx);
        self.socket.poll_recv(cx, bufs, meta)
    }
}
