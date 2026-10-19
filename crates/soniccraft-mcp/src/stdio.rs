//! Process stdin for the server, read on a detached thread.
//!
//! `tokio::io::stdin()` reads on Tokio's blocking pool with a read that cannot be cancelled, so a runtime
//! shutting down after a signal (stdin still open) waits for that read forever. Reading on a plain,
//! detached OS thread instead keeps shutdown prompt: the thread simply dies with the process.

use std::io::Read as _;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;

const CHUNK_BYTES: usize = 64 * 1024;

/// An `AsyncRead` over the process's stdin.
pub(crate) struct Stdin {
    rx: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    pos: usize,
}

/// Start reading stdin on a detached thread; falls back to Tokio's stdin if no thread can be spawned.
pub(crate) fn stdin() -> Pin<Box<dyn AsyncRead + Send>> {
    let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(16);
    let spawned = std::thread::Builder::new()
        .name("soniccraft-mcp-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = vec![0u8; CHUNK_BYTES];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let _ = tx.blocking_send(Err(error));
                        break;
                    }
                }
            }
        });
    match spawned {
        Ok(_detached) => Box::pin(Stdin {
            rx,
            chunk: Vec::new(),
            pos: 0,
        }),
        Err(error) => {
            tracing::warn!(%error, "cannot start the stdin thread; using Tokio's stdin");
            Box::pin(tokio::io::stdin())
        }
    }
}

impl AsyncRead for Stdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if self.pos < self.chunk.len() {
                let n = out.remaining().min(self.chunk.len() - self.pos);
                let start = self.pos;
                out.put_slice(&self.chunk[start..start + n]);
                self.pos += n;
                return Poll::Ready(Ok(()));
            }
            match ready!(self.rx.poll_recv(cx)) {
                Some(Ok(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Some(Err(error)) => return Poll::Ready(Err(error)),
                // End of input: a read that fills nothing.
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    #[tokio::test]
    async fn chunks_are_reassembled_in_order() {
        let (tx, rx) = mpsc::channel(4);
        let mut reader = Stdin {
            rx,
            chunk: Vec::new(),
            pos: 0,
        };
        tokio::spawn(async move {
            for part in [&b"{\"a\":"[..], b"1}\n", b"{\"b\":2}\n"] {
                tx.send(Ok(part.to_vec())).await.unwrap();
            }
        });
        let mut all = String::new();
        reader.read_to_string(&mut all).await.unwrap();
        assert_eq!(all, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[tokio::test]
    async fn errors_are_surfaced() {
        let (tx, rx) = mpsc::channel(1);
        let mut reader = Stdin {
            rx,
            chunk: Vec::new(),
            pos: 0,
        };
        tx.send(Err(std::io::Error::other("boom"))).await.unwrap();
        let mut buf = [0u8; 8];
        assert!(reader.read(&mut buf).await.is_err());
    }
}
