use crate::snapshot::{self, Settings, find_root};
use crate::store::{Action, Kind, Snapshot, Store, now_ms};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub struct Call {
    pub agent: String,
    pub tool: String,
    pub detail: String,
    pub session: Option<String>,
    pub start: PathBuf,
}

pub fn parse_claude(input: &str) -> Result<Call> {
    let value: Value = serde_json::from_str(input).context("hook input is not JSON")?;
    let cwd = PathBuf::from(value["cwd"].as_str().context("hook input has no cwd")?);
    let tool = value["tool_name"].as_str().unwrap_or_default().to_string();
    let input = &value["tool_input"];
    let file = input["file_path"]
        .as_str()
        .or_else(|| input["notebook_path"].as_str());
    let (start, detail) = match file {
        Some(file) if tool != "Bash" => (cwd.join(file), file.to_string()),
        _ => (
            cwd,
            input["command"].as_str().unwrap_or_default().to_string(),
        ),
    };
    Ok(Call {
        agent: "claude".into(),
        tool,
        detail,
        session: value["session_id"].as_str().map(String::from),
        start,
    })
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
