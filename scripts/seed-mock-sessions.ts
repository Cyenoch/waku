#!/usr/bin/env bun

/**
 * Seeds oversized synthetic sessions into the dev database so transcript
 * performance can be measured against something far larger than real history.
 *
 * The rows written here are exactly what the Rust side writes: a narrow
 * `sessions` row, the rest of `AgentSession` as JSON in `session_details`
 * (minus `messages`, which the app keeps in its own table), and one `messages`
 * row per message. Nothing about the shapes is invented — see `src/model.rs`
 * and `session_data` in `src/persistence.rs`.
 *
 * Every id it writes starts with `9e5e0000-`, so a reseed can delete precisely
 * what a previous run added and never touch real sessions.
 *
 *   bun ./scripts/seed-mock-sessions.ts --only trajectory # deterministic ledger fixture
 *   bun ./scripts/seed-mock-sessions.ts --only trajectory --clean # remove ledger fixture only
 *   bun ./scripts/seed-mock-sessions.ts --clean    # remove them again
 *
 * The trajectory profile also writes the daemon-owned continuation snapshot to
 * `snapshots/{session_id}.json`; it never embeds a harness snapshot in the
 * session-details JSON.
 *
 * The app reads sessions once at startup, so a running debug build only shows
 * these after its next relaunch.
 */

import { createHash } from "node:crypto";
import { mkdirSync, readdirSync, renameSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { Database } from "bun:sqlite";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");

/** Shared prefix of every id this script owns; `--clean` matches on it. */
const MOCK_ID_PREFIX = "9e5e0000-";
/** Fixed ids keep the trajectory profile idempotent across repeated runs. */
const TRAJECTORY_SESSION_ID = `${MOCK_ID_PREFIX}0000-4000-8000-000000000001`;
const TRAJECTORY_BASE_MS = 1_760_000_000_000;

function trajectoryUuid(index: number): string {
  const suffix = index.toString(16).padStart(12, "0");
  return `${MOCK_ID_PREFIX}1000-4000-8000-${suffix}`;
}

type Options = {
  databasePath: string;
  projectRef: string | undefined;
  scale: number;
  seed: number;
  only: string[];
  clean: boolean;
};

function parseOptions(argv: string[]): Options {
  const options: Options = {
    databasePath: join(root, "temp/app.db"),
    projectRef: undefined,
    scale: 1,
    seed: 20260806,
    only: [],
    clean: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = () => {
      const next = argv[index + 1];
      if (next === undefined) throw new Error(`${flag} needs a value`);
      index += 1;
      return next;
    };
    switch (flag) {
      case "--db":
        options.databasePath = resolve(value());
        break;
      case "--project":
        options.projectRef = value();
        break;
      case "--scale":
        options.scale = Number(value());
        break;
      case "--seed":
        options.seed = Number(value());
        break;
      case "--only":
        options.only = value().split(",").map((key) => key.trim());
        break;
      case "--clean":
        options.clean = true;
        break;
      default:
        throw new Error(`unknown flag: ${flag}`);
    }
  }
  if (!Number.isFinite(options.scale) || options.scale <= 0) {
    throw new Error("--scale must be a positive number");
  }
  return options;
}

// ---------------------------------------------------------------------------
// Deterministic randomness
// ---------------------------------------------------------------------------

/** mulberry32 — same seed, same database, so a run is reproducible. */
function makeRandom(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

type Random = () => number;

const pick = <T>(random: Random, values: readonly T[]): T =>
  values[Math.floor(random() * values.length)]!;

const int = (random: Random, min: number, max: number): number =>
  min + Math.floor(random() * (max - min + 1));

const chance = (random: Random, probability: number): boolean =>
  random() < probability;

function mockUuid(random: Random): string {
  const hex = (count: number) =>
    Array.from({ length: count }, () => "0123456789abcdef"[Math.floor(random() * 16)]).join("");
  // Valid v4 layout so `Uuid::parse_str` accepts it, with a fixed first group
  // marking it as ours.
  return `${MOCK_ID_PREFIX}${hex(4)}-4${hex(3)}-8${hex(3)}-${hex(12)}`;
}

// ---------------------------------------------------------------------------
// Content banks
//
// Realistic-looking material matters here: the cost being measured is markdown
// parsing, syntax highlighting, text shaping and wrapping, so the mock content
// has to have the same shape as what an agent actually produces.
// ---------------------------------------------------------------------------

const PROMPTS = [
  "the transcript stutters when I scroll fast through a long session, can you profile it",
  "make the sidebar remember which project was selected across restarts",
  "why does hydrating a session take so long on startup?",
  "add a keyboard shortcut to collapse every turn in the transcript",
  "the composer loses focus after a turn finishes, fix that",
  "reasoning blocks should keep their scroll position when a turn is expanded",
  "audit the render path for anything touching the filesystem",
  "tool output over a few thousand lines should virtualize instead of laying out everything",
  "the diff view wraps long lines in a way that breaks alignment, take a look",
  "can we cache the markdown parse between frames instead of reparsing",
  "session switching drops a frame or two, find out where the time goes",
  "make the activity disclosure animate open without janking the list",
  "the checkpoint row runs git on the ui thread somewhere, track it down",
  "add a setting for the transcript font size and make it live-update",
  "long code blocks inside assistant messages should get a copy button",
  "measure how much time we spend in text shaping for a 2000 message session",
  "the turn fold summary should say how many tools ran, not just the duration",
  "scrolling to the bottom on a new message should be instant, not animated",
  "check whether we re-measure every row when only the last one changed",
  "images in tool output are decoded on the ui thread, move that off",
];

const HEADINGS = [
  "What I found",
  "Root cause",
  "The fix",
  "Measurements",
  "Why it was slow",
  "Trade-offs",
  "What changed",
  "Follow-ups",
  "How it works now",
  "Verification",
];

const SENTENCES = [
  "The row builder was rebuilding whole-session state for every visible item, so the cost scaled with history rather than with what is on screen.",
  "Each frame walked the full transcript to compute anchors, which is fine at fifty messages and ruinous at five thousand.",
  "I hoisted that into a cache refreshed once per frame and keyed it on the fingerprint the fold already computes.",
  "The measurement pass shapes text twice: once for the height query and once for paint, and neither result was retained.",
  "Nothing here needs the filesystem, but the checkpoint affordance was reaching for `git` before the background prefetch had landed.",
  "Because the list is virtualized, only the visible window matters — the trap is any per-frame work that iterates the whole collection anyway.",
  "Deserializing the detail blob dominates session switching once a transcript passes a few megabytes.",
  "I kept the allocation out of the hot path by reusing the buffer between rows rather than collecting into a fresh vector.",
  "The generation counter guards against a superseded background pass overwriting newer state when two switches land close together.",
  "Wrapping is the expensive part: every soft break costs a shaping run, and code blocks defeat the fast path entirely.",
  "Syntax highlighting now runs once per code block and the result is stored beside the parsed block rather than recomputed on scroll.",
  "The regression only shows on a high-refresh display, where the frame budget is under seven milliseconds.",
  "I left the synchronous path in place for one-shot actions, where freshness is worth more than latency.",
  "Interleaving the blocks at message boundaries keeps ordering stable even when a provider emits events out of order.",
  "That turned a linear scan per row into a single hash map lookup, which is what made the difference under a long transcript.",
];

const INLINE_SNIPPETS = [
  "`transcript_rows_fingerprint`",
  "`folded_transcript_row_kinds`",
  "`ListState::splice`",
  "`cx.background_executor().spawn`",
  "`AgentSession::hydrate`",
  "`session_details.data`",
  "`render` → `measure` → `paint`",
  "`Window::request_animation_frame`",
];

const FILES = [
  "src/app/transcript.rs",
  "src/app/transcript_view.rs",
  "src/app/components.rs",
  "src/app/streaming.rs",
  "src/app/sessions.rs",
  "src/md/render.rs",
  "src/md/parser.rs",
  "src/md/highlight.rs",
  "src/persistence.rs",
  "src/model.rs",
  "src/driver/codex.rs",
  "src/ui/scrollbar.rs",
];

const CODE_SAMPLES: { language: string; lines: string[] }[] = [
  {
    language: "rust",
    lines: [
      "pub(super) fn visible_rows(&self, window: &Window) -> Range<usize> {",
      "    let viewport = window.viewport_size().height;",
      "    let first = self.offsets.partition_point(|offset| *offset < self.scroll_top);",
      "    let last = self.offsets.partition_point(|offset| *offset < self.scroll_top + viewport);",
      "    first..last.min(self.rows.len())",
      "}",
      "",
      "impl TranscriptView {",
      "    fn remeasure(&mut self, kind: TranscriptRowKind, cx: &mut Context<Self>) {",
      "        let Some(index) = self.rows.iter().position(|row| *row == kind) else {",
      "            return;",
      "        };",
      "        self.list.splice(index..index + 1, 1);",
      "        cx.notify();",
      "    }",
      "}",
    ],
  },
  {
    language: "rust",
    lines: [
      "let cached = self.row_cache.borrow();",
      "if cached.fingerprint == fingerprint {",
      "    return cached.rows.clone();",
      "}",
      "drop(cached);",
      "",
      "let rows = folded_transcript_row_kinds(session, &self.expanded_turns);",
      "*self.row_cache.borrow_mut() = RowCache {",
      "    fingerprint,",
      "    rows: rows.clone(),",
      "};",
      "rows",
    ],
  },
  {
    language: "typescript",
    lines: [
      "const rows = useMemo(() => foldTranscript(session, expanded), [session, expanded]);",
      "",
      "useLayoutEffect(() => {",
      "  if (!listRef.current) return;",
      "  listRef.current.scrollToIndex(rows.length - 1, { align: 'end' });",
      "}, [rows.length]);",
      "",
      "export function foldTranscript(session: Session, expanded: Set<string>) {",
      "  const anchors = session.blocks.map((block) => block.afterMessage);",
      "  return interleave(session.messages.length, anchors).filter((row) =>",
      "    expanded.has(row.turnId) || !row.hidden,",
      "  );",
      "}",
    ],
  },
  {
    language: "bash",
    lines: [
      "cargo build --profile dev 2>&1 | tail -20",
      "hyperfine --warmup 3 'target/debug/wakuwaku --bench transcript'",
      "rg -n 'fn render' src/app | wc -l",
      "sqlite3 temp/app.db 'EXPLAIN QUERY PLAN SELECT * FROM sessions ORDER BY updated_at'",
    ],
  },
  {
    language: "json",
    lines: [
      "{",
      '  "session": "long-history",',
      '  "messages": 4021,',
      '  "blocks": 6144,',
      '  "frame_ms": { "p50": 4.1, "p95": 11.7, "max": 38.2 },',
      '  "hydrate_ms": 182.4',
      "}",
    ],
  },
  {
    language: "diff",
    lines: [
      "-    let rows = folded_transcript_row_kinds(session, &self.expanded_turns);",
      "+    let fingerprint = transcript_rows_fingerprint(session, &self.expanded_turns);",
      "+    let rows = self.cached_rows(session, fingerprint);",
      "     for row in &rows {",
      "-        self.measure(row, window, cx);",
      "+        self.measure_if_dirty(row, window, cx);",
      "     }",
    ],
  },
];

const COMMANDS = [
  "cargo build",
  "cargo test transcript",
  "cargo clippy --all-targets",
  "rg -n 'transcript_rows_fingerprint' src",
  "git diff --stat",
  "sqlite3 temp/app.db 'select count(*) from messages'",
  "wc -l src/app/*.rs",
  "bun ./scripts/dev.ts",
];

const TOOL_TITLES = ["read", "edit", "write", "glob", "grep", "todowrite", "webfetch"];

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

function paragraph(random: Random, sentences = int(random, 2, 5)): string {
  const parts: string[] = [];
  for (let index = 0; index < sentences; index += 1) {
    let sentence = pick(random, SENTENCES);
    if (chance(random, 0.3)) {
      sentence = sentence.replace(/\.$/, ` — see ${pick(random, INLINE_SNIPPETS)}.`);
    }
    parts.push(sentence);
  }
  return parts.join(" ");
}

function codeFence(random: Random, minLines: number): string {
  const sample = pick(random, CODE_SAMPLES);
  const lines: string[] = [];
  let repeat = 0;
  while (lines.length < minLines) {
    for (const line of sample.lines) {
      // Vary the repeats so highlighting and wrapping never see identical input.
      lines.push(repeat === 0 ? line : line.replace(/\b(\d+)\b/g, (digits) => String(Number(digits) + repeat)));
    }
    repeat += 1;
    if (repeat > 40) break;
  }
  return ["```" + sample.language, ...lines, "```"].join("\n");
}

function table(random: Random, rows = int(random, 3, 9)): string {
  const lines = [
    "| Path | Rows | p50 (ms) | p95 (ms) |",
    "| --- | ---: | ---: | ---: |",
  ];
  for (let index = 0; index < rows; index += 1) {
    lines.push(
      `| \`${pick(random, FILES)}\` | ${int(random, 40, 9000)} | ${(random() * 6).toFixed(2)} | ${(random() * 24).toFixed(2)} |`,
    );
  }
  return lines.join("\n");
}

function bulletList(random: Random, items = int(random, 3, 7)): string {
  return Array.from({ length: items }, () => {
    const nested = chance(random, 0.35)
      ? `\n  - ${pick(random, SENTENCES).slice(0, 90)}`
      : "";
    return `- ${paragraph(random, 1)}${nested}`;
  }).join("\n");
}

function taskList(random: Random, items = int(random, 3, 6)): string {
  return Array.from(
    { length: items },
    () => `- [${chance(random, 0.6) ? "x" : " "}] ${paragraph(random, 1).slice(0, 110)}`,
  ).join("\n");
}

function numberedList(random: Random, items = int(random, 3, 6)): string {
  return Array.from(
    { length: items },
    (_, index) => `${index + 1}. **${pick(random, HEADINGS)}** — ${paragraph(random, 1)}`,
  ).join("\n");
}

/** A markdown answer of roughly `targetBytes`, mixing every construct the
 * renderer supports so parsing, highlighting, tables and wrapping all get hit. */
function markdownAnswer(random: Random, targetBytes: number): string {
  const parts: string[] = [`### ${pick(random, HEADINGS)}`, paragraph(random)];
  let size = parts.join("\n\n").length;
  while (size < targetBytes) {
    const roll = random();
    const part =
      roll < 0.3
        ? paragraph(random)
        : roll < 0.5
          ? codeFence(random, int(random, 6, 40))
          : roll < 0.63
            ? bulletList(random)
            : roll < 0.72
              ? numberedList(random)
              : roll < 0.79
                ? taskList(random)
                : roll < 0.86
                  ? table(random)
                  : roll < 0.93
                    ? `> ${paragraph(random, 2)}`
                    : `#### ${pick(random, HEADINGS)}`;
    parts.push(part);
    size += part.length + 2;
  }
  if (chance(random, 0.5)) {
    parts.push(
      `See [\`${pick(random, FILES)}\`](${pick(random, FILES)}) and ~~the old path~~ for the rest.`,
    );
  }
  return parts.join("\n\n");
}

function buildLog(random: Random, lines: number): string {
  const out: string[] = [];
  for (let index = 0; index < lines; index += 1) {
    const roll = random();
    if (roll < 0.08) {
      out.push(
        `warning: unused variable: \`${pick(random, ["index", "cx", "window", "session"])}\``,
      );
      out.push(`  --> ${pick(random, FILES)}:${int(random, 20, 1800)}:${int(random, 4, 60)}`);
      out.push("   |");
      out.push(`${String(int(random, 20, 1800)).padStart(4)} |     let ${pick(random, ["rows", "anchors", "cache"])} = session.transcript_blocks.len();`);
      out.push("   |         ^^^^ help: if this is intentional, prefix it with an underscore");
    } else if (roll < 0.2) {
      out.push(
        `   Compiling ${pick(random, ["gpui", "wakuwaku", "rusqlite", "pulldown-cmark", "smol"])} v${int(random, 0, 3)}.${int(random, 0, 40)}.${int(random, 0, 9)}`,
      );
    } else {
      out.push(
        `test ${pick(random, ["app::tests", "md::parser::tests", "persistence::tests"])}::${pick(random, ["folds_settled_turns", "keeps_scroll_anchor", "hydrates_one_session", "writes_only_dirty_rows"])}_${index} ... ok`,
      );
    }
  }
  out.push("");
  out.push(
    `test result: ok. ${lines} passed; 0 failed; 0 ignored; finished in ${(random() * 30).toFixed(2)}s`,
  );
  return out.join("\n");
}

function unifiedDiff(random: Random, hunks: number): string {
  const path = pick(random, FILES);
  const out = [`diff --git a/${path} b/${path}`, `--- a/${path}`, `+++ b/${path}`];
  for (let index = 0; index < hunks; index += 1) {
    const start = int(random, 20, 1600);
    out.push(`@@ -${start},${int(random, 6, 18)} +${start},${int(random, 6, 20)} @@`);
    const sample = pick(random, CODE_SAMPLES);
    for (const line of sample.lines) {
      const roll = random();
      out.push(roll < 0.25 ? `-${line}` : roll < 0.6 ? `+${line}` : ` ${line}`);
    }
  }
  return out.join("\n");
}

function searchResults(random: Random, hits: number): string {
  const out: string[] = [];
  for (let index = 0; index < hits; index += 1) {
    const sample = pick(random, CODE_SAMPLES);
    out.push(
      `${pick(random, FILES)}:${int(random, 10, 1900)}:${pick(random, sample.lines).trim()}`,
    );
  }
  return out.join("\n");
}

function reasoningText(random: Random, targetBytes: number): string {
  const parts: string[] = [];
  let size = 0;
  while (size < targetBytes) {
    const part = paragraph(random, int(random, 2, 6));
    parts.push(part);
    size += part.length + 2;
  }
  return parts.join("\n\n");
}

// ---------------------------------------------------------------------------
// Session assembly
// ---------------------------------------------------------------------------

type Activity = {
  id: string;
  source_id: string | null;
  kind: "reasoning" | "command" | "fileChange" | "search" | "plan" | "tool";
  title: string;
  detail: string | null;
  arguments?: string;
  output?: string;
  image_urls?: string[];
  failed: boolean;
  complete: boolean;
};

type Block = {
  after_message: number;
  turn_id: string;
  content: { kind: "activities"; data: Activity[] };
};

type MessageRow = {
  id: string;
  turn_id: string;
  position: number;
  role: "user" | "assistant";
  content: string;
  created_at: number;
};

type Profile = {
  key: string;
  title: string;
  provider: string;
  model: string;
  /** Turns before `--scale`. */
  turns: number;
  answerBytes: [number, number];
  answersPerTurn: [number, number];
  activityBlocksPerTurn: [number, number];
  activitiesPerBlock: [number, number];
  outputLines: [number, number];
  reasoningBytes: [number, number];
  reasoningBlocksPerTurn: [number, number];
  imageChance: number;
};

const PROFILES: Profile[] = [
  {
    // Row count: the fingerprint and fold walk every message and block each
    // frame, so this one is about sheer length rather than heavy rows.
    key: "long-history",
    title: "Perf · long history (thousands of turns)",
    provider: "openai-codex",
    model: "gpt-5.2-codex",
    turns: 2000,
    answerBytes: [300, 1400],
    answersPerTurn: [1, 2],
    activityBlocksPerTurn: [1, 3],
    activitiesPerBlock: [1, 2],
    outputLines: [6, 30],
    reasoningBytes: [200, 900],
    reasoningBlocksPerTurn: [0, 1],
    imageChance: 0,
  },
  {
    // Markdown: tables, fenced code, nested lists — parsing, highlighting and
    // wrapping, which is where per-row measurement time goes.
    key: "markdown-heavy",
    title: "Perf · markdown and code heavy answers",
    provider: "anthropic",
    model: "claude-opus-5",
    turns: 260,
    answerBytes: [6000, 22000],
    answersPerTurn: [1, 3],
    activityBlocksPerTurn: [1, 2],
    activitiesPerBlock: [1, 2],
    outputLines: [10, 60],
    reasoningBytes: [400, 1600],
    reasoningBlocksPerTurn: [0, 2],
    imageChance: 0,
  },
  {
    // Tool activity: huge outputs and many disclosure sections behind a fold.
    // Expanding one of these turns is the worst case for the transcript.
    key: "tool-storm",
    title: "Perf · tool activity storm (expand a turn)",
    provider: "xai",
    model: "grok-4.5",
    turns: 90,
    answerBytes: [400, 2500],
    answersPerTurn: [1, 2],
    activityBlocksPerTurn: [15, 40],
    activitiesPerBlock: [1, 2],
    outputLines: [30, 150],
    reasoningBytes: [800, 3500],
    reasoningBlocksPerTurn: [1, 3],
    imageChance: 0.12,
  },
  {
    // Everything at once, and the biggest detail blob — the hydrate-cost test.
    key: "kitchen-sink",
    title: "Perf · everything at maximum",
    provider: "opencode-zen",
    model: "gpt-5.2",
    turns: 700,
    answerBytes: [800, 9000],
    answersPerTurn: [1, 3],
    activityBlocksPerTurn: [2, 8],
    activitiesPerBlock: [1, 3],
    outputLines: [15, 70],
    reasoningBytes: [400, 3000],
    reasoningBlocksPerTurn: [0, 3],
    imageChance: 0.05,
  },
];

function blobReferences(databasePath: string): string[] {
  const blobsRoot = join(resolve(databasePath, ".."), "blobs");
  try {
    if (!statSync(blobsRoot).isDirectory()) return [];
  } catch {
    return [];
  }
  const references: string[] = [];
  for (const shard of readdirSync(blobsRoot)) {
    let files: string[];
    try {
      files = readdirSync(join(blobsRoot, shard));
    } catch {
      continue;
    }
    for (const file of files) {
      if (/\.(png|jpe?g|gif|webp)$/i.test(file)) references.push(`wakuwaku-blob:${file}`);
    }
  }
  return references;
}

function makeActivity(
  random: Random,
  profile: Profile,
  images: string[],
): Activity {
  const roll = random();
  const id = mockUuid(random);
  const sourceId = `call_${Math.floor(random() * 1e12).toString(36)}`;
  const lines = int(random, profile.outputLines[0], profile.outputLines[1]);

  if (roll < 0.3) {
    const command = pick(random, COMMANDS);
    return {
      id,
      source_id: sourceId,
      kind: "command",
      title: "bash",
      detail: JSON.stringify({ command }),
      arguments: JSON.stringify({ command, cwd: root }, null, 2),
      output: buildLog(random, lines),
      failed: chance(random, 0.06),
      complete: true,
    };
  }
  if (roll < 0.5) {
    const path = pick(random, FILES);
    return {
      id,
      source_id: sourceId,
      kind: "fileChange",
      title: chance(random, 0.5) ? "edit" : "write",
      detail: `+${int(random, 3, 220)} −${int(random, 0, 90)}`,
      arguments: JSON.stringify({ filePath: join(root, path) }, null, 2),
      output: unifiedDiff(random, int(random, 1, 6)),
      failed: false,
      complete: true,
    };
  }
  if (roll < 0.68) {
    const query = pick(random, ["transcript row", "fingerprint", "ListState", "hydrate", "measure"]);
    return {
      id,
      source_id: sourceId,
      kind: "search",
      title: chance(random, 0.5) ? "grep" : "websearch",
      detail: JSON.stringify({ query }),
      arguments: JSON.stringify({ query, path: "src", limit: 50 }, null, 2),
      output: searchResults(random, Math.max(4, Math.floor(lines / 3))),
      failed: false,
      complete: true,
    };
  }
  if (roll < 0.74) {
    return {
      id,
      source_id: sourceId,
      kind: "plan",
      title: "todowrite",
      detail: null,
      arguments: JSON.stringify(
        {
          todos: Array.from({ length: int(random, 3, 8) }, (_, index) => ({
            content: paragraph(random, 1).slice(0, 80),
            status: index === 0 ? "in_progress" : "pending",
          })),
        },
        null,
        2,
      ),
      output: taskList(random, int(random, 3, 8)),
      failed: false,
      complete: true,
    };
  }

  const withImage = images.length > 0 && chance(random, profile.imageChance);
  return {
    id,
    source_id: sourceId,
    kind: "tool",
    title: pick(random, TOOL_TITLES),
    detail: JSON.stringify({ filePath: join(root, pick(random, FILES)) }),
    arguments: JSON.stringify(
      { title: `Inspecting ${pick(random, FILES)}`, limit: int(random, 50, 400) },
      null,
      2,
    ),
    output: withImage ? undefined : searchResults(random, Math.max(3, Math.floor(lines / 4))),
    image_urls: withImage
      ? Array.from({ length: int(random, 1, 3) }, () => pick(random, images))
      : undefined,
    failed: chance(random, 0.04),
    complete: true,
  };
}

type BuiltSession = {
  id: string;
  title: string;
  provider: string;
  model: string;
  createdAt: number;
  updatedAt: number;
  lastReplyAt: number;
  messages: MessageRow[];
  blockCount: number;
  detail: string;
};

function buildSession(
  profile: Profile,
  options: Options,
  projectId: string,
  images: string[],
  index: number,
): BuiltSession {
  const random = makeRandom(options.seed + index * 7919);
  const sessionId = mockUuid(random);
  const turnCount = Math.max(1, Math.round(profile.turns * options.scale));

  const messages: MessageRow[] = [];
  const blocks: Block[] = [];
  const turns: unknown[] = [];

  // Walk backwards from now so the newest turn is the last one, and the whole
  // session lands in a plausible recent window.
  const now = Math.floor(Date.now() / 1000);
  let clock = now - turnCount * 45 - index * 3600;
  const createdAt = clock;

  for (let turnIndex = 0; turnIndex < turnCount; turnIndex += 1) {
    const turnId = mockUuid(random);
    const startedAt = clock;

    messages.push({
      id: mockUuid(random),
      turn_id: turnId,
      position: messages.length,
      role: "user",
      content: `${pick(random, PROMPTS)}${chance(random, 0.25) ? `\n\n\`\`\`\n${pick(random, COMMANDS)}\n\`\`\`` : ""}`,
      created_at: clock,
    });
    clock += int(random, 2, 12);

    // Work for this turn: reasoning and activity blocks anchored after the
    // prompt, exactly where the streaming path would have put them.
    const anchor = messages.length;
    const reasoningBlocks = int(random, profile.reasoningBlocksPerTurn[0], profile.reasoningBlocksPerTurn[1]);
    const activityBlocks = int(random, profile.activityBlocksPerTurn[0], profile.activityBlocksPerTurn[1]);
    const work: Block[] = [];
    for (let blockIndex = 0; blockIndex < reasoningBlocks; blockIndex += 1) {
      const startedMs = clock * 1000;
      work.push({
        after_message: anchor,
        turn_id: turnId,
        content: {
          kind: "activities",
          data: [{
            id: mockUuid(random),
            source_id: null,
            kind: "reasoning",
            title: "Reasoning",
            detail: null,
            arguments: null,
            output: null,
            image_urls: [],
            failed: false,
            complete: true,
            file_changes: [],
            display_target: null,
            display_description: null,
            reasoning: {
              content: reasoningText(random, int(random, profile.reasoningBytes[0], profile.reasoningBytes[1])),
              started_at_ms: startedMs,
              finished_at_ms: startedMs + int(random, 400, 9000),
            },
          }],
        },
      });
    }
    for (let blockIndex = 0; blockIndex < activityBlocks; blockIndex += 1) {
      const count = int(random, profile.activitiesPerBlock[0], profile.activitiesPerBlock[1]);
      work.push({
        after_message: anchor,
        turn_id: turnId,
        content: {
          kind: "activities",
          data: Array.from({ length: count }, () => makeActivity(random, profile, images)),
        },
      });
    }
    // Interleave so reasoning and tool calls alternate the way they arrive.
    for (let position = work.length - 1; position > 0; position -= 1) {
      const swap = Math.floor(random() * (position + 1));
      [work[position], work[swap]] = [work[swap]!, work[position]!];
    }
    blocks.push(...work);
    clock += int(random, 5, 90);

    const answers = int(random, profile.answersPerTurn[0], profile.answersPerTurn[1]);
    for (let answerIndex = 0; answerIndex < answers; answerIndex += 1) {
      messages.push({
        id: mockUuid(random),
        turn_id: turnId,
        position: messages.length,
        role: "assistant",
        content: markdownAnswer(random, int(random, profile.answerBytes[0], profile.answerBytes[1])),
        created_at: clock,
      });
      clock += int(random, 1, 8);
    }

    const status = chance(random, 0.04) ? "interrupted" : chance(random, 0.03) ? "failed" : "completed";
    turns.push({
      id: turnId,
      turn_count: turnIndex + 1,
      status,
      provider_turn_started: true,
      started_at: startedAt,
      completed_at: clock,
      checkpoint: chance(random, 0.7)
        ? {
            turn_count: turnIndex + 1,
            // No such ref exists, so the rewind affordance stays hidden —
            // `prefetch_checkpoint_refs` only offers it for refs git resolves.
            git_ref: `refs/wakuwaku/session-${sessionId}-turn-${turnIndex + 1}`,
            status: "ready",
            files: Array.from({ length: int(random, 0, 5) }, () => ({
              path: pick(random, FILES),
              additions: int(random, 1, 300),
              deletions: int(random, 0, 120),
            })),
            created_at: clock,
          }
        : null,
    });
    clock += int(random, 20, 900);
  }

  // Clamped so a long walk cannot land in the future, staggered so the seeded
  // sessions sort deterministically at the top of the list instead of tying.
  const updatedAt = Math.min(clock, now - index * 60);
  const detail = JSON.stringify({
    id: sessionId,
    title: profile.title,
    project_id: projectId,
    provider: profile.provider,
    model: profile.model,
    runtime_mode: "fullAccess",
    interaction_mode: "build",
    status: "idle",
    created_at: createdAt,
    updated_at: updatedAt,
    last_reply_at: updatedAt,
    runtime_event_cursor: null,
    turns,
    transcript_blocks: blocks,
  });

  return {
    id: sessionId,
    title: profile.title,
    provider: profile.provider,
    model: profile.model,
    createdAt,
    updatedAt,
    lastReplyAt: updatedAt,
    messages,
    blockCount: blocks.length,
    detail,
  };
}

type TrajectoryRecordSeed = {
  recordId: string;
  sequence: number;
  requestId: string | null;
  parentRecordId: string | null;
  promptId: string | null;
  turnCount: number;
  step: number;
  kind: "System" | "User" | "Context" | "Request" | "Assistant" | "Tool";
  lane: "Input" | "Model" | "Tools";
  status: "pending" | "running" | "completed" | "failed" | "cancelled" | "unavailable";
  title: string;
  preview: string;
  searchText: string;
  startedAtMs: number | null;
  firstTokenAtMs: number | null;
  completedAtMs: number | null;
  durationMs: number | null;
  ttftMs: number | null;
  detailJson: string;
};

type BuiltTrajectoryFixture = {
  session: BuiltSession;
  prompt: {
    promptId: string;
    sequence: number;
    fingerprint: string;
    systemPrompt: string;
    toolsJson: string;
    optionsJson: string;
    modelHint: string;
    createdAtMs: number;
  };
  records: TrajectoryRecordSeed[];
  snapshot: string;
};

const TRAJECTORY_SYSTEM_PROMPT = `You are WakuWaku, a coding assistant working in the user's workspace.

Use the available tools to inspect and change files. Prefer precise, minimal edits. Do not invent file contents or command results you have not observed.

Interaction modes:
- Build: implement the requested change. You may edit files and, when permitted, run shell commands.
- Plan: analyze the request and propose an approach. Do not edit files or run mutating shell commands; use read-only inspection only.

Follow the user's instructions. Ask when a required choice is ambiguous.`;
const TRAJECTORY_PROVIDER = "openai-responses";
const TRAJECTORY_MODEL = "gpt-5.2";
const TRAJECTORY_MODEL_HINT = `${TRAJECTORY_PROVIDER}/${TRAJECTORY_MODEL}`;
const TRAJECTORY_TOOLS = [
  { name: "read_file", description: "Read a UTF-8 file.", parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] } },
  { name: "run_tests", description: "Run focused tests.", parameters: { type: "object", properties: { command: { type: "string" } }, required: ["command"] } },
];
const TRAJECTORY_OPTIONS = { max_tokens: 4096, temperature: null, reasoning: "medium", service_tier: "default" };
const TRAJECTORY_USAGE = { input: 1874, output: 638, cache_read: 512, cache_write: 128, reasoning: 244 };

function boundedTrajectoryText(text: string, limit: number): string {
  return text.length <= limit ? text : `${text.slice(0, limit - 1)}…`;
}

function trajectoryDetail(kind: string, detail: Record<string, unknown>): string {
  return JSON.stringify({ v: 1, kind, ...detail });
}

function makeTrajectorySnapshot(): string {
  const budget = { max_messages: null, max_tokens: null };
  const queueMode = "OneAtATime";
  const callId = "call_trajectory_read_001";
  const messages = [
    { User: { parts: [{ Text: "Trace the request pipeline and run the focused tests." }] } },
    { Assistant: { content: [
      { Thinking: { thinking: "I will inspect the request path before running the focused checks.", signature: null, redacted: false } },
      { ToolCall: { id: callId, name: "read_file", arguments: { path: "crates/wakuwaku-core/src/trajectory_store.rs" }, thought_signature: null } },
    ], model: TRAJECTORY_MODEL, provider: TRAJECTORY_PROVIDER, response_id: "resp_trajectory_001", usage: { ...TRAJECTORY_USAGE, total_tokens: 3024 }, stop_reason: "ToolUse", error_message: null } },
    { ToolResult: { tool_call_id: callId, tool_name: "read_file", content: [{ Text: "The trajectory writer commits one revision per batch." }], is_error: false } },
    { Assistant: { content: [{ Text: "The request path is ready; the focused checks can run next." }], model: TRAJECTORY_MODEL, provider: TRAJECTORY_PROVIDER, response_id: "resp_trajectory_002", usage: { ...TRAJECTORY_USAGE, input: 2240, output: 284, reasoning: 96, total_tokens: 2772 }, stop_reason: "Stop", error_message: null } },
  ];
  return JSON.stringify({ system_prompt: TRAJECTORY_SYSTEM_PROMPT, messages, queue_mode: queueMode, budget, checkpoints: [{ message_count: messages.length, queue_mode: queueMode, budget }], initial_checkpoint: { message_count: 0, queue_mode: queueMode, budget } });
}

function buildTrajectoryFixture(projectId: string): BuiltTrajectoryFixture {
  const createdAt = Math.floor((TRAJECTORY_BASE_MS - 1_000) / 1000);
  const updatedAt = Math.floor((TRAJECTORY_BASE_MS + 7_000) / 1000);
  const turnId = trajectoryUuid(500);
  const promptId = trajectoryUuid(1);
  const toolsJson = JSON.stringify(TRAJECTORY_TOOLS);
  const optionsJson = JSON.stringify(TRAJECTORY_OPTIONS);
  const fingerprint = createHash("sha256").update(`${TRAJECTORY_SYSTEM_PROMPT}\0${toolsJson}\0${optionsJson}\0${TRAJECTORY_MODEL_HINT}`).digest("hex");
  const messages: MessageRow[] = [
    { id: trajectoryUuid(501), turn_id: turnId, position: 0, role: "user", content: "Trace the request pipeline and run the focused tests.", created_at: createdAt + 1 },
    { id: trajectoryUuid(502), turn_id: turnId, position: 1, role: "assistant", content: "The request path is ready; the focused checks can run next.", created_at: updatedAt },
  ];
  const detail = JSON.stringify({ id: TRAJECTORY_SESSION_ID, title: "Trajectory · deterministic request ledger", project_id: projectId, provider: TRAJECTORY_PROVIDER, model: TRAJECTORY_MODEL, runtime_mode: "fullAccess", interaction_mode: "build", status: "idle", created_at: createdAt, updated_at: updatedAt, last_reply_at: updatedAt, runtime_event_cursor: null, turns: [{ id: turnId, turn_count: 1, status: "completed", provider_turn_started: true, started_at: createdAt + 1, completed_at: updatedAt, checkpoint: null }], transcript_blocks: [] });
  const records: TrajectoryRecordSeed[] = [];
  const addRecord = (record: Omit<TrajectoryRecordSeed, "recordId" | "sequence">): string => {
    const sequence = records.length + 2;
    const recordId = trajectoryUuid(sequence);
    records.push({ ...record, recordId, sequence });
    return recordId;
  };
  const addStatic = (kind: TrajectoryRecordSeed["kind"], lane: TrajectoryRecordSeed["lane"], title: string, preview: string, searchText: string, turnCount: number, step: number, value: Record<string, unknown>): void => {
    addRecord({ requestId: null, parentRecordId: null, promptId, turnCount, step, kind, lane, status: "completed", title, preview: boundedTrajectoryText(preview, 512), searchText: boundedTrajectoryText(searchText, 2560), startedAtMs: null, firstTokenAtMs: null, completedAtMs: null, durationMs: null, ttftMs: null, detailJson: trajectoryDetail(kind.toLowerCase(), value) });
  };
  addStatic("System", "Input", "System prompt", TRAJECTORY_SYSTEM_PROMPT, TRAJECTORY_SYSTEM_PROMPT, 0, 0, { model_hint: TRAJECTORY_MODEL_HINT, text: TRAJECTORY_SYSTEM_PROMPT });
  addStatic("User", "Input", "User request", messages[0]!.content, messages[0]!.content, 1, 0, { text: messages[0]!.content, display_text: messages[0]!.content, has_image: false, source_metadata_missing: false, attachments: [] });
  addStatic("Context", "Input", "Request context", "Focused trajectory fixture with two provider steps", "trajectory fixture provider steps", 1, 0, { steering_id: null, forked_from: null });

  const addRequest = (step: number, started: number, duration: number, input: number, output: number, reasoning: number, title: string): string => {
    const requestId = trajectoryUuid(records.length + 2);
    addRecord({ requestId, parentRecordId: null, promptId, turnCount: 1, step, kind: "Request", lane: "Model", status: "completed", title, preview: `${TRAJECTORY_MODEL_HINT} · exact usage · completed`, searchText: `${title} ${TRAJECTORY_MODEL_HINT}`, startedAtMs: started, firstTokenAtMs: started + (step === 1 ? 180 : 160), completedAtMs: started + duration, durationMs: duration, ttftMs: step === 1 ? 180 : 160, detailJson: trajectoryDetail("request", { model: TRAJECTORY_MODEL, provider: TRAJECTORY_PROVIDER, options: TRAJECTORY_OPTIONS, usage: { ...TRAJECTORY_USAGE, input, output, reasoning }, timing: { started_at_ms: started, first_token_at_ms: started + (step === 1 ? 180 : 160), completed_at_ms: started + duration, duration_ms: duration, ttft_ms: step === 1 ? 180 : 160 }, error: null }) });
    return requestId;
  };
  const request1Started = TRAJECTORY_BASE_MS + 1_000;
  const request1Id = addRequest(1, request1Started, 1_200, 1874, 638, 244, "Request · inspect trajectory writer");
  addRecord({ requestId: request1Id, parentRecordId: request1Id, promptId, turnCount: 1, step: 1, kind: "Assistant", lane: "Model", status: "completed", title: "Assistant · tool batch", preview: "I will inspect the request path before running the focused checks.", searchText: "assistant inspect request path tool batch", startedAtMs: request1Started, firstTokenAtMs: request1Started + 180, completedAtMs: request1Started + 1_200, durationMs: 1_200, ttftMs: 180, detailJson: trajectoryDetail("assistant", { model: TRAJECTORY_MODEL, provider: TRAJECTORY_PROVIDER, usage: TRAJECTORY_USAGE, stop_reason: "ToolUse", error_message: null, blocks: [{ type: "thinking", text: "I will inspect the request path before running the focused checks.", redacted: false }, { type: "tool_call", call_id: "call_trajectory_read_001", name: "read_file", arguments: { path: "crates/wakuwaku-core/src/trajectory_store.rs" } }] }) });
  for (let index = 0; index < 54; index += 1) {
    const started = request1Started + 1_250;
    const duration = 120 + index * 11;
    const path = `crates/wakuwaku-core/src/trajectory/${index % 2 === 0 ? "recording.rs" : "projection.rs"}`;
    addRecord({ requestId: request1Id, parentRecordId: request1Id, promptId, turnCount: 1, step: 1, kind: "Tool", lane: "Tools", status: "completed", title: `read_file · ${path.split("/").pop()}`, preview: `Read ${path} (${index + 1}/54)`, searchText: `read_file ${path} parallel tool ${index + 1}`, startedAtMs: started, firstTokenAtMs: null, completedAtMs: started + duration, durationMs: duration, ttftMs: null, detailJson: trajectoryDetail("tool", { call_id: `call_parallel_read_${String(index + 1).padStart(3, "0")}`, name: "read_file", is_error: false, arguments: { path }, result: `Read ${path}; line count ${80 + index}.`, timing: { started_at_ms: started, completed_at_ms: started + duration, duration_ms: duration } }) });
  }
  const request2Started = TRAJECTORY_BASE_MS + 4_000;
  const request2Id = addRequest(2, request2Started, 1_300, 2488, 782, 311, "Request · run focused checks");
  addRecord({ requestId: request2Id, parentRecordId: request2Id, promptId, turnCount: 1, step: 2, kind: "Assistant", lane: "Model", status: "completed", title: "Assistant · focused checks", preview: "The request path is ready; the focused checks can run next.", searchText: "assistant request path focused checks", startedAtMs: request2Started, firstTokenAtMs: request2Started + 160, completedAtMs: request2Started + 1_300, durationMs: 1_300, ttftMs: 160, detailJson: trajectoryDetail("assistant", { model: TRAJECTORY_MODEL, provider: TRAJECTORY_PROVIDER, usage: { ...TRAJECTORY_USAGE, input: 2488, output: 782, reasoning: 311 }, stop_reason: "Stop", error_message: null, blocks: [{ type: "text", text: "The request path is ready; the focused checks can run next." }] }) });
  for (let index = 0; index < 62; index += 1) {
    const failed = index === 17;
    const started = request2Started + 1_350;
    const duration = 150 + index * 9;
    const command = index % 2 === 0 ? "cargo test -p wakuwaku-core trajectory" : "cargo test -p wakuwaku-client trajectory";
    const result = failed ? "test process exited with status 1: one assertion failed" : `tests passed (${index + 2} assertions)`;
    addRecord({ requestId: request2Id, parentRecordId: request2Id, promptId, turnCount: 1, step: 2, kind: "Tool", lane: "Tools", status: failed ? "failed" : "completed", title: `run_tests · ${failed ? "failed" : "passed"}`, preview: boundedTrajectoryText(`${command} · ${result}`, 512), searchText: boundedTrajectoryText(`${command} ${result} parallel tool ${index + 1}`, 2560), startedAtMs: started, firstTokenAtMs: null, completedAtMs: started + duration, durationMs: duration, ttftMs: null, detailJson: trajectoryDetail("tool", { call_id: `call_parallel_test_${String(index + 1).padStart(3, "0")}`, name: "run_tests", is_error: failed, arguments: { command }, result, timing: { started_at_ms: started, completed_at_ms: started + duration, duration_ms: duration } }) });
  }
  return { session: { id: TRAJECTORY_SESSION_ID, title: "Trajectory · deterministic request ledger", provider: TRAJECTORY_PROVIDER, model: TRAJECTORY_MODEL, createdAt, updatedAt, lastReplyAt: updatedAt, messages, blockCount: 0, detail }, prompt: { promptId, sequence: 1, fingerprint, systemPrompt: TRAJECTORY_SYSTEM_PROMPT, toolsJson, optionsJson, modelHint: TRAJECTORY_MODEL_HINT, createdAtMs: TRAJECTORY_BASE_MS }, records, snapshot: makeTrajectorySnapshot() };
}

function trajectorySnapshotPath(databasePath: string): string {
  return join(resolve(databasePath, ".."), "snapshots", `${TRAJECTORY_SESSION_ID}.json`);
}

function deleteTrajectoryRows(database: Database, sessionLike: string): number {
  const sessions = database
    .query<{ session_id: string }, [string]>("SELECT session_id FROM trajectory_sessions WHERE session_id LIKE ?")
    .all(sessionLike)
    .map((row) => row.session_id);
  for (const sessionId of sessions) {
    database.run("DELETE FROM trajectory_records WHERE session_id = ?", [sessionId]);
    database.run("DELETE FROM trajectory_prompt_snapshots WHERE session_id = ?", [sessionId]);
  }
  database.run("DELETE FROM trajectory_sessions WHERE session_id LIKE ?", [sessionLike]);
  return sessions.length;
}

function deleteTrajectorySnapshot(databasePath: string): void {
  const path = trajectorySnapshotPath(databasePath);
  for (const candidate of [path, `${path}.tmp`]) {
    try {
      unlinkSync(candidate);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
  }
}

function writeTrajectorySnapshot(databasePath: string, snapshot: string): void {
  const path = trajectorySnapshotPath(databasePath);
  mkdirSync(resolve(path, ".."), { recursive: true });
  const temporary = `${path}.tmp`;
  writeFileSync(temporary, snapshot, { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, path);
}

function deleteTrajectoryFixture(database: Database): number {
  const removed = deleteTrajectoryRows(database, TRAJECTORY_SESSION_ID);
  database.run("DELETE FROM messages WHERE session_id = ?", [TRAJECTORY_SESSION_ID]);
  database.run("DELETE FROM session_details WHERE session_id = ?", [TRAJECTORY_SESSION_ID]);
  database.run("DELETE FROM sessions WHERE id = ?", [TRAJECTORY_SESSION_ID]);
  return removed;
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

function deleteMockSessions(database: Database): number {
  const like = `${MOCK_ID_PREFIX}%`;
  const before = database
    .query<{ count: number }, [string]>("SELECT count(*) AS count FROM sessions WHERE id LIKE ?")
    .get(like)!.count;
  deleteTrajectoryRows(database, like);
  database.run("DELETE FROM messages WHERE session_id LIKE ?", [like]);
  database.run("DELETE FROM session_details WHERE session_id LIKE ?", [like]);
  database.run("DELETE FROM sessions WHERE id LIKE ?", [like]);
  return before;
}

function resolveProject(database: Database, ref: string | undefined): { id: string; name: string } {
  const projects = database
    .query<{ id: string; name: string }, []>("SELECT id, name FROM projects ORDER BY position")
    .all();
  if (projects.length === 0) {
    throw new Error("no projects in the database — add one in the app first");
  }
  if (!ref) return projects[0]!;
  const match = projects.find((project) => project.id === ref || project.name === ref);
  if (!match) {
    throw new Error(
      `no project matching "${ref}" (have: ${projects.map((project) => project.name).join(", ")})`,
    );
  }
  return match;
}

const formatBytes = (bytes: number): string =>
  bytes >= 1024 * 1024 ? `${(bytes / 1024 / 1024).toFixed(1)} MB` : `${(bytes / 1024).toFixed(0)} KB`;

function main(): void {
  const options = parseOptions(process.argv.slice(2));
  const database = new Database(options.databasePath, { create: false, readwrite: true });
  database.run("PRAGMA busy_timeout = 10000");
  database.run("PRAGMA foreign_keys = ON");

  if (options.clean) {
    const trajectoryOnly = options.only.length === 1 && options.only[0] === "trajectory";
    const removed = database.transaction(() =>
      trajectoryOnly ? deleteTrajectoryFixture(database) : deleteMockSessions(database),
    )();
    if (trajectoryOnly || options.only.length === 0) deleteTrajectorySnapshot(options.databasePath);
    console.log(`[seed] removed ${removed} mock session(s) from ${options.databasePath}`);
    database.close();
    return;
  }

  const project = resolveProject(database, options.projectRef);
  const images = blobReferences(options.databasePath);
  const knownProfiles: Record<string, true> = Object.fromEntries(
    [...PROFILES.map((profile) => profile.key), "trajectory"].map((profile) => [profile, true]),
  );
  const unknownProfiles = options.only.filter((profile) => !knownProfiles[profile]);
  if (unknownProfiles.length > 0) {
    throw new Error(`--only matched nothing (unknown: ${unknownProfiles.join(", ")}; have: ${Object.keys(knownProfiles).join(", ")})`);
  }
  const wantsTrajectory = options.only.length === 0 || options.only.includes("trajectory");
  const trajectoryOnly = options.only.length === 1 && options.only[0] === "trajectory";
  const profiles = options.only.length
    ? PROFILES.filter((profile) => options.only.includes(profile.key))
    : PROFILES;

  console.log(`[seed] project ${project.name} · scale ${options.scale} · ${images.length} blob(s) available`);
  const messageBytes = (session: BuiltSession) => session.messages.reduce((sum, message) => sum + Buffer.byteLength(message.content), 0);
  const built = profiles.map((profile, index) => buildSession(profile, options, project.id, images, index));
  const trajectory = wantsTrajectory ? buildTrajectoryFixture(project.id) : undefined;
  if (trajectory) console.log(`[seed] trajectory       ${String(trajectory.records.length).padStart(6)} records · exact revision 1`);

  const insertSession = database.prepare(`INSERT INTO sessions(id, project_id, title, provider, model, status, created_at, updated_at, last_reply_at) VALUES (?, ?, ?, ?, ?, 'idle', ?, ?, ?)`);
  const insertDetail = database.prepare("INSERT INTO session_details(session_id, data) VALUES (?, ?)");
  const insertMessage = database.prepare(`INSERT INTO messages(id, session_id, turn_id, position, role, content, created_at, streaming) VALUES (?, ?, ?, ?, ?, ?, ?, 0)`);
  const insertTrajectorySession = database.prepare("INSERT INTO trajectory_sessions(session_id, schema_version, generation, revision, next_sequence, availability) VALUES (?, 1, 1, 1, ?, 'exact')");
  const insertPrompt = database.prepare("INSERT INTO trajectory_prompt_snapshots(session_id, prompt_id, sequence, fingerprint, system_prompt, tools_json, options_json, model_hint, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)");
  const insertRecord = database.prepare(`INSERT INTO trajectory_records(session_id, record_id, sequence, revision, request_id, parent_record_id, prompt_id, turn_count, step, kind, lane, status, title, preview, search_text, started_at_ms, first_token_at_ms, completed_at_ms, duration_ms, ttft_ms, detail_json) VALUES (?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);

  const write = database.transaction(() => {
    const removed = trajectoryOnly ? deleteTrajectoryFixture(database) : deleteMockSessions(database);
    for (const session of built) {
      insertSession.run(session.id, project.id, session.title, session.provider, session.model, session.createdAt, session.updatedAt, session.lastReplyAt);
      insertDetail.run(session.id, session.detail);
      for (const message of session.messages) insertMessage.run(message.id, session.id, message.turn_id, message.position, message.role, message.content, message.created_at);
    }
    if (trajectory) {
      const session = trajectory.session;
      insertSession.run(session.id, project.id, session.title, session.provider, session.model, session.createdAt, session.updatedAt, session.lastReplyAt);
      insertDetail.run(session.id, session.detail);
      for (const message of session.messages) insertMessage.run(message.id, session.id, message.turn_id, message.position, message.role, message.content, message.created_at);
      insertTrajectorySession.run(session.id, trajectory.records.length + 2);
      insertPrompt.run(session.id, trajectory.prompt.promptId, trajectory.prompt.sequence, trajectory.prompt.fingerprint, trajectory.prompt.systemPrompt, trajectory.prompt.toolsJson, trajectory.prompt.optionsJson, trajectory.prompt.modelHint, trajectory.prompt.createdAtMs);
      for (const record of trajectory.records) insertRecord.run(session.id, record.recordId, record.sequence, record.requestId, record.parentRecordId, record.promptId, record.turnCount, record.step, record.kind, record.lane, record.status, record.title, record.preview, record.searchText, record.startedAtMs, record.firstTokenAtMs, record.completedAtMs, record.durationMs, record.ttftMs, record.detailJson);
    }
    return removed;
  });
  const removed = write();
  if (trajectory) writeTrajectorySnapshot(options.databasePath, trajectory.snapshot);
  else deleteTrajectorySnapshot(options.databasePath);
  database.run("PRAGMA wal_checkpoint(PASSIVE)");
  const allSessions = trajectory ? [...built, trajectory.session] : built;
  const totalMessages = allSessions.reduce((sum, session) => sum + session.messages.length, 0);
  const totalBytes = allSessions.reduce((sum, session) => sum + Buffer.byteLength(session.detail) + messageBytes(session), 0);
  database.close();
  console.log(`[seed] replaced ${removed} → wrote ${allSessions.length} sessions · ${totalMessages} messages · ${formatBytes(totalBytes)} of transcript`);
  console.log("[seed] relaunch the debug app to see them — sessions are read once at startup");
}

main();
