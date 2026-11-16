//! Java's continuous AES/CFB8 stream, including frame prefixes.
use aes::Aes128;
use cfb8::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

type Encryptor = cfb8::Encryptor<Aes128>;
type Decryptor = cfb8::Decryptor<Aes128>;

fn encrypt(cipher: &mut Encryptor, bytes: &mut [u8]) {
    for byte in bytes {
        let mut block = [*byte].into();
        cipher.encrypt_block_mut(&mut block);
        *byte = block[0];
    }
}
fn decrypt(cipher: &mut Decryptor, bytes: &mut [u8]) {
    for byte in bytes {
        let mut block = [*byte].into();
        cipher.decrypt_block_mut(&mut block);
        *byte = block[0];
    }
}

pub struct Stream<S> {
    inner: S,
    encrypt: Option<Encryptor>,
    decrypt: Option<Decryptor>,
}

impl<S> Stream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            encrypt: None,
            decrypt: None,
        }
    }
    pub fn enable_encryption(&mut self, secret: &[u8; 16]) -> anyhow::Result<()> {
        anyhow::ensure!(self.encrypt.is_none(), "Java encryption already enabled");
        self.encrypt = Some(Encryptor::new(secret.into(), secret.into()));
        self.decrypt = Some(Decryptor::new(secret.into(), secret.into()));
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if let Some(cipher) = &mut this.decrypt {
                    decrypt(cipher, &mut buf.filled_mut()[before..]);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(current) = &mut this.encrypt else {
            return Pin::new(&mut this.inner).poll_write(cx, bytes);
        };
        let mut candidate = current.clone();
        let mut encrypted = bytes.to_vec();
        encrypt(&mut candidate, &mut encrypted);
        match Pin::new(&mut this.inner).poll_write(cx, &encrypted) {
            Poll::Ready(Ok(written)) => {
                // Commit only accepted bytes. Pending/partial writes and cancelled
                // futures must never consume keystream for bytes not sent.
                if written == bytes.len() {
                    *current = candidate;
                } else {
                    encrypted[..written].copy_from_slice(&bytes[..written]);
                    encrypt(current, &mut encrypted[..written]);
                }
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn cfb8_matches_nist_sp800_38a_across_incremental_calls() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let plain = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let expected = [
            0x3b, 0x79, 0x42, 0x4c, 0x9c, 0x0d, 0xd4, 0x36, 0xba, 0xce, 0x9e, 0x0e, 0xd4, 0x58,
            0x6a, 0x4f,
        ];
        let mut bytes = plain;
        let mut cipher = Encryptor::new(&key.into(), &iv.into());
        for chunk in bytes.chunks_mut(3) {
            encrypt(&mut cipher, chunk);
        }
        assert_eq!(bytes, expected);
        let mut cipher = Decryptor::new(&key.into(), &iv.into());
        for chunk in bytes.chunks_mut(5) {
            decrypt(&mut cipher, chunk);
        }
        assert_eq!(bytes, plain);
    }

    struct LimitedWriter {
        bytes: Vec<u8>,
        allowed: usize,
    }
    impl AsyncWrite for LimitedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if this.allowed == 0 {
                return Poll::Pending;
            }
            let count = bytes.len().min(this.allowed).min(3);
            this.bytes.extend_from_slice(&bytes[..count]);
            this.allowed -= count;
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn cancelled_partial_write_only_advances_cipher_for_accepted_bytes() {
        let plain = b"framed ciphertext continues after partial writes";
        let secret = [17; 16];
        let mut stream = Stream::new(LimitedWriter {
            bytes: Vec::new(),
            allowed: 3,
        });
        stream.enable_encryption(&secret).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), stream.write_all(plain))
                .await
                .is_err()
        );
        assert_eq!(stream.inner.bytes.len(), 3);
        stream.inner.allowed = plain.len();
        stream.write_all(&plain[3..]).await.unwrap();
        let mut expected = plain.to_vec();
        encrypt(
            &mut Encryptor::new(&secret.into(), &secret.into()),
            &mut expected,
        );
        assert_eq!(stream.inner.bytes, expected);
        assert!(stream.enable_encryption(&secret).is_err());
    }

    #[tokio::test]
    async fn encrypted_partial_frame_survives_cancelled_reads() {
        let (a, b) = tokio::io::duplex(64);
        let mut sender = Stream::new(a);
        let mut receiver = Stream::new(b);
        sender.enable_encryption(&[42; 16]).unwrap();
        receiver.enable_encryption(&[42; 16]).unwrap();
        let mut frames = super::super::codec::Frames::default();
        sender.write_all(&[4, 0]).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(5),
                frames.next(&mut receiver)
            )
            .await
            .is_err()
        );
        sender.write_all(b"xyz").await.unwrap();
        assert_eq!(frames.next(&mut receiver).await.unwrap(), b"\0xyz");
    }
}
