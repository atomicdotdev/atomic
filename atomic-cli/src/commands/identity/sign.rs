use clap::Parser;
use data_encoding::BASE64;
use std::io::Read;

use atomic_identity::IdentityStore;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

/// Sign bytes from stdin using an identity's Ed25519 key.
///
/// Reads raw bytes from stdin and outputs a JSON object with the
/// signature, public key, identity name, and algorithm.
///
/// # Examples
///
/// ```text
/// # Sign a file with the default identity
/// atomic identity sign < file.bin
///
/// # Sign with a specific identity
/// atomic identity sign --identity alice-work < file.bin
///
/// # Sign a string
/// echo -n "hello world" | atomic identity sign
/// ```
#[derive(Debug, Parser)]
pub struct Sign {
    /// Identity to sign with. Defaults to the current default identity.
    #[arg(short, long)]
    pub identity: Option<String>,
}

impl Command for Sign {
    fn run(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        let identity = if let Some(name) = &self.identity {
            store
                .load_by_name(name)
                .map_err(|_| CliError::IdentityNotFound(name.clone()))?
        } else {
            store
                .get_default()
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to load default identity: {}", e))
                })?
                .ok_or_else(|| {
                    CliError::Internal(anyhow::anyhow!(
                        "No default identity set. Create one first:\n  \
                         atomic identity new <name> --email <email> --set-default"
                    ))
                })?
        };

        let keypair = store.load_keypair(&identity.id, None).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to load keypair for '{}': {}",
                identity.name,
                e
            ))
        })?;

        let mut data = Vec::new();
        std::io::stdin()
            .read_to_end(&mut data)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to read stdin: {}", e)))?;

        let sig_bytes = keypair.sign(&data);
        let signature = BASE64.encode(&sig_bytes);
        let public_key = identity.public_key_base32();
        let name = &identity.name;

        let output = serde_json::json!({
            "signature": signature,
            "public_key": public_key,
            "identity": name,
            "alg": "ed25519",
        });

        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        Ok(())
    }
}
