//! `remux relay`: the byte pump between ssh's stdio and this machine's
//! server socket. It never parses a frame and never logs one: what crosses it
//! is message bodies and screen contents, so the log gets byte counts only.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// How a pump ended: which side closed first, and how many bytes went each
/// way before it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpEnd {
    /// Bytes from ssh to the server.
    pub up: u64,
    /// Bytes from the server to ssh.
    pub down: u64,
    /// Whether ssh's side (stdin) ended first, else the server's.
    pub ssh_closed: bool,
}

impl PumpEnd {
    /// The relay's one line about the connection.
    pub fn summary(&self) -> String {
        format!(
            "relay: {} closed first; {} bytes up, {} bytes down",
            if self.ssh_closed { "ssh" } else { "the server" },
            self.up,
            self.down
        )
    }
}

/// Copy `ssh_in` to `server_out` and `server_in` to `ssh_out` until either
/// direction ends. The two directions are independent halves: one blocked
/// writing (its reader is busy writing back) never stops the other from
/// reading, which a single loop alternating between them would, and which
/// deadlocks once both directions are full at once.
pub async fn pump<A, B, C, D>(
    mut ssh_in: A,
    mut ssh_out: B,
    mut server_in: C,
    mut server_out: D,
) -> PumpEnd
where
    A: AsyncRead + Unpin,
    B: AsyncWrite + Unpin,
    C: AsyncRead + Unpin,
    D: AsyncWrite + Unpin,
{
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::AsyncReadExt;
    let (up, down) = (AtomicU64::new(0), AtomicU64::new(0));
    // Each half ends with whether it was ssh's side that closed.
    let upward = async {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match ssh_in.read(&mut buf).await {
                Ok(0) | Err(_) => return true,
                Ok(n) => {
                    if server_out.write_all(&buf[..n]).await.is_err() {
                        return false;
                    }
                    up.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    };
    let downward = async {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match server_in.read(&mut buf).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => {
                    if ssh_out.write_all(&buf[..n]).await.is_err() || ssh_out.flush().await.is_err()
                    {
                        return true;
                    }
                    down.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    };
    let ssh_closed = tokio::select! {
        closed = upward => closed,
        closed = downward => closed,
    };
    PumpEnd {
        up: up.load(Ordering::Relaxed),
        down: down.load(Ordering::Relaxed),
        ssh_closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions full at once: the server writes everything before it
    /// reads anything, while ssh's side reads and writes together. A pump
    /// that waits on its write to the server before reading the server
    /// again never finishes.
    #[tokio::test]
    async fn both_directions_full_at_once_do_not_deadlock() {
        use tokio::io::AsyncReadExt;
        const N: usize = 1024 * 1024;
        let (far, ssh_side) = tokio::io::duplex(256);
        let (server_side, server) = tokio::io::duplex(256);
        let (ssh_in, ssh_out) = tokio::io::split(ssh_side);
        let (server_in, server_out) = tokio::io::split(server_side);
        let pumping = tokio::spawn(pump(ssh_in, ssh_out, server_in, server_out));
        let (mut far_in, mut far_out) = tokio::io::split(far);
        let far_writes = tokio::spawn(async move {
            far_out.write_all(&vec![b'u'; N]).await.unwrap();
            far_out
        });
        let far_reads = tokio::spawn(async move {
            let mut got = vec![0u8; N];
            far_in.read_exact(&mut got).await.unwrap();
        });
        let server_turns = tokio::spawn(async move {
            let (mut server_in, mut server_out) = tokio::io::split(server);
            server_out.write_all(&vec![b'd'; N]).await.unwrap();
            let mut got = vec![0u8; N];
            server_in.read_exact(&mut got).await.unwrap();
            (server_in, server_out)
        });
        let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            far_reads.await.unwrap();
            let _ = server_turns.await.unwrap();
            let _ = far_writes.await.unwrap();
        })
        .await;
        assert!(
            finished.is_ok(),
            "the relay deadlocked with both directions full"
        );
        pumping.abort();
    }

    #[tokio::test]
    async fn the_relay_logs_counts_never_bytes() {
        let secret = b"RELAY_SECRET body that must never reach a log";
        let (mut far, ssh_side) = tokio::io::duplex(1024);
        let (server_side, mut server) = tokio::io::duplex(1024);
        let (ssh_in, ssh_out) = tokio::io::split(ssh_side);
        let (server_in, server_out) = tokio::io::split(server_side);
        let pumping = tokio::spawn(pump(ssh_in, ssh_out, server_in, server_out));
        far.write_all(secret).await.unwrap();
        let mut got = vec![0u8; secret.len()];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut got)
            .await
            .unwrap();
        assert_eq!(got, secret);
        drop(far);
        let end = pumping.await.unwrap();
        assert_eq!(end.up, secret.len() as u64);
        assert!(end.ssh_closed);
        let line = end.summary();
        assert!(line.contains(&secret.len().to_string()), "{line}");
        assert!(!line.contains("RELAY_SECRET"), "{line}");
    }
}
