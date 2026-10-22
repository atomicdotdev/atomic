//! `atomic kind list [<k>]` — the declared kinds, or one kind's instances.

use clap::Parser;
use serde_json::{Map, Value};

use atomic_canonical::kind_spec::{BaseType, KindSpec};
use atomic_repository::{
    check_instance, InstanceCheck, KindEntry, KindPath, KindRefResolver, KindRegistry, KindSource,
    KindStatus, Repository,
};

use super::{
    display_path, kind_not_found, open_repository, status_label, status_problems, Guidance,
};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{warning, Column, Table};

/// The most field columns `kind list <k>` shows, after UID and TITLE.
const MAX_FIELD_COLUMNS: usize = 4;
/// The widest a field column may grow.
const FIELD_COLUMN_WIDTH: usize = 24;
/// The widest the TITLE column may grow.
const TITLE_COLUMN_WIDTH: usize = 40;

/// List the declared kinds, or one kind's instances.
#[derive(Parser, Debug)]
#[command(name = "list")]
pub struct KindList {
    /// List this kind's instances instead of the kinds.
    pub kind: Option<String>,

    /// Also print each kind's guidance, as delimited repository data.
    #[arg(long)]
    pub guidance: bool,

    /// Output JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for KindList {
    fn run(&self) -> CliResult<()> {
        let repo = open_repository()?;
        let registry = repo
            .kind_registry(&KindSource::WorkingCopy)
            .map_err(CliError::Repository)?;
        match &self.kind {
            None => self.list_kinds(&registry),
            Some(kind) => {
                let entry = registry.get(kind).ok_or_else(|| kind_not_found(kind))?;
                self.list_instances(&repo, entry)
            }
        }
    }
}

impl KindList {
    fn list_kinds(&self, registry: &KindRegistry) -> CliResult<()> {
        if self.json {
            let kinds: Vec<Value> = registry
                .kinds()
                .map(|entry| {
                    let def = &entry.definition;
                    let mut row = serde_json::json!({
                        "name": def.name,
                        "status": status_label(def),
                        "def_path": display_path(&def.def_path),
                        "instances": entry.instances.len(),
                        "edges": entry.spec().map(KindSpec::edges).unwrap_or_default(),
                        "problems": status_problems(def),
                    });
                    if self.guidance {
                        row["guidance"] =
                            serde_json::to_value(Guidance::new(def)).expect("guidance serializes");
                    }
                    row
                })
                .collect();
            let out = serde_json::json!({
                "kinds": kinds,
                "unregistered": registry.unregistered(),
                "stray": registry.stray(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&out).expect("json serializes")
            );
            return Ok(());
        }

        if registry.is_empty() {
            println!("No kinds defined. Create one with `atomic kind init <name>`.");
            return Ok(());
        }

        if registry.kinds().next().is_some() {
            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("NAME"),
                Column::new("STATUS"),
                Column::new("INSTANCES"),
                Column::new("EDGES"),
            ]);
            for entry in registry.kinds() {
                let edges = entry.spec().map(KindSpec::edges).unwrap_or_default();
                table.add_row([
                    entry.name().to_string(),
                    status_label(&entry.definition).to_string(),
                    entry.instances.len().to_string(),
                    if edges.is_empty() {
                        "-".to_string()
                    } else {
                        edges.join(", ")
                    },
                ]);
            }
            println!("{table}");
        }

        if !registry.unregistered().is_empty() {
            println!();
            println!(
                "{}",
                warning("Instance directories with no _kind.md (unregistered):")
            );
            for (kind, paths) in registry.unregistered() {
                println!("  .vault/kinds/{kind}/  ({} files)", paths.len());
            }
        }
        if !registry.stray().is_empty() {
            println!();
            println!(
                "{}",
                warning(format!(
                    "{} stray file(s) under .vault/kinds/; see `atomic kind validate`",
                    registry.stray().len()
                ))
            );
        }

        if self.guidance {
            for entry in registry.kinds() {
                println!();
                print!("{}", Guidance::new(&entry.definition).render());
            }
        }
        Ok(())
    }

    fn list_instances(&self, repo: &Repository, entry: &KindEntry) -> CliResult<()> {
        let def = &entry.definition;
        let files = repo
            .kind_files(&KindSource::WorkingCopy)
            .map_err(CliError::Repository)?;
        let resolver = KindRefResolver::new(&files);
        let spec = match &def.status {
            KindStatus::Registered(spec) => Some(spec),
            KindStatus::Invalid(_) => None,
        };
        let columns = spec.map(display_fields).unwrap_or_default();

        let mut rows = Vec::new();
        for path in &entry.instances {
            let uid = match KindPath::classify(path) {
                Some(KindPath::Instance { uid, .. }) => uid,
                _ => continue,
            };
            let mut row = InstanceRow {
                uid: uid.clone(),
                path: display_path(path),
                title: None,
                conforms: false,
                fields: Map::new(),
            };
            if let (Some(spec), Some(bytes)) =
                (spec, files.read(path).map_err(CliError::Repository)?)
            {
                if let InstanceCheck::Checked {
                    frontmatter,
                    report,
                    ..
                } = check_instance(spec, &uid, &bytes, &resolver)
                {
                    row.conforms = report.conforms;
                    row.title = frontmatter
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    row.fields = frontmatter
                        .into_iter()
                        .filter(|(k, _)| spec.field(k).is_some())
                        .collect();
                }
            }
            rows.push(row);
        }

        if self.json {
            let mut out = serde_json::json!({
                "kind": def.name,
                "status": status_label(def),
                "problems": status_problems(def),
                "columns": columns,
                "instances": rows,
            });
            if self.guidance {
                out["guidance"] =
                    serde_json::to_value(Guidance::new(def)).expect("guidance serializes");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&out).expect("json serializes")
            );
            return Ok(());
        }

        for p in status_problems(def) {
            eprintln!("{} {}: {p}", warning("⚠"), display_path(&def.def_path));
        }
        if rows.is_empty() {
            println!(
                "No {} instances. Create one with `atomic kind new {}`.",
                def.name, def.name
            );
        } else {
            let mut table = Table::new();
            let mut cols = vec![
                Column::new("UID"),
                Column::new("TITLE").max_width(TITLE_COLUMN_WIDTH),
            ];
            cols.extend(
                columns
                    .iter()
                    .map(|c| Column::new(c.to_uppercase()).max_width(FIELD_COLUMN_WIDTH)),
            );
            table.set_columns(cols);
            for row in &rows {
                let mut cells = vec![
                    short_uid(&row.uid),
                    row.title.clone().unwrap_or_else(|| "-".to_string()),
                ];
                cells.extend(columns.iter().map(|c| cell(row.fields.get(c))));
                table.add_row(cells);
            }
            println!("{table}");
        }

        if self.guidance {
            println!();
            print!("{}", Guidance::new(def).render());
        }
        Ok(())
    }
}

#[derive(Debug, serde::Serialize)]
struct InstanceRow {
    uid: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    conforms: bool,
    fields: Map<String, Value>,
}

/// The single-valued fields worth a column: plain values first, then refs,
/// each in declaration (name) order, capped at [`MAX_FIELD_COLUMNS`].
fn display_fields(spec: &KindSpec) -> Vec<String> {
    let scalar = |want_ref: bool| {
        spec.fields
            .iter()
            .filter(move |(_, f)| {
                !f.field_type.list && (f.field_type.base == BaseType::Ref) == want_ref
            })
            .map(|(name, _)| name.clone())
    };
    scalar(false)
        .chain(scalar(true))
        .take(MAX_FIELD_COLUMNS)
        .collect()
}

/// The first 8 characters of a uid, marked as shortened.
fn short_uid(uid: &str) -> String {
    match uid.char_indices().nth(8) {
        Some((cut, _)) => format!("{}…", &uid[..cut]),
        None => uid.to_string(),
    }
}

fn cell(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_uid_truncates_to_eight() {
        assert_eq!(short_uid("01JD8ZK3M6W2Q4V7X9B5N1C3TR"), "01JD8ZK3…");
        assert_eq!(short_uid("ABC"), "ABC");
    }

    #[test]
    fn display_fields_puts_plain_scalars_before_refs() {
        let fm = serde_json::json!({
            "name": "adr",
            "specVersion": 1,
            "fields": {
                "intent": {"type": "ref", "target": ["intent"]},
                "status": {"type": "enum", "values": ["a"]},
                "tags": {"type": "string[]"},
                "a_count": {"type": "integer"},
                "b": {"type": "boolean"},
                "c": {"type": "string"}
            }
        });
        let spec = KindSpec::from_frontmatter("adr", fm.as_object().unwrap()).unwrap();
        assert_eq!(display_fields(&spec), ["a_count", "b", "c", "status"]);
    }
}
