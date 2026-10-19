//! One cycle of the soak test (java/bench/soak.sh): connects to the live agent of a JVM, receives
//! and acknowledges its data for a while, takes a thread dump and prints one line of counts.
//! Fails when the link does not connect or closes early.
//! Usage: cargo run -p openprofiler-link --example soak_client -- <pid> <seconds>

use openprofiler_link::{LinkClient, LinkState};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(pid), Some(secs)) = (args.get(1), args.get(2)) else {
        anyhow::bail!("usage: soak_client <pid> <seconds>");
    };
    let client = LinkClient::connect_pid(pid.parse()?).await?;
    let mut state = client.state();
    let mut generation = client.generation();
    let wait = tokio::time::sleep(Duration::from_secs(secs.parse()?));
    tokio::pin!(wait);
    loop {
        tokio::select! {
            _ = &mut wait => break,
            r = state.changed() => {
                r?;
                if let LinkState::Closed(why) = &*state.borrow() {
                    anyhow::bail!("link closed: {why}");
                }
            }
            r = generation.changed() => r?,
        }
    }
    let dump = client.thread_dump().await?;
    let batches = *generation.borrow();
    let p = client.profile();
    let line = {
        let p = p.read();
        let samples: u64 = p.samples.values().map(|s| s.count.iter().map(|&c| c as u64).sum::<u64>()).sum();
        format!(
            "batches={batches} samples={samples} threads={} stacks={} probe_calls={} dump_threads={}",
            p.threads.len(),
            p.stacks.len(),
            p.probes.calls.len(),
            dump.threads.len()
        )
    };
    client.close().await;
    println!("{line}");
    Ok(())
}
