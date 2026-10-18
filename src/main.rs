use airbag::restore::{self, Change, Restored, Status};
use airbag::snapshot::{self, Settings};
use airbag::store::{Action, Kind, Project, Snapshot, Store, now_ms};
use airbag::{gc, hook, install};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Read};
use std::path::{Component, Path, PathBuf};

const DIFF_LINES: usize = 200;

#[derive(Parser)]
#[command(
    name = "airbag",
    version,
    about = "Undo anything your AI coding agent did to your disk, bash commands included."
)]
struct Cli {
    #[arg(
        short = 'C',
        long,
        global = true,
        default_value = ".",
        help = "Run as if airbag was started in this folder"
    )]
    dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Take a snapshot of the project right now")]
    Snap {
        #[arg(short, long, help = "Note to show next to the snapshot in the log")]
        message: Option<String>,
    },
    #[command(about = "List snapshots, newest first, with what changed after each one")]
    Log {
        #[arg(short = 'n', long, default_value_t = 20, help = "How many to show")]
        limit: usize,
    },
    #[command(
        about = "Show what changed since a snapshot; by default, what `airbag undo` would revert"
    )]
    Diff {
        #[arg(help = "Snapshot number from `airbag log`")]
        id: Option<u64>,
        #[arg(long, help = "Only look at this file or folder")]
        path: Option<PathBuf>,
    },
    #[command(
        about = "Put the project back to how it was before the last change; `airbag redo` reverts it"
    )]
    Undo {
        #[arg(help = "Snapshot number to restore instead of the one before the last change")]
        id: Option<u64>,
        #[arg(long, help = "Only restore this file or folder")]
        path: Option<PathBuf>,
    },
    #[command(about = "Revert the last undo")]
    Redo,
    #[command(
        about = "Make an AI coding agent snapshot your git projects before every risky tool call"
    )]
    Install {
        agent: Agent,
        #[arg(
            long,
            help = "Write to this project's settings instead of your user settings"
        )]
        project: bool,
    },
    #[command(about = "Remove the airbag hook from an agent")]
    Uninstall {
        agent: Agent,
        #[arg(
            long,
            help = "Remove it from this project's settings instead of your user settings"
        )]
        project: bool,
    },
    #[command(about = "Delete stored copies that no snapshot needs any more")]
    Gc,
    #[command(hide = true)]
    Hook { agent: Agent },
}

#[derive(Clone, Copy, ValueEnum)]
enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn hookup(self) -> &'static install::Hookup {
        match self {
            Agent::Claude => &install::CLAUDE,
            Agent::Codex => &install::CODEX,
        }
    }

    fn installed_note(self) -> &'static str {
        match self {
            Agent::Claude => {
                "Claude Code now takes a snapshot before every Bash, Edit and Write call in a git project; restart open sessions so they load it"
            }
            Agent::Codex => {
                "Codex now takes a snapshot before every shell command and apply_patch in a git project; open /hooks in Codex once and trust the airbag hook"
            }
        }
    }
}

struct Ctx {
    store: Store,
    settings: Settings,
    dir: PathBuf,
    paint: Paint,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("airbag: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let ctx = Ctx {
        store: Store::open()?,
        settings: Settings::from_env(),
        dir: cli.dir,
        paint: Paint::detect(),
    };
    match cli.command {
        Command::Snap { message } => snap(&ctx, message),
        Command::Log { limit } => log(&ctx, limit),
        Command::Diff { id, path } => show_diff(&ctx, id, path),
        Command::Undo { id, path } => undo(&ctx, id, path),
        Command::Redo => redo(&ctx),
        Command::Gc => collect_garbage(&ctx),
        Command::Install { agent, project } => install_hook(&ctx, agent, project),
        Command::Uninstall { agent, project } => uninstall_hook(&ctx, agent, project),
        Command::Hook { agent } => run_hook(&ctx, agent)
            .context("could not take a snapshot, so this tool call is not protected"),
    }
}

impl Ctx {
    fn project(&self) -> Result<Project> {
        let root = snapshot::find_root(&self.dir).with_context(|| {
            format!(
                "{} is not inside a git work tree; airbag protects git projects",
                self.dir.display()
            )
        })?;
        self.store.project(&root)
    }

    fn scope(&self, project: &Project, path: Option<PathBuf>) -> Result<Option<String>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let full = resolve(&self.dir.join(&path))?;
        let rel = full
            .strip_prefix(&project.root)
            .ok()
            .and_then(Path::to_str)
            .with_context(|| {
                format!(
                    "{} is not inside {}",
                    path.display(),
                    project.root.display()
                )
            })?;
        Ok((!rel.is_empty()).then(|| rel.to_string()))
    }

    fn current(&self, project: &Project) -> Result<String> {
        restore::current_tree(&self.store, project, &self.settings)
    }
}

fn snap(ctx: &Ctx, message: Option<String>) -> Result<()> {
    let project = ctx.project()?;
    let action = Action {
        agent: "you".into(),
        tool: "snap".into(),
        detail: message.unwrap_or_default(),
        session: None,
        at_ms: now_ms(),
    };
    let snapshot = snapshot::take(
        &ctx.store,
        &project,
        &ctx.settings,
        Kind::Manual,
        Some(action),
    )?;
    println!(
        "saved #{}: {}, {}",
        snapshot.id,
        plural(snapshot.files, "file"),
        human_bytes(snapshot.bytes)
    );
    for note in &snapshot.skipped {
        println!("  not protected: {note}");
    }
    Ok(())
}

fn log(ctx: &Ctx, limit: usize) -> Result<()> {
    let project = ctx.project()?;
    let snapshots = project.snapshots()?;
    if snapshots.is_empty() {
        println!("no snapshots yet for {}", project.root.display());
        return Ok(());
    }
    let mut after = ctx.current(&project)?;
    for snapshot in snapshots.iter().rev().take(limit) {
        let changes = restore::diff(&ctx.store, &snapshot.tree, &after, None)?;
        println!("{}", log_line(snapshot, &changes, &ctx.paint));
        after = snapshot.tree.clone();
    }
    Ok(())
}

fn show_diff(ctx: &Ctx, id: Option<u64>, path: Option<PathBuf>) -> Result<()> {
    let project = ctx.project()?;
    let scope = ctx.scope(&project, path)?;
    let current = ctx.current(&project)?;
    let target = match id {
        Some(id) => project.load(id)?,
        None => match restore::undo_target(&ctx.store, &project, &current, scope.as_deref())? {
            Some(target) => target,
            None => {
                println!("nothing changed since the last snapshot");
                return Ok(());
            }
        },
    };
    let changes = restore::diff(&ctx.store, &target.tree, &current, scope.as_deref())?;
    println!("since #{} ({}):", target.id, describe(&target));
    print_changes(&changes, &ctx.paint);
    if id.is_none() {
        println!("{}", ctx.paint.dim("run `airbag undo` to revert this"));
    }
    Ok(())
}

fn undo(ctx: &Ctx, id: Option<u64>, path: Option<PathBuf>) -> Result<()> {
    let project = ctx.project()?;
    let scope = ctx.scope(&project, path)?;
    let current = ctx.current(&project)?;
    let target = match id {
        Some(id) => project.load(id)?,
        None => restore::undo_target(&ctx.store, &project, &current, scope.as_deref())?
            .context("nothing to undo: the project matches its latest snapshot")?,
    };
    if restore::diff(&ctx.store, &target.tree, &current, scope.as_deref())?.is_empty() {
        println!("already matches #{}", target.id);
        return Ok(());
    }
    let restored = restore::restore(
        &ctx.store,
        &project,
        &ctx.settings,
        &target,
        scope.as_deref(),
    )?;
    print_restored(&target, &restored, &ctx.paint);
    println!("{}", ctx.paint.dim("changed your mind? run `airbag redo`"));
    Ok(())
}

fn redo(ctx: &Ctx) -> Result<()> {
    let project = ctx.project()?;
    let target =
        restore::redo_target(&project)?.context("nothing to redo: no undo has run here yet")?;
    let restored = restore::restore(&ctx.store, &project, &ctx.settings, &target, None)?;
    print_restored(&target, &restored, &ctx.paint);
    Ok(())
}

fn collect_garbage(ctx: &Ctx) -> Result<()> {
    let collected = gc::collect(&ctx.store)?;
    let noun = if collected.removed == 1 {
        "copy"
    } else {
        "copies"
    };
    println!(
        "removed {} stored {noun} no snapshot needs ({} at most), kept {}",
        collected.removed,
        human_bytes(collected.freed_bytes),
        collected.kept
    );
    println!(
        "{}",
        ctx.paint.dim(
            "copies made in the last hour are always kept, so snapshots in progress stay whole"
        )
    );
    Ok(())
}

fn install_hook(ctx: &Ctx, agent: Agent, project: bool) -> Result<()> {
    let hookup = agent.hookup();
    let settings_path = settings_path(ctx, hookup, project)?;
    let command = hookup.command(&airbag_program());
    if install::install(hookup, &settings_path, &command)? {
        println!("added the airbag hook to {}", settings_path.display());
        println!("{}", ctx.paint.dim(agent.installed_note()));
    } else {
        println!("the airbag hook is already in {}", settings_path.display());
    }
    Ok(())
}

fn uninstall_hook(ctx: &Ctx, agent: Agent, project: bool) -> Result<()> {
    let hookup = agent.hookup();
    let settings_path = settings_path(ctx, hookup, project)?;
    if install::uninstall(hookup, &settings_path)? {
        println!("removed the airbag hook from {}", settings_path.display());
    } else {
        println!("no airbag hook found in {}", settings_path.display());
    }
    Ok(())
}

fn settings_path(ctx: &Ctx, hookup: &install::Hookup, project: bool) -> Result<PathBuf> {
    let root = if project {
        Some(ctx.project()?.root)
    } else {
        None
    };
    hookup.settings_path(root.as_deref())
}

fn run_hook(ctx: &Ctx, agent: Agent) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let call = match agent {
        Agent::Claude => hook::parse_claude(&input)?,
        Agent::Codex => hook::parse_codex(&input)?,
    };
    hook::protect(&ctx.store, &ctx.settings, call)?;
    Ok(())
}

fn airbag_program() -> String {
    std::env::current_exe().map_or_else(|_| "airbag".to_string(), |path| path.display().to_string())
}

fn print_restored(target: &Snapshot, restored: &Restored, paint: &Paint) {
    println!("restored #{} ({})", target.id, describe(target));
    println!(
        "  {} written, {} removed",
        plural(restored.report.written, "file"),
        restored.report.removed
    );
    for note in &restored.report.kept {
        println!("  {}", paint.wrap("33", note));
    }
}

fn print_changes(changes: &[Change], paint: &Paint) {
    if changes.is_empty() {
        println!("  no changes");
    }
    for change in changes.iter().take(DIFF_LINES) {
        let (label, color) = status_style(change.status);
        println!(
            "  {} {}",
            paint.wrap(color, &format!("{label:<8}")),
            change.path
        );
    }
    if changes.len() > DIFF_LINES {
        println!("  ... and {} more", changes.len() - DIFF_LINES);
    }
}

fn status_style(status: Status) -> (&'static str, &'static str) {
    match status {
        Status::Added => ("added", "32"),
        Status::Modified => ("changed", "33"),
        Status::Deleted => ("deleted", "31"),
    }
}

fn log_line(snapshot: &Snapshot, changes: &[Change], paint: &Paint) -> String {
    let (agent, tool, detail) = snapshot.actions.last().map_or(("", "", ""), |action| {
        (
            action.agent.as_str(),
            action.tool.as_str(),
            action.detail.as_str(),
        )
    });
    let earlier = match snapshot.actions.len() {
        0 | 1 => String::new(),
        count => paint.dim(&format!("  (+{} earlier)", count - 1)),
    };
    format!(
        "{}  {}  {}  {:<7}{:<6} {}{}",
        paint.bold(&format!("#{:<4}", snapshot.id)),
        paint.dim(&format!("{:>7}", ago(snapshot.created_ms))),
        summary(changes, paint),
        agent,
        tool,
        one_line(detail, 60),
        earlier
    )
}

fn summary(changes: &[Change], paint: &Paint) -> String {
    const WIDTH: usize = 11;
    let parts: Vec<(String, &str)> = [
        (Status::Added, "+", "32"),
        (Status::Modified, "~", "33"),
        (Status::Deleted, "-", "31"),
    ]
    .into_iter()
    .filter_map(|(status, sign, color)| {
        let count = changes
            .iter()
            .filter(|change| change.status == status)
            .count();
        (count > 0).then(|| (format!("{sign}{count}"), color))
    })
    .collect();
    if parts.is_empty() {
        return paint.dim(&format!("{:<WIDTH$}", "no changes"));
    }
    let shown = parts.iter().map(|(text, _)| text.len()).sum::<usize>() + parts.len() - 1;
    let colored: Vec<String> = parts
        .iter()
        .map(|(text, color)| paint.wrap(color, text))
        .collect();
    format!(
        "{}{}",
        colored.join(" "),
        " ".repeat(WIDTH.saturating_sub(shown))
    )
}

fn describe(snapshot: &Snapshot) -> String {
    let action = snapshot.actions.last();
    let detail = action
        .map(|action| one_line(&action.detail, 60))
        .filter(|detail| !detail.is_empty());
    match (snapshot.kind, action, detail) {
        (Kind::Undo, _, _) => "the state right before an undo".into(),
        (Kind::Manual, _, Some(detail)) => format!("your snapshot: {detail}"),
        (Kind::Manual, _, None) => "your snapshot".into(),
        (Kind::Agent, Some(action), Some(detail)) => {
            format!("before {} ran {}: {detail}", action.agent, action.tool)
        }
        (Kind::Agent, Some(action), None) => format!("before {} ran {}", action.agent, action.tool),
        (Kind::Agent, None, _) => "agent snapshot".into(),
    }
}

fn resolve(path: &Path) -> Result<PathBuf> {
    let mut normal = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    let mut missing = Vec::new();
    let mut existing = normal.as_path();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return Ok(missing
                .iter()
                .rev()
                .fold(real, |path, part| path.join(part)));
        }
        missing.push(
            existing
                .file_name()
                .context("cannot resolve that path")?
                .to_owned(),
        );
        existing = existing.parent().context("cannot resolve that path")?;
    }
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() <= max && !text.contains('\n') {
        line.to_string()
    } else {
        format!(
            "{}...",
            line.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn plural(count: u64, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn ago(ms: u64) -> String {
    let seconds = now_ms().saturating_sub(ms) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

struct Paint(bool);

impl Paint {
    fn detect() -> Paint {
        Paint(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
}
