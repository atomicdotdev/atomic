use clap::Parser;
use data_encoding::BASE64;
use std::io::Read;

use atomic_identity::keypair::PublicKey;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

/// Verify an Ed25519 signature against bytes from stdin.
///
/// Reads raw bytes from stdin and checks the signature. Exits 0 if
/// valid, 1 if invalid.
///
/// # Examples
///
/// ```text
/// # Verify a signature
/// atomic identity verify \
///   --signature <base64-sig> \
///   --public-key <base32-key> \
///   < file.bin
/// ```
#[derive(Debug, Parser)]
pub struct Verify {
    /// Base64-encoded signature to verify.
    #[arg(long)]
    pub signature: String,

    /// Base32-encoded public key to verify against.
    #[arg(long)]
    pub public_key: String,
}

impl Command for Verify {
    fn run(&self) -> CliResult<()> {
        let sig_bytes = BASE64.decode(self.signature.as_bytes()).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Invalid signature encoding: {}", e))
        })?;

        if sig_bytes.len() != 64 {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Invalid signature: expected 64 bytes, got {}",
                sig_bytes.len()
            )));
        }

        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);

        let public_key = PublicKey::from_base32(&self.public_key).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Invalid public key: {}", e))
        })?;

        let mut data = Vec::new();
        std::io::stdin().read_to_end(&mut data).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to read stdin: {}", e))
        })?;

        match public_key.verify(&data, &sig_arr) {
            Ok(()) => {
                eprintln!("Signature valid");
                std::process::exit(0);
            }
            Err(_) => {
                eprintln!("Signature invalid");
                std::process::exit(1);
            }
        }
    }
}
