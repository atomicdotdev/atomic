use std::io::{Read, Write};
use std::path::PathBuf;

use atomic_canonical::provenance_export::{verify_provenance_export, MAX_EXPORT_BYTES};
use atomic_identity::keypair::PublicKey;
use clap::Parser;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

#[derive(Parser, Debug)]
#[command(name = "verify-dsse")]
pub struct VerifyDsse {
    /// Envelope JSON file. Read stdin if omitted.
    pub file: Option<PathBuf>,

    /// Base32 Ed25519 exporter public key, pinned independently of this export.
    #[arg(long)]
    pub public_key: String,
}

impl Command for VerifyDsse {
    fn run(&self) -> CliResult<()> {
        let key =
            PublicKey::from_base32(&self.public_key).map_err(|e| CliError::InvalidArgument {
                message: e.to_string(),
            })?;
        let reader: Box<dyn Read> = match &self.file {
            Some(path) => Box::new(std::fs::File::open(path).map_err(CliError::Io)?),
            None => Box::new(std::io::stdin()),
        };
        let mut bytes = Vec::new();
        reader
            .take(MAX_EXPORT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(CliError::Io)?;
        let verified =
            verify_provenance_export(&bytes, &key).map_err(|e| CliError::InvalidArgument {
                message: e.to_string(),
            })?;
        // Deliberately write the authenticated bytes without reserialization,
        // or a second decode of the original envelope.
        std::io::stdout()
            .lock()
            .write_all(&verified.payload)
            .map_err(CliError::Io)
    }
}
