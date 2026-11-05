//! `atomic kind show <k>` — a kind's schema and status.

use clap::Parser;
use serde_json::Value;

use atomic_canonical::kind_spec::{FieldSpec, KindSpec};
use atomic_repository::{KindEntry, KindSource, KindStatus};

use super::{
    display_path, display_text, kind_not_found, open_for_read, status_label, status_problems,
    Guidance,
};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, error, Column, KeyValueTable, Table};

/// Show a kind's schema and status.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct KindShow {
    /// The kind to show.
    pub kind: String,

    /// Also print the definition's guidance, as delimited repository data.
    #[arg(long)]
    pub guidance: bool,

    /// Output JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for KindShow {
    fn run(&self) -> CliResult<()> {
        let repo = open_for_read()?;
        let registry = repo
            .kind_registry(&KindSource::WorkingCopy)
            .map_err(CliError::Repository)?;
        let entry = registry
            .get(&self.kind)
            .ok_or_else(|| kind_not_found(&self.kind))?;
        if self.json {
            let kg_id = repo
                .kind_hub_iri(entry.name())
                .map_err(CliError::Repository)?;
            self.print_json(entry, kg_id.as_deref());
        } else {
            self.print_human(entry);
        }
        Ok(())
    }
}

impl KindShow {
    fn print_json(&self, entry: &KindEntry, kg_id: Option<&str>) {
        let def = &entry.definition;
        let spec = entry.spec();
        let mut out = serde_json::json!({
            "kind": def.name,
            "kg_id": kg_id,
            "status": status_label(def),
            "problems": status_problems(def),
            "def_path": display_path(&def.def_path),
            "instance_count": entry.instances.len(),
            "spec": spec,
            "edges": spec.map(KindSpec::edges).unwrap_or_default(),
        });
        if self.guidance {
            out["guidance"] =
                serde_json::to_value(Guidance::new(def)).expect("guidance serializes");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&out).expect("json serializes")
        );
    }

    fn print_human(&self, entry: &KindEntry) {
        let def = &entry.definition;
        let mut summary = KeyValueTable::new()
            .add("Kind", def.name.clone())
            .add("Status", status_label(def));
        if let Some(title) = entry.spec().and_then(|s| s.title.as_deref()) {
            summary = summary.add("Title", display_text(title));
        }
        summary = summary
            .add("Definition", display_path(&def.def_path))
            .add("Instances", entry.instances.len().to_string());
        if let Some(spec) = entry.spec() {
            let yes_no = |b: bool| if b { "yes" } else { "no" };
            summary = summary
                .add("Body required", yes_no(spec.body.required))
                .add("Embed", yes_no(spec.embed))
                .add("Context", yes_no(spec.context));
            let edges = spec.edges();
            if !edges.is_empty() {
                summary = summary.add("Edges", edges.join(", "));
            }
        }
        println!("{summary}");

        match &def.status {
            KindStatus::Invalid(problems) => {
                println!();
                println!("{}", emphasis("Problems (the kind is not registered)"));
                for p in problems {
                    println!("  {} {}", error("✗"), display_text(p));
                }
            }
            KindStatus::Registered(spec) => {
                println!();
                println!("{}", emphasis("Fields"));
                if spec.fields.is_empty() {
                    println!("  (none: instances carry only uid and title)");
                } else {
                    println!("{}", fields_table(spec));
                }
            }
        }

        if self.guidance {
            println!();
            print!("{}", Guidance::new(def).render());
        }
    }
}

fn fields_table(spec: &KindSpec) -> Table {
    let mut table = Table::new();
    table.set_columns(vec![
        Column::new("NAME"),
        Column::new("TYPE"),
        Column::new("REQUIRED"),
        Column::new("DEFAULT").max_width(24),
        Column::new("VALUES/TARGET").max_width(40),
        Column::new("EDGE"),
        Column::new("DESCRIPTION").max_width(60),
    ]);
    for (name, field) in &spec.fields {
        table.add_row([
            name.clone(),
            field.field_type.to_string(),
            if field.required { "yes" } else { "no" }.to_string(),
            field
                .default
                .as_ref()
                .map_or_else(|| "-".to_string(), |v| display_text(&compact(v))),
            display_text(&values_or_target(field)),
            field
                .edge
                .as_deref()
                .map_or_else(|| "-".to_string(), str::to_string),
            field
                .description
                .as_deref()
                .map_or_else(|| "-".to_string(), display_text),
        ]);
    }
    table
}

fn values_or_target(field: &FieldSpec) -> String {
    let list = if field.values.is_empty() {
        &field.target
    } else {
        &field.values
    };
    if list.is_empty() {
        "-".to_string()
    } else {
        list.join(", ")
    }
}

fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
