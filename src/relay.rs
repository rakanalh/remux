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
/// direction ends.
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
    use tokio::io::AsyncReadExt;
    let (mut up, mut down) = (0u64, 0u64);
    let mut from_ssh = [0u8; 16 * 1024];
    let mut from_server = [0u8; 16 * 1024];
    let ssh_closed = loop {
        tokio::select! {
            got = ssh_in.read(&mut from_ssh) => match got {
                Ok(0) | Err(_) => break true,
                Ok(n) => {
                    if server_out.write_all(&from_ssh[..n]).await.is_err() {
                        break false;
                    }
                    up += n as u64;
                }
            },
            got = server_in.read(&mut from_server) => match got {
                Ok(0) | Err(_) => break false,
                Ok(n) => {
                    if ssh_out.write_all(&from_server[..n]).await.is_err()
                        || ssh_out.flush().await.is_err()
                    {
                        break true;
                    }
                    down += n as u64;
                }
            },
        }
    };
    PumpEnd {
        up,
        down,
        ssh_closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
