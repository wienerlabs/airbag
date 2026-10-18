use airbag::hook::{Call, parse_claude};
use airbag::install::{CLAUDE_MATCHER, hook_command, install_claude, uninstall_claude};
use airbag::store::{Kind, Store};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn run_hook(store: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_airbag"))
        .args(["hook", "claude"])
        .env("AIRBAG_HOME", store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn bash_calls_protect_the_working_directory() {
    let call = parse_claude(
        r#"{"session_id":"s1","cwd":"/work/app","tool_name":"Bash","tool_input":{"command":"rm -rf build","description":"clean"}}"#,
    )
    .unwrap();

    assert_eq!(
        call,
        Call {
            agent: "claude".into(),
            tool: "Bash".into(),
            detail: "rm -rf build".into(),
            session: Some("s1".into()),
            start: PathBuf::from("/work/app"),
        }
    );
}

#[test]
fn file_edits_protect_the_project_of_the_file() {
    let edit = parse_claude(
        r#"{"cwd":"/work/app","tool_name":"Edit","tool_input":{"file_path":"/other/repo/src/lib.rs","old_string":"a","new_string":"b"}}"#,
    )
    .unwrap();
    let notebook = parse_claude(
        r#"{"cwd":"/work/app","tool_name":"NotebookEdit","tool_input":{"notebook_path":"nb/a.ipynb"}}"#,
    )
    .unwrap();

    assert_eq!(edit.start, PathBuf::from("/other/repo/src/lib.rs"));
    assert_eq!(edit.detail, "/other/repo/src/lib.rs");
    assert_eq!(notebook.start, PathBuf::from("/work/app/nb/a.ipynb"));
}

#[test]
fn hook_input_without_a_cwd_is_rejected() {
    assert!(parse_claude(r#"{"tool_name":"Bash"}"#).is_err());
    assert!(parse_claude("not json").is_err());
}

#[test]
fn install_keeps_other_settings_and_runs_once() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".claude/settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = json!({
        "model": "opus",
        "permissions": { "allow": ["Bash(git status)"] },
        "hooks": { "PreToolUse": [
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": "lint-guard" }] }
        ] }
    });
    fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();
    let command = hook_command("/opt/homebrew/bin/airbag");

    assert!(install_claude(&path, &command).unwrap());
    assert!(!install_claude(&path, &command).unwrap());

    let installed = read_json(&path);
    let keys: Vec<&str> = installed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["model", "permissions", "hooks"]);
    let groups = installed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0], original["hooks"]["PreToolUse"][0]);
    assert_eq!(groups[1]["matcher"], CLAUDE_MATCHER);
    assert_eq!(
        groups[1]["hooks"][0]["command"],
        "/opt/homebrew/bin/airbag hook claude"
    );
    assert!(path.with_extension("json.airbag-backup").exists());

    assert!(uninstall_claude(&path).unwrap());
    assert_eq!(read_json(&path), original);
    assert!(!uninstall_claude(&path).unwrap());
}

#[test]
fn install_creates_missing_settings_and_writes_through_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("dotfiles/claude.json");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "{}").unwrap();
    let link = temp.path().join(".claude/settings.json");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&real, &link).unwrap();
    let fresh = temp.path().join("new/.claude/settings.json");

    assert!(install_claude(&link, "airbag hook claude").unwrap());
    assert!(install_claude(&fresh, "airbag hook claude").unwrap());

    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&real)
            .unwrap()
            .contains("airbag hook claude")
    );
    assert_eq!(
        read_json(&fresh)["hooks"]["PreToolUse"][0]["matcher"],
        CLAUDE_MATCHER
    );
}

#[test]
fn hook_command_quotes_paths_with_spaces() {
    assert_eq!(
        hook_command("/Users/me/My Tools/airbag"),
        "'/Users/me/My Tools/airbag' hook claude"
    );
}

#[test]
fn hook_binary_snapshots_before_a_bash_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("app");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let store_dir = temp.path().join("store");
    let input = json!({
        "session_id": "abc",
        "cwd": root,
        "tool_name": "Bash",
        "tool_input": { "command": "rm -rf ." }
    })
    .to_string();

    let output = run_hook(&store_dir, &input);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let snapshots = Store::at(store_dir)
        .unwrap()
        .project(&root)
        .unwrap()
        .snapshots()
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].kind, Kind::Agent);
    let action = &snapshots[0].actions[0];
    assert_eq!(
        (
            action.agent.as_str(),
            action.tool.as_str(),
            action.detail.as_str()
        ),
        ("claude", "Bash", "rm -rf .")
    );
    assert_eq!(action.session.as_deref(), Some("abc"));
}

#[test]
fn hook_binary_skips_folders_outside_git_and_flags_bad_input() {
    let temp = tempfile::tempdir().unwrap();
    let store_dir = temp.path().join("store");
    let outside = json!({
        "cwd": temp.path(),
        "tool_name": "Bash",
        "tool_input": { "command": "ls" }
    })
    .to_string();

    let quiet = run_hook(&store_dir, &outside);
    let broken = run_hook(&store_dir, "{");

    assert!(quiet.status.success());
    assert_eq!(fs::read_dir(store_dir.join("projects")).unwrap().count(), 0);
    assert_eq!(broken.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&broken.stderr).contains("not protected"));
}
