//! Observe typed clientbound packets only after the transport accepts the batch.
use crate::connections::Trace;
use bedrock::{
    network::tokio::{Connection, transport::Transport},
    protocol::V2225,
};

pub(in crate::protocol::bedrock) struct Observed<'a, T: Transport> {
    inner: &'a mut Connection<V2225, T>,
    trace: Trace,
}
impl<'a, T: Transport> Observed<'a, T> {
    pub fn new(inner: &'a mut Connection<V2225, T>, trace: Trace) -> Self {
        Self { inner, trace }
    }
    pub async fn send(&mut self, packets: &[V2225]) -> anyhow::Result<()> {
        self.inner.send(packets).await?;
        if self.trace.sent_enabled()
            && let Some((_, phase)) = self.trace.context()
        {
            for packet in packets {
                let info = super::super::catalog::receipt(packet, 1);
                self.trace
                    .sent(sculith_core::connections::ConnectionPacket {
                        protocol: Some(info.protocol),
                        phase,
                        id: info.id,
                        name: info.name,
                        dispatch: if info.dispatch == sculith_core::PacketDispatch::Unknown {
                            sculith_core::PacketDispatch::Unknown
                        } else if matches!(
                            phase,
                            sculith_core::connections::PacketPhase::Spawn
                                | sculith_core::connections::PacketPhase::Play
                        ) {
                            sculith_core::PacketDispatch::Notification
                        } else {
                            sculith_core::PacketDispatch::Control
                        },
                        sequence: 0,
                    });
            }
        }
        Ok(())
    }
}
impl<T: Transport> std::ops::Deref for Observed<'_, T> {
    type Target = Connection<V2225, T>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}
impl<T: Transport> std::ops::DerefMut for Observed<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bedrock::network::tokio::error::TransportLayerError;
    use sculith_core::connections::{ConnectionEvent, ConnectionTransport, PacketPhase};

    struct FailedTransport;
    impl Transport for FailedTransport {
        async fn send(&mut self, _: &[u8]) -> Result<(), TransportLayerError> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into())
        }
        async fn recv(&mut self) -> Result<Vec<u8>, TransportLayerError> {
            unreachable!()
        }
        fn remote_addr(&self) -> std::net::SocketAddr {
            "127.0.0.1:12345".parse().unwrap()
        }
        fn max_message_len(&self) -> usize {
            65536
        }
        async fn close(&self) {}
        async fn is_closed(&self) -> bool {
            true
        }
    }
    #[tokio::test]
    async fn failed_native_batch_emits_no_success_and_closes_once() {
        let observer = crate::connections::Observer::default();
        let mut events = observer.subscribe();
        observer.configure_sent(true);
        let trace = observer.open(ConnectionTransport::BedrockRaknet);
        trace.set_phase(PacketPhase::Login);
        let mut raw = Connection::from_transport_conn(FailedTransport);
        let mut observed = Observed::new(&mut raw, trace);
        assert!(
            observed
                .send(&[super::super::status(
                    bedrock::protocol::v662::enums::PlayStatus::LoginSuccess
                )])
                .await
                .is_err()
        );
        assert!(matches!(
            events.try_recv().unwrap(),
            ConnectionEvent::Opened { .. }
        ));
        assert!(events.try_recv().is_err());
        drop(observed);
        assert!(matches!(
            events.try_recv().unwrap(),
            ConnectionEvent::Closed { .. }
        ));
        assert!(events.try_recv().is_err());
    }
}
