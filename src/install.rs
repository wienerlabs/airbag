use crate::store::{home_dir, unique_suffix};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const CLAUDE_MATCHER: &str = "Bash|Edit|Write|MultiEdit|NotebookEdit";

pub fn claude_settings(project_root: Option<&Path>) -> Result<PathBuf> {
    let base = match project_root {
        Some(root) => root.to_path_buf(),
        None => home_dir()?,
    };
    Ok(base.join(".claude").join("settings.json"))
}

pub fn hook_command(program: &str) -> String {
    format!("{} hook claude", shell_quote(program))
}

pub fn install_claude(settings_path: &Path, command: &str) -> Result<bool> {
    let path = real_path(settings_path);
    let mut settings = read_settings(&path)?;
    let groups = settings
        .as_object_mut()
        .context("settings.json is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("\"hooks\" in settings.json is not an object")?
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("\"hooks.PreToolUse\" in settings.json is not a list")?;
    if groups
        .iter()
        .any(|group| hooks_of(group).iter().any(is_airbag_hook))
    {
        return Ok(false);
    }
    groups.push(json!({
        "matcher": CLAUDE_MATCHER,
        "hooks": [{ "type": "command", "command": command, "timeout": 60 }]
    }));
    write_settings(&path, &settings)?;
    Ok(true)
}

pub fn uninstall_claude(settings_path: &Path) -> Result<bool> {
    let path = real_path(settings_path);
    if !path.exists() {
        return Ok(false);
    }
    let mut settings = read_settings(&path)?;
    let Some(groups) = settings
        .pointer_mut("/hooks/PreToolUse")
        .and_then(Value::as_array_mut)
    else {
        return Ok(false);
    };
    let mut removed = 0;
    for group in groups.iter_mut() {
        if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = hooks.len();
            hooks.retain(|hook| !is_airbag_hook(hook));
            removed += before - hooks.len();
        }
    }
    if removed == 0 {
        return Ok(false);
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|hooks| !hooks.is_empty())
    });
    write_settings(&path, &settings)?;
    Ok(true)
}

fn hooks_of(group: &Value) -> &[Value] {
    group["hooks"].as_array().map_or(&[], Vec::as_slice)
}

fn is_airbag_hook(hook: &Value) -> bool {
    hook["command"].as_str().is_some_and(|command| {
        command.contains("airbag") && command.trim_end().ends_with("hook claude")
    })
}

fn real_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn read_settings(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; fix it or add the hook by hand",
                path.display()
            )
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    let dir = path.parent().context("settings path has no folder")?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".settings.airbag-{}", unique_suffix()));
    fs::write(
        &temp,
        format!("{}\n", serde_json::to_string_pretty(settings)?),
    )?;
    if let Ok(meta) = fs::metadata(path) {
        let backup = path.with_extension("json.airbag-backup");
        if !backup.exists() {
            fs::copy(path, &backup)
                .with_context(|| format!("cannot back up {}", path.display()))?;
        }
        fs::set_permissions(&temp, meta.permissions())?;
    }
    fs::rename(&temp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

fn shell_quote(text: &str) -> String {
    let plain = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+@%:".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}
