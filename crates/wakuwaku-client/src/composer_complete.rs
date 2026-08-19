//! Pure client-side composer matching over daemon-provided command/file lists.
//!
//! Matching is provider-neutral: every configured HTTP endpoint sees the same
//! command/file helpers. Fast-mode and Codex CLI command catalogs were removed.

use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
pub use wakuwaku_protocol::composer::{CommandScope, FileEntry, ReportedCommand, SlashCommand};

pub const FILTER_CAP: usize = 64;
pub const FILE_INDEX_CAP: usize = 50_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerKind {
    Command,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: Range<usize>,
}

pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_prefix = &text[line_start..cursor];
    if let Some(query) = line_prefix.strip_prefix('/') {
        if !query.chars().any(char::is_whitespace) {
            return Some(Trigger {
                kind: TriggerKind::Command,
                query: query.to_owned(),
                range: line_start..cursor,
            });
        }
        return None;
    }
    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| {
            index + text[index..].chars().next().map_or(1, char::len_utf8)
        });
    let token = &text[token_start..cursor];
    Some(Trigger {
        kind: TriggerKind::File,
        query: token.strip_prefix('@')?.to_owned(),
        range: token_start..cursor,
    })
}

pub fn expand_command_template(template: &str, args: &str) -> String {
    let positional = args.split_whitespace().collect::<Vec<_>>();
    let mut expanded = String::with_capacity(template.len() + args.len());
    let mut consumed_args = false;
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        expanded.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            expanded.push_str(args);
            consumed_args = true;
            rest = tail;
        } else if let Some(tail) = after.strip_prefix('@') {
            expanded.push_str(args);
            consumed_args = true;
            rest = tail;
        } else if let Some(digit) = after
            .chars()
            .next()
            .and_then(|character| character.to_digit(10))
            .filter(|digit| (1..=9).contains(digit))
        {
            if let Some(argument) = positional.get(digit as usize - 1) {
                expanded.push_str(argument);
            }
            consumed_args = true;
            rest = &after[1..];
        } else {
            expanded.push('$');
            rest = after;
        }
    }
    expanded.push_str(rest);
    if !consumed_args && !args.is_empty() {
        expanded.push_str("\n\n");
        expanded.push_str(args);
    }
    expanded
}

/// Merge workspace-discovered commands with extra names reported by the
/// daemon. Provider-specific CLI catalogs are not consulted.
pub fn merge_reported_commands(
    discovered: &[SlashCommand],
    reported: &[ReportedCommand],
) -> Vec<SlashCommand> {
    let mut commands = discovered.to_vec();
    for report in reported {
        if let Some(existing) = commands
            .iter_mut()
            .find(|command| command.name == report.name)
        {
            if existing.description.trim().is_empty() {
                existing.description.clone_from(&report.description);
            }
            continue;
        }
        commands.push(SlashCommand {
            name: report.name.clone(),
            description: report.description.clone(),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        });
    }
    commands.sort_by(|left, right| {
        left.scope
            .display_rank()
            .cmp(&right.scope.display_rank())
            .then_with(|| left.name.cmp(&right.name))
    });
    commands
}

pub fn expanded_submission(prompt: &str, commands: &[SlashCommand]) -> Option<String> {
    let invocation = prompt.strip_prefix('/')?;
    let (name, args) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, args)| (name, args.trim()));
    let command = commands
        .iter()
        .find(|command| command.name == name && command.template.is_some())?;
    Some(expand_command_template(
        command.template.as_deref().unwrap_or_default(),
        args,
    ))
}

pub fn matcher() -> Matcher {
    Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())
}

#[derive(Clone, Debug)]
pub struct Scored<T> {
    pub item: T,
    pub positions: Vec<u32>,
}

fn filter_scored(
    haystack: &[&str],
    query: &str,
    matcher: &mut Matcher,
    cap: usize,
) -> Vec<(usize, Vec<u32>)> {
    if query.trim().is_empty() {
        return (0..haystack.len().min(cap))
            .map(|index| (index, Vec::new()))
            .collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored = Vec::new();
    for (index, text) in haystack.iter().enumerate() {
        if let Some(score) = pattern.score(Utf32Str::new(text, &mut buf), matcher) {
            scored.push((score, index));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(cap);
    scored
        .into_iter()
        .map(|(_, index)| {
            let mut positions = Vec::new();
            pattern.indices(
                Utf32Str::new(haystack[index], &mut buf),
                matcher,
                &mut positions,
            );
            positions.sort_unstable();
            positions.dedup();
            (index, positions)
        })
        .collect()
}

pub fn filter_commands(
    commands: &[SlashCommand],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<SlashCommand>> {
    let names = commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    filter_scored(&names, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: commands[index].clone(),
            positions,
        })
        .collect()
}

pub fn filter_files(
    files: &[FileEntry],
    query: &str,
    matcher: &mut Matcher,
) -> Vec<Scored<FileEntry>> {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    filter_scored(&paths, query, matcher, FILTER_CAP)
        .into_iter()
        .map(|(index, positions)| Scored {
            item: files[index].clone(),
            positions,
        })
        .collect()
}

pub fn highlight_byte_ranges(
    text: &str,
    positions: &[u32],
    char_offset: usize,
) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (char_index, (byte_index, character)) in (char_offset..).zip(text.char_indices()) {
        if positions.binary_search(&(char_index as u32)).is_ok() {
            let byte_end = byte_index + character.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte_index => last.end = byte_end,
                _ => ranges.push(byte_index..byte_end),
            }
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_templates_expand_arguments() {
        assert_eq!(
            expand_command_template("Review $1 in $ARGUMENTS", "src/lib.rs"),
            "Review src/lib.rs in src/lib.rs"
        );
    }

    #[test]
    fn trigger_detection_accepts_commands_and_mentions() {
        assert_eq!(
            detect_trigger("/rev", 4).map(|trigger| trigger.kind),
            Some(TriggerKind::Command)
        );
        assert_eq!(
            detect_trigger("see @src/", 9).map(|trigger| trigger.kind),
            Some(TriggerKind::File)
        );
        assert!(detect_trigger("user@example", 12).is_none());
    }

    #[test]
    fn reported_commands_merge_without_cli_catalogs() {
        let discovered = vec![SlashCommand {
            name: "review".into(),
            description: String::new(),
            scope: CommandScope::Project,
            argument_hint: None,
            template: Some("Review $ARGUMENTS".into()),
        }];
        let reported = vec![
            ReportedCommand {
                name: "review".into(),
                description: "Review changes".into(),
            },
            ReportedCommand {
                name: "compact".into(),
                description: "Compact context".into(),
            },
        ];
        let merged = merge_reported_commands(&discovered, &reported);
        let review = merged
            .iter()
            .find(|command| command.name == "review")
            .unwrap();
        assert_eq!(review.description, "Review changes");
        assert_eq!(review.scope, CommandScope::Project);
        assert!(merged.iter().any(|command| command.name == "compact"));
        assert!(merged.iter().all(|command| command.name != "fast"));
    }
}
