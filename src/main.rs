use airbag::snapshot::{self, Settings};
use airbag::store::{Action, Kind, Project, Snapshot, Store, now_ms};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

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
    #[command(about = "List snapshots, newest first")]
    Log {
        #[arg(short = 'n', long, default_value_t = 20, help = "How many to show")]
        limit: usize,
    },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("airbag: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let store = Store::open()?;
    let settings = Settings::from_env();
    match cli.command {
        Command::Snap { message } => {
            let project = open_project(&store, &cli.dir)?;
            let action = Action {
                agent: "you".into(),
                tool: "snap".into(),
                detail: message.unwrap_or_default(),
                session: None,
                at_ms: now_ms(),
            };
            let snapshot = snapshot::take(&store, &project, &settings, Kind::Manual, Some(action))?;
            println!(
                "saved #{}: {} files, {}",
                snapshot.id,
                snapshot.files,
                human_bytes(snapshot.bytes)
            );
            for note in &snapshot.skipped {
                println!("  not protected: {note}");
            }
        }
        Command::Log { limit } => {
            let project = open_project(&store, &cli.dir)?;
            let snapshots = project.snapshots()?;
            if snapshots.is_empty() {
                println!("no snapshots yet for {}", project.root.display());
            }
            let paint = Paint::detect();
            for snapshot in snapshots.iter().rev().take(limit) {
                println!("{}", log_line(snapshot, &paint));
            }
        }
    }
    Ok(())
}

fn open_project(store: &Store, dir: &Path) -> Result<Project> {
    let root = snapshot::find_root(dir).with_context(|| {
        format!(
            "{} is not inside a git work tree; airbag protects git projects",
            dir.display()
        )
    })?;
    store.project(&root)
}

fn log_line(snapshot: &Snapshot, paint: &Paint) -> String {
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
        "{}  {}  {:<7} {:<6} {}{}",
        paint.bold(&format!("#{:<4}", snapshot.id)),
        paint.dim(&format!("{:>8}", ago(snapshot.created_ms))),
        agent,
        tool,
        one_line(detail, 60),
        earlier
    )
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() <= max && !text.contains('\n') {
        line.to_string()
    } else {
        format!("{}...", line.chars().take(max).collect::<String>())
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
