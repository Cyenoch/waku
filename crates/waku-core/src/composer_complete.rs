//! Provider-neutral composer discovery for the daemon.
//!
//! Provider-specific CLI command discovery was intentionally removed. Waku's
//! own command and skill trees are transport-independent and are safe to
//! expose for every configured HTTP endpoint.

use std::collections::BTreeSet;
use std::path::Path;

use waku_protocol::ProviderId;
use waku_protocol::composer::{CommandScope, FileEntry, SlashCommand};

const MAX_COMMAND_BYTES: u64 = 64 * 1024;
const MAX_WALK_DEPTH: usize = 8;

/// List workspace files and directories in stable display order.
pub fn list_project_files(root: &Path, cap: usize) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    walk_files(root, root, 0, cap.max(1), &mut entries);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.truncate(cap);
    entries
}

fn walk_files(root: &Path, path: &Path, depth: usize, cap: usize, entries: &mut Vec<FileEntry>) {
    if entries.len() >= cap || depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<_> = read_dir.filter_map(Result::ok).collect();
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        if entries.len() >= cap {
            break;
        }
        let child = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Ok(relative) = child.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        entries.push(FileEntry {
            path: relative,
            is_dir: file_type.is_dir(),
        });
        if file_type.is_dir() {
            walk_files(root, &child, depth + 1, cap, entries);
        }
    }
}

/// Discover Waku-owned slash commands and shared skills.
pub fn discover_slash_commands(_provider: ProviderId, project_root: &Path) -> Vec<SlashCommand> {
    let mut commands = builtin_commands();
    scan_command_files(
        &project_root.join(".waku/commands"),
        CommandScope::Project,
        &mut commands,
    );
    if let Some(home) = dirs::home_dir() {
        scan_command_files(
            &home.join(".config/waku/commands"),
            CommandScope::User,
            &mut commands,
        );
    }
    scan_skill_files(&project_root.join(".agents/skills"), &mut commands);
    if let Some(home) = dirs::home_dir() {
        scan_skill_files(&home.join(".agents/skills"), &mut commands);
    }
    deduplicate(commands)
}

fn builtin_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "init".into(),
            description: "Create or update AGENTS.md for this repository.".into(),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: Some("Analyze this repository and write AGENTS.md for coding agents working in it.".into()),
        },
        SlashCommand {
            name: "review".into(),
            description: "Review pending changes in this working tree.".into(),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: Some("Review the pending changes in this working tree and report bugs, regressions, and risky patterns.".into()),
        },
        SlashCommand {
            name: "commit".into(),
            description: "Create a commit for the current changes.".into(),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: Some("Inspect the current changes, stage the appropriate files, and create a clear commit.".into()),
        },
    ]
}

fn scan_command_files(root: &Path, scope: CommandScope, commands: &mut Vec<SlashCommand>) {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_COMMAND_BYTES {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(template) = std::fs::read_to_string(&path) else {
            continue;
        };
        let description = template
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(stem)
            .trim()
            .to_owned();
        commands.push(SlashCommand {
            name: stem.to_owned(),
            description,
            scope,
            argument_hint: None,
            template: Some(template),
        });
    }
}

fn scan_skill_files(root: &Path, commands: &mut Vec<SlashCommand>) {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        if depth > MAX_WALK_DEPTH {
            continue;
        }
        let Ok(read_dir) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in read_dir.filter_map(Result::ok) {
            let child = entry.path();
            let Ok(meta) = std::fs::metadata(&child) else {
                continue;
            };
            if meta.is_dir() {
                stack.push((child, depth + 1));
            } else if child.file_name().is_some_and(|name| name == "SKILL.md")
                && meta.len() <= MAX_COMMAND_BYTES
            {
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&child) else {
                    continue;
                };
                commands.push(SlashCommand {
                    name: name.to_owned(),
                    description: text
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or(name)
                        .trim()
                        .to_owned(),
                    scope: CommandScope::Skill,
                    argument_hint: None,
                    template: None,
                });
            }
        }
    }
}

fn deduplicate(commands: Vec<SlashCommand>) -> Vec<SlashCommand> {
    let mut seen = BTreeSet::new();
    commands
        .into_iter()
        .filter(|command| seen.insert(command.name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_commands_are_available_without_provider_catalogs() {
        let root = std::env::temp_dir().join(format!("waku-composer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".waku/commands")).unwrap();
        std::fs::write(root.join(".waku/commands/deploy.md"), "Deploy safely").unwrap();
        let commands = discover_slash_commands(ProviderId::new("local"), &root);
        assert!(commands.iter().any(|command| command.name == "review"));
        assert!(commands.iter().any(|command| command.name == "deploy"));
        let _ = std::fs::remove_dir_all(root);
    }
}
