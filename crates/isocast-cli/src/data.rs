//! `isocast data …`: the data store (docs/scenes-as-code.md, 5.5), values
//! by key that text in scenes shows (`data = "key"`). Each command is one
//! `data_*` call on the method table.

use std::process::ExitCode;

use anyhow::Result;
use clap::Subcommand;
use isocast_protocol::api::Method;
use isocast_protocol::data::{
    DataDeleteParams, DataGetParams, DataRevision, DataSetParams, DataValues,
};
use serde_json::Value;

use crate::client::Client;

#[derive(Subcommand)]
pub enum DataCommand {
    /// Show data keys and their values: every key, the KEYs given, or those
    /// under --prefix.
    Get {
        keys: Vec<String>,
        /// Only keys under this prefix: `score` shows `score` and
        /// `score.home`, not `scoreboard`.
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set KEY to VALUE: JSON (`'"Hello"'`, `3`, `'{"home": 2}'`), or else
    /// the text as it is (`Hello`). Text showing the key (`data = "KEY"`)
    /// changes at once. Keys are dotted ids like `score.home`.
    Set {
        key: String,
        value: String,
        /// Keep it with the scene collection, so it comes back after a
        /// restart (it stays kept when set again without this).
        #[arg(long, conflicts_with = "no_persist")]
        persist: bool,
        /// Stop keeping it: it lasts until isocastd stops.
        #[arg(long)]
        no_persist: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete data keys.
    Delete {
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long)]
        json: bool,
    },
}

/// VALUE as `data set` takes it: JSON, or else the text as it is.
pub fn value_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

pub fn run(command: DataCommand) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    match command {
        DataCommand::Get { keys, prefix, json } => {
            let values: DataValues =
                client.call(Method::DataGet(Some(DataGetParams { keys, prefix })))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&values)?);
            } else {
                print_values(&values);
            }
        }
        DataCommand::Set {
            key,
            value,
            persist,
            no_persist,
            json,
        } => {
            let persist = match (persist, no_persist) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let set: DataRevision = client.call(Method::DataSet(Box::new(DataSetParams {
                values: [(key.clone(), value_of(&value))].into(),
                persist,
            })))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&set)?);
            } else if set.changed.is_empty() {
                println!("{key} already had that value (revision {})", set.revision);
            } else {
                println!("set {key} (revision {})", set.revision);
            }
        }
        DataCommand::Delete { keys, json } => {
            let deleted: DataRevision =
                client.call(Method::DataDelete(DataDeleteParams { keys: keys.clone() }))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&deleted)?);
            } else {
                if !deleted.changed.is_empty() {
                    println!(
                        "deleted {} (revision {})",
                        deleted.changed.join(", "),
                        deleted.revision
                    );
                }
                let missing: Vec<&String> = keys
                    .iter()
                    .filter(|key| !deleted.changed.contains(key))
                    .collect();
                for key in missing {
                    println!("{key} wasn't set");
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_values(values: &DataValues) {
    if values.values.is_empty() && values.missing.is_empty() {
        println!("no data keys set (revision {})", values.revision);
        return;
    }
    for (key, value) in &values.values {
        let kept = if values.persisted.contains(key) {
            "  (kept)"
        } else {
            ""
        };
        println!("{key} = {value}{kept}");
    }
    for key in &values.missing {
        println!("{key}: not set");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_json_or_text() {
        assert_eq!(value_of("3"), Value::from(3));
        assert_eq!(value_of(r#""Hello""#), Value::from("Hello"));
        assert_eq!(value_of("Hello there"), Value::from("Hello there"));
        assert_eq!(value_of(r#"{"home": 2}"#), serde_json::json!({"home": 2}));
        assert_eq!(value_of("true"), Value::from(true));
    }
}
