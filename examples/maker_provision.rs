//! On the financial key's host: links and sets up Hyperliquid, creates a maker credential for the
//! quote key (saved to `credential.json`, read by `maker_quote`), then adds capital from the
//! HyperEVM wallet to the credential's Hyperliquid pool and waits until it is live.
//!
//! ```sh
//! LAMARGE_PRIVATE_KEY=0x… LAMARGE_OPERATOR=0x… LAMARGE_QUOTE_SIGNER=0x… LAMARGE_CAPITAL=1000 \
//!     cargo run --example maker_provision
//! ```
//!
//! `LAMARGE_OPERATOR` is the escrow operator from the deployment values you reviewed, never one
//! read from the API. Rerunning resumes: the saved credential is reused and the same
//! `client_request_id` resumes the same funding; a later top-up needs a new id.
#![expect(clippy::print_stdout, reason = "an example prints what it does")]

use std::{env, error::Error, fs, io::Write as _, time::Duration};

use alloy_signer_local::PrivateKeySigner;
use lamarge::{
    Address, Client, Environment, MakerCredential,
    api::CredentialRequest,
    flows::CapitalRequest,
    model::Funding,
    store::FileStore,
    types::{Amount, ClientId, VenueId},
};

const CREDENTIAL_FILE: &str = "credential.json";
const CREDENTIAL_LIFETIME: Duration = Duration::from_hours(30 * 24);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let key: PrivateKeySigner = env::var("LAMARGE_PRIVATE_KEY")?.parse()?;
    let operator: Address = env::var("LAMARGE_OPERATOR")?.parse()?;
    let quote_signer: Address = env::var("LAMARGE_QUOTE_SIGNER")?.parse()?;
    let venue = VenueId::Hyperliquid;
    let amount = Amount::parse_venue(&venue, &env::var("LAMARGE_CAPITAL")?)?;

    let client = Client::new(Environment::Production)?;
    let session = client.sign_in(key, FileStore::open("maker-state")?).await?;
    session.link_account(&venue).await?;
    session.set_up_venue(&venue, None).await?;

    let credential = match fs::read(CREDENTIAL_FILE) {
        Ok(saved) => serde_json::from_slice::<MakerCredential>(&saved)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let request = CredentialRequest::new("quoter-1", quote_signer, CREDENTIAL_LIFETIME);
            let credential = MakerCredential::from(session.credentials().create(&request).await?);
            save_secret(CREDENTIAL_FILE, &serde_json::to_vec(&credential)?)?;
            credential
        }
        Err(error) => return Err(error.into()),
    };
    if credential.identity.quote_signer != quote_signer {
        return Err(format!("{CREDENTIAL_FILE} names another quote signer").into());
    }

    let maker = session.maker(operator);
    let id = ClientId::new("pool-hyperliquid-1")?;
    let request = CapitalRequest::new(id, venue, amount, Funding::Wallet);
    let order = maker.add_capital(&request, &credential.identity).await?;
    let order = maker.wait_for_funding(&order, Duration::from_secs(600)).await?;
    println!("order {} live, {} available to quote", order.id, order.available_to_quote);
    session.sign_out().await?;
    Ok(())
}

/// Writes a new file readable only by its owner (on Unix).
fn save_secret(path: &str, contents: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}
