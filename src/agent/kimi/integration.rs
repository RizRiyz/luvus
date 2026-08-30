use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use super::super::types::IntegrationOperations;
use crate::integration;

pub(super) const OPERATIONS: IntegrationOperations = IntegrationOperations {
    install,
    uninstall,
    is_installed,
};

const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", Some("startup|resume")),
    ("Notification", None),
    ("Stop", None),
];

fn config_dir() -> PathBuf {
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| integration::home().join(".kimi-code"))
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub(in crate::agent) fn entry_is_luvus(table: &Table) -> bool {
    table
        .get("command")
        .and_then(|value| value.as_str())
        .map(|command| command.contains("luvus-agent-hook") || command.contains("bohay-agent-hook"))
        .unwrap_or(false)
}

fn strip_luvus(hooks: &mut ArrayOfTables) {
    let doomed: Vec<usize> = hooks
        .iter()
        .enumerate()
        .filter(|(_, table)| entry_is_luvus(table))
        .map(|(index, _)| index)
        .collect();
    for index in doomed.into_iter().rev() {
        hooks.remove(index);
    }
}

fn install() -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    let path = config_path();
    let mut document: DocumentMut = match fs::read_to_string(&path) {
        Ok(contents) => contents.parse()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DocumentMut::default(),
        Err(error) => return Err(error.into()),
    };
    let script = dir.join("luvus-agent-hook.sh");
    fs::write(&script, integration::agent_hook_script("kimi"))?;
    integration::set_executable(&script)?;
    let command = script.to_string_lossy().into_owned();
    let hooks = document
        .as_table_mut()
        .entry("hooks")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !hooks.is_array_of_tables() {
        *hooks = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let hooks = hooks.as_array_of_tables_mut().unwrap();
    strip_luvus(hooks);
    for (event, matcher) in HOOK_EVENTS {
        let mut table = Table::new();
        table["event"] = value(*event);
        if let Some(matcher) = matcher {
            table["matcher"] = value(*matcher);
        }
        table["command"] = value(command.clone());
        hooks.push(table);
    }
    fs::write(path, document.to_string())?;
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    Ok(())
}

fn uninstall() -> Result<()> {
    let dir = config_dir();
    let _ = fs::remove_file(dir.join("luvus-agent-hook.sh"));
    let _ = fs::remove_file(dir.join("bohay-agent-hook.sh"));
    let path = config_path();
    if let Ok(contents) = fs::read_to_string(&path) {
        if let Ok(mut document) = contents.parse::<DocumentMut>() {
            if let Some(hooks) = document
                .as_table_mut()
                .get_mut("hooks")
                .and_then(Item::as_array_of_tables_mut)
            {
                strip_luvus(hooks);
            }
            let _ = fs::write(path, document.to_string());
        }
    }
    Ok(())
}

fn is_installed() -> bool {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|contents| contents.parse::<DocumentMut>().ok())
        .and_then(|document| {
            document
                .get("hooks")
                .and_then(Item::as_array_of_tables)
                .map(|hooks| hooks.iter().any(entry_is_luvus))
        })
        .unwrap_or(false)
}
