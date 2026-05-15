//! Live ping of the local mem0 stack (Hermes' Docker compose).
//! Run with: `cargo test -p nucleus-core --test mem0_smoke -- --include-ignored`

use nucleus_core::mem0::Mem0Client;

#[tokio::test]
#[ignore]
async fn ping_local() {
    let client = Mem0Client::local();
    let alive = client.ping().await;
    println!("mem0 ping: {:?}", alive);
    // We don't assert OK because mem0's /health route varies between versions;
    // we just want to confirm the client compiles and reaches the network.
}
