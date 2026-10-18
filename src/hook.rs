use crate::snapshot::{self, Settings, find_root};
use crate::store::{Action, Kind, Snapshot, Store, now_ms};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

const PATCH_FILE_PREFIXES: [&str; 3] = ["*** Add File: ", "*** Update File: ", "*** Delete File: "];

#[derive(Debug, PartialEq, Eq)]
pub struct Call {
    pub agent: String,
    pub tool: String,
    pub detail: String,
    pub session: Option<String>,
    pub start: PathBuf,
}

pub fn parse_claude(input: &str) -> Result<Call> {
    parse_tool_call("claude", "Bash", input)
}

pub fn parse_codex(input: &str) -> Result<Call> {
    parse_tool_call("codex", "Bash", input)
}

fn parse_tool_call(agent: &str, shell_tool: &str, input: &str) -> Result<Call> {
    let value: Value = serde_json::from_str(input).context("hook input is not JSON")?;
    let cwd = PathBuf::from(value["cwd"].as_str().context("hook input has no cwd")?);
    let tool = value["tool_name"].as_str().unwrap_or_default().to_string();
    let args = &value["tool_input"];
    let command = command_text(&args["command"]);
    let file = ["file_path", "notebook_path"]
        .iter()
        .find_map(|key| args[key].as_str());
    let (start, detail) = if tool == shell_tool {
        let workdir = ["workdir", "directory"]
            .iter()
            .find_map(|key| args[key].as_str());
        (
            workdir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir)),
            command,
        )
    } else if tool == "apply_patch" {
        (cwd, patch_files(&command))
    } else if let Some(file) = file {
        (cwd.join(file), file.to_string())
    } else {
        (cwd, command)
    };
    Ok(Call {
        agent: agent.into(),
        tool,
        detail,
        session: value["session_id"].as_str().map(String::from),
        start,
    })
}

fn command_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn patch_files(patch: &str) -> String {
    let files: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            PATCH_FILE_PREFIXES
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(str::trim)
        .collect();
    if files.is_empty() {
        patch.lines().next().unwrap_or_default().to_string()
    } else {
        files.join(", ")
    }
}

pub fn protect(store: &Store, settings: &Settings, call: Call) -> Result<Option<Snapshot>> {
    let Some(root) = find_root(&call.start) else {
        return Ok(None);
    };
    let project = store.project(&root)?;
    let action = Action {
        agent: call.agent,
        tool: call.tool,
        detail: call.detail,
        session: call.session,
        at_ms: now_ms(),
    };
    snapshot::take(store, &project, settings, Kind::Agent, Some(action)).map(Some)
}
