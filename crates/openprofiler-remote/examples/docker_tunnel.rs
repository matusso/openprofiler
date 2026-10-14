//! Manual end-to-end check: uploads a relay into a Docker container and round-trips bytes
//! through a [`Tunnel`] to a TCP echo server inside it.
//!
//! `cargo run -p openprofiler-remote --example docker_tunnel -- <container> <relay-binary> <port>`

use openprofiler_remote::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let args: Vec<String> = std::env::args().collect();
    let (container, relay_local, port) = (&args[1], &args[2], args[3].parse::<u16>()?);
    let target = Target::local().then(Hop::Docker { container: container.clone(), user: None });
    println!("platform: {:?}", target.platform().await?);
    let relay = "/tmp/openprofiler/openprofiler-relay".to_owned();
    let t0 = std::time::Instant::now();
    upload(&target, std::path::Path::new(relay_local), &relay, true).await?;
    println!("uploaded relay in {:.2?}", t0.elapsed());
    let tunnel = Tunnel::open(target.clone(), relay, port).await?;
    for i in 0..3 {
        let mut s = tokio::net::TcpStream::connect(tunnel.local_addr).await?;
        let msg = format!("ping {i}\n");
        s.write_all(msg.as_bytes()).await?;
        let mut buf = vec![0u8; msg.len()];
        s.read_exact(&mut buf).await?;
        assert_eq!(buf, msg.as_bytes());
        println!("round trip {i} ok");
        drop(s);
        // The test server (`nc -l` in a loop) needs a moment to listen again.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    Ok(())
}
