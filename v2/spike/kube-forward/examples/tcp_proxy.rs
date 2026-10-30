//! Minimal TCP proxy to simulate network outages between the spike and the
//! API server: freeze it (SIGSTOP) to drop traffic silently like a dead VPN,
//! kill it to refuse connections.
//!
//! Usage: tcp_proxy <listen addr> <upstream addr>

use anyhow::{Context, Result};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(listen), Some(upstream)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: tcp_proxy <listen addr> <upstream addr>");
    };
    let listener = TcpListener::bind(&listen).await.with_context(|| format!("bind {listen}"))?;
    eprintln!("proxy {listen} -> {upstream}");
    loop {
        let (mut client, _) = listener.accept().await?;
        let upstream = upstream.clone();
        tokio::spawn(async move {
            if let Ok(mut server) = TcpStream::connect(&upstream).await {
                let _ = copy_bidirectional(&mut client, &mut server).await;
            }
        });
    }
}
