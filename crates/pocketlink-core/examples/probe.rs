//! Print what the daemon reports: `cargo run -p pocketlink-core --example probe`

#[tokio::main]
async fn main() -> pocketlink_core::Result<()> {
    let client = pocketlink_core::Client::connect().await?;
    for device in client.devices().await? {
        println!("{device:#?}");
    }
    Ok(())
}
