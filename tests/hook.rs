use airbag::hook::{Call, parse_claude, parse_codex};
use airbag::install::{CLAUDE, CODEX, install, uninstall};
use airbag::store::{Kind, Store};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn run_hook(store: &Path, agent: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_airbag"))
        .args(["hook", agent])
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
    let command = CLAUDE.command("/opt/homebrew/bin/airbag");

    assert!(install(&CLAUDE, &path, &command).unwrap());
    assert!(!install(&CLAUDE, &path, &command).unwrap());

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
    assert_eq!(groups[1]["matcher"], CLAUDE.matcher);
    assert_eq!(
        groups[1]["hooks"][0]["command"],
        "/opt/homebrew/bin/airbag hook claude"
    );
    assert!(path.with_extension("json.airbag-backup").exists());

    assert!(uninstall(&CLAUDE, &path).unwrap());
    assert_eq!(read_json(&path), original);
    assert!(!uninstall(&CLAUDE, &path).unwrap());
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

    assert!(install(&CLAUDE, &link, "airbag hook claude").unwrap());
    assert!(install(&CLAUDE, &fresh, "airbag hook claude").unwrap());

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
        CLAUDE.matcher
    );
}

#[test]
fn hook_command_quotes_paths_with_spaces() {
    assert_eq!(
        CLAUDE.command("/Users/me/My Tools/airbag"),
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

    let output = run_hook(&store_dir, "claude", &input);

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

    let quiet = run_hook(&store_dir, "claude", &outside);
    let broken = run_hook(&store_dir, "claude", "{");

    assert!(quiet.status.success());
    assert_eq!(fs::read_dir(store_dir.join("projects")).unwrap().count(), 0);
    assert_eq!(broken.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&broken.stderr).contains("not protected"));
}

#[test]
fn codex_shell_calls_accept_a_string_or_an_argv_list() {
    let text = parse_codex(
        r#"{"session_id":"s2","turn_id":"t1","cwd":"/work/app","hook_event_name":"PreToolUse","model":"gpt-5","tool_name":"Bash","tool_use_id":"call_1","tool_input":{"command":"rm -rf build"}}"#,
    )
    .unwrap();
    let argv = parse_codex(
        r#"{"cwd":"/work/app","tool_name":"Bash","tool_input":{"command":["bash","-lc","git clean -fdx"],"workdir":"pkg"}}"#,
    )
    .unwrap();

    assert_eq!(
        (
            text.agent.as_str(),
            text.tool.as_str(),
            text.detail.as_str()
        ),
        ("codex", "Bash", "rm -rf build")
    );
    assert_eq!(text.start, PathBuf::from("/work/app"));
    assert_eq!(argv.detail, "bash -lc git clean -fdx");
    assert_eq!(argv.start, PathBuf::from("/work/app/pkg"));
}

#[test]
fn codex_patches_are_logged_by_the_files_they_touch() {
    let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** Add File: notes.md\n+hi\n*** Delete File: old.txt\n*** End Patch\n";
    let input = json!({
        "cwd": "/work/app",
        "tool_name": "apply_patch",
        "tool_input": { "command": patch }
    })
    .to_string();

    let call = parse_codex(&input).unwrap();

    assert_eq!(call.detail, "src/main.rs, notes.md, old.txt");
    assert_eq!(call.start, PathBuf::from("/work/app"));
}

#[test]
fn codex_install_writes_hooks_json_and_leaves_claude_hooks_alone() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".codex/hooks.json");
    let shared = temp.path().join("shared.json");

    assert!(install(&CODEX, &path, &CODEX.command("/usr/local/bin/airbag")).unwrap());
    assert!(install(&CLAUDE, &shared, "airbag hook claude").unwrap());
    assert!(install(&CODEX, &shared, "airbag hook codex").unwrap());
    assert!(uninstall(&CODEX, &shared).unwrap());

    assert_eq!(
        read_json(&path),
        json!({ "hooks": { "PreToolUse": [{
            "matcher": "^(Bash|apply_patch)$",
            "hooks": [{ "type": "command", "command": "/usr/local/bin/airbag hook codex", "timeout": 60 }]
        }] } })
    );
    let remaining = read_json(&shared);
    let groups = remaining["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "airbag hook claude");
}

#[test]
fn hook_files_live_where_each_agent_reads_them() {
    let root = Path::new("/work/app");

    assert_eq!(
        CLAUDE.settings_path(Some(root)).unwrap(),
        PathBuf::from("/work/app/.claude/settings.json")
    );
    assert_eq!(
        CODEX.settings_path(Some(root)).unwrap(),
        PathBuf::from("/work/app/.codex/hooks.json")
    );
}

#[test]
fn hook_binary_snapshots_before_a_codex_patch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("app");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let store_dir = temp.path().join("store");
    let input = json!({
        "session_id": "c1",
        "cwd": root,
        "tool_name": "apply_patch",
        "tool_input": { "command": "*** Begin Patch\n*** Delete File: main.rs\n*** End Patch\n" }
    })
    .to_string();

    let output = run_hook(&store_dir, "codex", &input);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshots = Store::at(store_dir)
        .unwrap()
        .project(&root)
        .unwrap()
        .snapshots()
        .unwrap();
    let action = &snapshots[0].actions[0];
    assert_eq!(
        (
            action.agent.as_str(),
            action.tool.as_str(),
            action.detail.as_str()
        ),
        ("codex", "apply_patch", "main.rs")
    );
}
