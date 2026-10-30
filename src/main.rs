mod bind;
mod herdr;
mod hook;
mod review;
mod room;
mod setup;
mod state;
mod ui;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: herdr-rooms <command>

  open [--report]      open the room popup over the calling pane
                       (`room` runs this; /room adds --report:
                       print any error instead of failing)
  setup                the popup itself (a herdr plugin pane)
  room                 the room itself (a herdr plugin pane)
  hook <kind> [json]   agent hooks: claude-prompt | claude-stop | codex-notify
  install              put herdr-rooms and room on PATH, set up Claude Code and Codex
  uninstall            undo install";

/// The name `install` links this binary under so that one word opens a room from both
/// agents: `!room` runs it in Claude Code and in Codex alike.
const LAUNCHER: &str = "room";

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let invoked_as = args.first().and_then(|a| Path::new(a).file_name()).map(|n| n.to_string_lossy().into_owned());
    let mut args = args.split_off(1);
    if invoked_as.as_deref() == Some(LAUNCHER) {
        args.insert(0, "open".into());
    }
    let command = args.first().map(String::as_str).unwrap_or_default();
    let result = match command {
        // For Claude Code's /room: a failing shell command there aborts the command
        // before the model can say what happened, so report every outcome and exit 0.
        "open" if args.iter().any(|a| a == "--report") => {
            if let Err(err) = setup::open() {
                println!("herdr-rooms: {err:#}");
            }
            Ok(())
        }
        "open" => setup::open(),
        "setup" => setup::run(),
        "room" => room::run(),
        "hook" => hook::run(args.get(1).map(String::as_str).unwrap_or_default(), args.get(2).cloned()),
        "install" => install(),
        "uninstall" => uninstall(),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(err) = result {
        // A failing hook must never disturb the agent it runs in.
        if command == "hook" {
            return;
        }
        eprintln!("herdr-rooms: {err:#}");
        std::process::exit(1);
    }
}

const CODEX_SKILL: &str = "---
name: room
description: Open a herdr room, a review room where another agent reviews this session until they agree. Use when the user types $room or /room, or asks to open a room or to have another agent review the work.
---

Run this shell command once and tell the user its output in one short line, in the user's language:

    herdr-rooms open

It talks to the local herdr server over its Unix socket, which the sandbox blocks: run it with escalated permissions. If that is not possible, tell the user to type `!room` instead, which runs it directly. Do nothing else.
";

/// The marker `uninstall` looks for before it removes a notify line.
const CODEX_NOTIFY_HOOK: &str = "codex-notify";

/// The plugin's `/room`, also written as a user command: Claude Code lists a plugin's
/// commands as `/<plugin>:<command>`, and this keeps the short `/room` next to it.
const CLAUDE_COMMAND: &str = include_str!("../claude-plugin/commands/room.md");

/// The marker `uninstall` looks for before it removes a skill or a command.
const OPEN_COMMAND: &str = "herdr-rooms open";

struct Paths {
    bin_dir: PathBuf,
    codex_home: PathBuf,
    claude_home: PathBuf,
}

fn paths() -> Result<Paths> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
    let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
    Ok(Paths { bin_dir: home.join(".local/bin"), codex_home, claude_home })
}

/// Puts the binary on PATH (as `herdr-rooms` for the agent hooks, and as `room`),
/// adds the Claude Code plugin and the short `/room`, and wires Codex up: the `$room`
/// skill, plus a `notify` hook so a Codex host's turns reach the room. Run again after
/// the plugin moves: the links point at this binary.
fn install() -> Result<()> {
    let Paths { bin_dir, codex_home, claude_home } = paths()?;
    let exe = std::env::current_exe()?.canonicalize()?;
    std::fs::create_dir_all(&bin_dir)?;
    let mut done = vec![];
    for name in ["herdr-rooms", LAUNCHER] {
        let link = bin_dir.join(name);
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)?;
        }
        link_to(&exe, &link)?;
        println!("linked   {} -> {}", link.display(), exe.display());
    }
    done.push(format!("herdr-rooms and {LAUNCHER} are in {}", bin_dir.display()));

    // The checkout this binary was built in: <root>/target/release/herdr-rooms.
    match exe.ancestors().nth(3) {
        Some(root) if root.join(".claude-plugin/marketplace.json").is_file() => done.push(install_claude_plugin(root)),
        _ => println!("skipped  Claude Code: this binary is not inside a herdr-rooms checkout"),
    }

    if !claude_home.is_dir() {
        println!("skipped  /room: {} does not exist", claude_home.display());
    } else {
        let command = claude_home.join("commands/room.md");
        match std::fs::read_to_string(&command) {
            Ok(text) if !text.contains(OPEN_COMMAND) => {
                println!("warning  {} is another /room; left as it is, /{PLUGIN_NAME}:room still works", command.display());
            }
            _ => {
                std::fs::create_dir_all(command.parent().unwrap_or(&claude_home))?;
                std::fs::write(&command, CLAUDE_COMMAND)?;
                println!("wrote    {}", command.display());
            }
        }
    }

    if !codex_home.is_dir() {
        println!("skipped  Codex: {} does not exist", codex_home.display());
    } else {
        let skill_dir = codex_home.join("skills/room");
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(skill_dir.join("SKILL.md"), CODEX_SKILL)?;
        println!("wrote    {}", skill_dir.join("SKILL.md").display());

        let config = codex_home.join("config.toml");
        let text = std::fs::read_to_string(&config).unwrap_or_default();
        let has_notify = text.lines().any(|l| l.trim_start().starts_with("notify") && l.contains('='));
        if text.contains(CODEX_NOTIFY_HOOK) {
            println!("ok       {} already notifies herdr-rooms", config.display());
            done.push("Codex is set up".into());
        } else if has_notify {
            println!(
                "warning  {} already has a `notify` program; Codex allows one. To host rooms from Codex, \
                 make it also run: {} hook {CODEX_NOTIFY_HOOK} <json>",
                config.display(),
                bin_dir.join("herdr-rooms").display()
            );
            done.push("Codex already has a notify program: see the README to host rooms from Codex".into());
        } else {
            // Top-level keys must come before the first [table], so prepend.
            let link = bin_dir.join("herdr-rooms").display().to_string();
            let line = format!("notify = [{link:?}, \"hook\", \"{CODEX_NOTIFY_HOOK}\"]\n");
            std::fs::write(&config, format!("{line}{text}"))?;
            println!("added    notify to {} (a no-op outside a room)", config.display());
            done.push("Codex is set up".into());
        }
    }
    // Run as a herdr action, nobody sees stdout: say it in a notification too.
    if std::env::var_os("HERDR_SOCKET_PATH").is_some() {
        herdr::notify("Rooms installed", &done.join(" · "));
    }
    Ok(())
}

#[cfg(unix)]
fn link_to(target: &Path, link: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, link)?)
}

/// Native Windows is not supported yet: the hooks and the install script are POSIX shell,
/// and Claude Code's sandbox, which a read-only Claude reviewer needs, does not run there.
#[cfg(not(unix))]
fn link_to(_target: &Path, _link: &Path) -> Result<()> {
    anyhow::bail!("native Windows is not supported yet: run herdr and your agents in WSL2")
}

/// Undoes `install`: the links, the command, the skill and the notify line, each only
/// when it is ours.
fn uninstall() -> Result<()> {
    let Paths { bin_dir, codex_home, claude_home } = paths()?;
    for name in ["herdr-rooms", LAUNCHER] {
        let link = bin_dir.join(name);
        let ours = std::fs::read_link(&link).is_ok_and(|target| target.file_name().is_some_and(|n| n == "herdr-rooms"));
        if ours {
            std::fs::remove_file(&link)?;
            println!("removed  {}", link.display());
        }
    }
    let command = claude_home.join("commands/room.md");
    if std::fs::read_to_string(&command).is_ok_and(|t| t.contains(OPEN_COMMAND)) {
        std::fs::remove_file(&command)?;
        println!("removed  {}", command.display());
    }
    let skill = codex_home.join("skills/room");
    if std::fs::read_to_string(skill.join("SKILL.md")).is_ok_and(|t| t.contains(OPEN_COMMAND)) {
        std::fs::remove_dir_all(&skill)?;
        println!("removed  {}", skill.display());
    }
    let config = codex_home.join("config.toml");
    if let Ok(text) = std::fs::read_to_string(&config) {
        let is_ours = |l: &str| l.trim_start().starts_with("notify") && l.contains(CODEX_NOTIFY_HOOK);
        if text.lines().any(is_ours) {
            let kept: String = text.split_inclusive('\n').filter(|l| !is_ours(l)).collect();
            std::fs::write(&config, kept)?;
            println!("removed  the notify line from {}", config.display());
        }
    }
    if setup::on_path("claude") && claude_marketplace(&claude_marketplaces()).is_some() {
        claude(&["plugin", "uninstall", PLUGIN_SELECTOR]);
        claude(&["plugin", "marketplace", "remove", PLUGIN_NAME]);
    }
    println!("Left for you: `herdr plugin uninstall {PLUGIN_NAME}`.");
    Ok(())
}

/// The name of the herdr plugin, the Claude Code plugin and its marketplace alike.
const PLUGIN_NAME: &str = "herdr-rooms";
const PLUGIN_SELECTOR: &str = "herdr-rooms@herdr-rooms";

/// Runs a `claude` CLI command, printing what it did; false when it failed.
fn claude(args: &[&str]) -> bool {
    match std::process::Command::new("claude").args(args).output() {
        Ok(out) if out.status.success() => {
            println!("ok       claude {}", args.join(" "));
            true
        }
        Ok(out) => {
            let why = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
            println!("warning  claude {}: {why}", args.join(" "));
            false
        }
        Err(e) => {
            println!("warning  claude {}: {e}", args.join(" "));
            false
        }
    }
}

fn claude_marketplaces() -> String {
    std::process::Command::new("claude")
        .args(["plugin", "marketplace", "list", "--json"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// The folder this plugin's Claude Code marketplace is declared at, when there is one.
fn claude_marketplace(list_json: &str) -> Option<PathBuf> {
    let list: serde_json::Value = serde_json::from_str(list_json).ok()?;
    let entry = list.as_array()?.iter().find(|m| m["name"] == PLUGIN_NAME)?;
    Some(PathBuf::from(entry["path"].as_str().or(entry["installLocation"].as_str()).unwrap_or_default()))
}

/// Registers the Claude Code plugin from this checkout, which is its own marketplace
/// (`.claude-plugin/marketplace.json`). A marketplace of the same name declared at
/// another folder, an older checkout say, is replaced. Both `claude` calls are no-ops
/// when the plugin is already installed from here.
fn install_claude_plugin(root: &Path) -> String {
    if !setup::on_path("claude") {
        println!("skipped  Claude Code: claude is not on PATH");
        return "Claude Code not found".into();
    }
    if let Some(declared) = claude_marketplace(&claude_marketplaces()) {
        if declared != root {
            claude(&["plugin", "marketplace", "remove", PLUGIN_NAME]);
        }
    }
    let root = root.display().to_string();
    if claude(&["plugin", "marketplace", "add", &root]) && claude(&["plugin", "install", PLUGIN_SELECTOR]) {
        "Claude Code has /room".into()
    } else {
        "Claude Code setup failed: run the install action again".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_this_plugin_marketplace() {
        let list = r#"[{"name":"other","source":"github","repo":"a/b"},
                       {"name":"herdr-rooms","source":"directory","path":"/src/herdr-rooms"}]"#;
        assert_eq!(claude_marketplace(list), Some(PathBuf::from("/src/herdr-rooms")));
        assert_eq!(claude_marketplace(r#"[{"name":"other","path":"/x"}]"#), None);
        assert_eq!(claude_marketplace("not json"), None);
    }

    #[test]
    fn uninstall_recognizes_what_install_writes() {
        assert!(CLAUDE_COMMAND.contains(OPEN_COMMAND));
        assert!(CODEX_SKILL.contains(OPEN_COMMAND));
    }
}
