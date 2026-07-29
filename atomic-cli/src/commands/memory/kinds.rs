//! `atomic memory kinds` — list the allowed memory kinds and their guidance.
//!
//! This surfaces the closed `memoryKind` vocabulary (the single source of
//! truth in `atomic_canonical::vocab::MEMORY_KIND_GUIDANCE`) so a model — or a
//! human — can read a session ledger and classify each durable insight into the
//! right kind, emitting one `atomic memory new --kind <kind>` per memory.
//!
//! # Examples
//!
//! ```text
//! atomic memory kinds
//! atomic memory kinds --json
//! ```

use clap::Parser;

use atomic_canonical::vocab::MEMORY_KIND_GUIDANCE;

use crate::commands::Command;
use crate::error::CliResult;

/// List the allowed memory kinds and when to use each.
#[derive(Parser, Debug)]
#[command(name = "kinds")]
pub struct MemoryKinds {
    /// Emit the kinds as a JSON array of `{"kind","description"}` objects.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryKinds {
    fn run(&self) -> CliResult<()> {
        if self.json {
            let arr: Vec<serde_json::Value> = MEMORY_KIND_GUIDANCE
                .iter()
                .map(|(kind, description)| {
                    serde_json::json!({ "kind": kind, "description": description })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&arr).unwrap());
            return Ok(());
        }

        let width = MEMORY_KIND_GUIDANCE
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(0);
        println!(
            "Allowed memory kinds (pick the best fit; create one memory per durable insight):"
        );
        println!();
        for (kind, description) in MEMORY_KIND_GUIDANCE {
            println!("  {:<width$}  {}", kind, description, width = width);
        }
        Ok(())
    }
}
