/**
 * WakuWaku local state schema.
 *
 * Drizzle is a build-time tool here: `bun run db:generate` diffs this file and
 * writes plain SQL into `db/migrations`, which the Rust app applies at startup
 * (see `apply_migrations` in `src/persistence.rs`). drizzle-orm never ships in
 * the binary — Rust owns every query.
 *
 * Session history is kept out of the `sessions` row: the row holds only what
 * the session list renders, so listing is a scan over narrow rows. The
 * transcript lives in `session_details` and messages in `messages`, both
 * fetched only when a session is opened.
 */

import { sql } from "drizzle-orm";
import {
  check,
  foreignKey,
  index,
  integer,
  primaryKey,
  sqliteTable,
  text,
  uniqueIndex,
} from "drizzle-orm/sqlite-core";

export const projects = sqliteTable("projects", {
  id: text("id").primaryKey(),
  name: text("name").notNull(),
  path: text("path").notNull(),
  /** Order shown in the sidebar. */
  position: integer("position").notNull(),
  /** When the project was added, unix seconds. */
  createdAt: integer("created_at").notNull(),
});

export const sessions = sqliteTable(
  "sessions",
  {
    id: text("id").primaryKey(),
    projectId: text("project_id").notNull(),
    /** Explicit user title; "New task" means the automatic fallback is active. */
    title: text("title").notNull(),
    /** Provider-generated title, with the first prompt as a local fallback. */
    autoTitle: text("auto_title"),
    provider: text("provider").notNull(),
    model: text("model"),
    status: text("status").notNull(),
    /** Session creation time, unix seconds. */
    createdAt: integer("created_at").notNull(),
    /** Any mutation, unix seconds — including title edits and truncation. */
    updatedAt: integer("updated_at").notNull(),
    /** Completion of the most recent assistant turn, unix seconds. */
    lastReplyAt: integer("last_reply_at"),
  },
  (table) => [
    index("sessions_by_project").on(table.projectId, table.updatedAt),
    index("sessions_by_updated_at").on(table.updatedAt),
    index("sessions_by_last_reply_at").on(table.lastReplyAt),
  ],
);

/**
 * Conversation messages, one row each.
 *
 * Split out of `sessions.data` so appending to a long conversation writes one
 * small row instead of rewriting the whole history, and so a message can be
 * read or counted without deserializing a transcript.
 */
export const messages = sqliteTable(
  "messages",
  {
    id: text("id").primaryKey(),
    sessionId: text("session_id").notNull(),
    turnId: text("turn_id"),
    /** Ordinal within the session; conversation order, not wall-clock. */
    position: integer("position").notNull(),
    role: text("role").notNull(),
    content: text("content").notNull(),
    /** User-visible text before provider-facing attachment mentions. */
    displayContent: text("display_content"),
    /** JSON-serialized MessageAttachment array. */
    attachments: text("attachments").notNull().default("[]"),
    createdAt: integer("created_at").notNull(),
    streaming: integer("streaming", { mode: "boolean" }).notNull(),
  },
  (table) => [index("messages_by_session").on(table.sessionId, table.position)],
);

/**
 * The rest of `AgentSession` as JSON — transcript blocks, turns, provider
 * cursor.
 *
 * Split from `sessions` because it is large and rarely read: keeping it in the
 * row would mean listing sessions pages through every transcript, and every
 * title edit rewrites a transcript-sized row.
 */
export const sessionDetails = sqliteTable("session_details", {
  sessionId: text("session_id").primaryKey(),
  data: text("data").notNull(),
});

/**
 * Append-only billed assistant responses. Rewind, fork, and session delete
 * never rewrite these rows; `event_id` is the insert-once identity.
 */
export const usageEvents = sqliteTable(
  "usage_events",
  {
    eventId: text("event_id").primaryKey(),
    sessionId: text("session_id").notNull(),
    projectPath: text("project_path").notNull(),
    provider: text("provider").notNull(),
    model: text("model").notNull(),
    timestampMs: integer("timestamp_ms").notNull(),
    input: integer("input").notNull(),
    output: integer("output").notNull(),
    cacheRead: integer("cache_read").notNull(),
    cacheWrite: integer("cache_write").notNull(),
    reasoning: integer("reasoning"),
  },
  (table) => [index("usage_events_by_time").on(table.timestampMs)],
);

/**
 * Daemon-owned trajectory ledger. One row per session; child prompt/record
 * tables cascade through this row so deleting a session cannot leave orphans.
 */
export const trajectorySessions = sqliteTable("trajectory_sessions", {
  sessionId: text("session_id")
    .primaryKey()
    .references(() => sessions.id, { onDelete: "cascade" }),
  schemaVersion: integer("schema_version").notNull(),
  generation: integer("generation").notNull(),
  revision: integer("revision").notNull(),
  nextSequence: integer("next_sequence").notNull(),
  availability: text("availability").notNull(),
});

export const trajectoryPromptSnapshots = sqliteTable(
  "trajectory_prompt_snapshots",
  {
    sessionId: text("session_id").notNull(),
    promptId: text("prompt_id").notNull(),
    sequence: integer("sequence").notNull(),
    fingerprint: text("fingerprint").notNull(),
    systemPrompt: text("system_prompt"),
    toolsJson: text("tools_json").notNull(),
    optionsJson: text("options_json").notNull(),
    modelHint: text("model_hint").notNull(),
    createdAtMs: integer("created_at_ms").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.sessionId, table.promptId] }),
    index("trajectory_prompts_by_sequence").on(table.sessionId, table.sequence),
    foreignKey({
      columns: [table.sessionId],
      foreignColumns: [trajectorySessions.sessionId],
      name: "trajectory_prompts_session_fk",
    }).onDelete("cascade"),
  ],
);

export const trajectoryRecords = sqliteTable(
  "trajectory_records",
  {
    sessionId: text("session_id").notNull(),
    recordId: text("record_id").notNull(),
    sequence: integer("sequence").notNull(),
    revision: integer("revision").notNull(),
    requestId: text("request_id"),
    parentRecordId: text("parent_record_id"),
    promptId: text("prompt_id"),
    turnCount: integer("turn_count").notNull(),
    step: integer("step").notNull(),
    kind: text("kind").notNull(),
    lane: text("lane").notNull(),
    status: text("status").notNull(),
    title: text("title").notNull(),
    preview: text("preview").notNull(),
    searchText: text("search_text").notNull(),
    startedAtMs: integer("started_at_ms"),
    firstTokenAtMs: integer("first_token_at_ms"),
    completedAtMs: integer("completed_at_ms"),
    durationMs: integer("duration_ms"),
    ttftMs: integer("ttft_ms"),
    detailJson: text("detail_json").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.sessionId, table.recordId] }),
    uniqueIndex("trajectory_records_by_sequence").on(
      table.sessionId,
      table.sequence,
    ),
    index("trajectory_records_by_request").on(table.sessionId, table.requestId),
    foreignKey({
      columns: [table.sessionId],
      foreignColumns: [trajectorySessions.sessionId],
      name: "trajectory_records_session_fk",
    }).onDelete("cascade"),
  ],
);

/**
 * Shadow session event log (diagnostics only; opt-in capture). One stream per
 * session generation; a fork/rewind lineage stores only its new suffix and
 * points at `parentStreamId`/`parentSeq`. `session_id` deliberately has no FK
 * to `sessions`: deleting a UI session must not erase diagnostic history.
 *
 * Note: journal entries for 0003+ are maintained by hand for the Rust
 * migration runner (`crates/wakuwaku-core/build.rs`); do not regenerate a
 * Drizzle snapshot for them. CHECK constraint names live only here; SQLite
 * names them from the migration text, so keep both sides in sync when
 * editing either.
 */
export const sessionStreams = sqliteTable(
  "session_streams",
  {
    streamId: text("stream_id").primaryKey(),
    sessionId: text("session_id").notNull(),
    parentStreamId: text("parent_stream_id"),
    parentSeq: integer("parent_seq"),
    generation: integer("generation").notNull().default(0),
    createdAtMs: integer("created_at_ms").notNull(),
    retiredAtMs: integer("retired_at_ms"),
  },
  (table) => [
    uniqueIndex("session_streams_by_session_generation").on(
      table.sessionId,
      table.generation,
    ),
    check("session_streams_parent_seq_check", sql`parent_seq IS NULL OR parent_seq > 0`),
    check("session_streams_generation_check", sql`generation >= 0`),
    foreignKey({
      columns: [table.parentStreamId],
      foreignColumns: [sessionStreams.streamId],
      name: "session_streams_parent_fk",
    }),
  ],
);

export const sessionHeads = sqliteTable(
  "session_heads",
  {
    streamId: text("stream_id").primaryKey(),
    headSeq: integer("head_seq").notNull().default(0),
    revision: integer("revision").notNull().default(0),
    schemaVersion: integer("schema_version").notNull(),
    lastEventId: text("last_event_id"),
    updatedAtMs: integer("updated_at_ms").notNull(),
  },
  (table) => [
    check("session_heads_head_seq_check", sql`head_seq >= 0`),
    check("session_heads_revision_check", sql`revision >= 0`),
    check("session_heads_schema_version_check", sql`schema_version > 0`),
    foreignKey({
      columns: [table.streamId],
      foreignColumns: [sessionStreams.streamId],
      name: "session_heads_stream_fk",
    }).onDelete("cascade"),
  ],
);

export const sessionEvents = sqliteTable(
  "session_events",
  {
    streamId: text("stream_id").notNull(),
    seq: integer("seq").notNull(),
    eventId: text("event_id").notNull(),
    commandId: text("command_id"),
    schemaVersion: integer("schema_version").notNull(),
    kind: text("kind").notNull(),
    payloadJson: text("payload_json").notNull(),
    createdAtMs: integer("created_at_ms").notNull(),
    runtimeId: text("runtime_id"),
    turnId: text("turn_id"),
  },
  (table) => [
    primaryKey({ columns: [table.streamId, table.seq] }),
    uniqueIndex("session_events_by_event_id").on(table.streamId, table.eventId),
    // A command id identifies a whole batch, so this is a lookup index;
    // reuse consistency is enforced by the writer's append transaction.
    index("session_events_by_command").on(table.streamId, table.commandId),
    index("session_events_by_kind").on(table.streamId, table.kind, table.seq),
    index("session_events_by_turn").on(table.streamId, table.turnId, table.seq),
    check("session_events_seq_check", sql`seq > 0`),
    check("session_events_schema_version_check", sql`schema_version > 0`),
    check("session_events_kind_check", sql`length(kind) > 0`),
    check(
      "session_events_payload_json_check",
      sql`json_valid(payload_json)`,
    ),
    foreignKey({
      columns: [table.streamId],
      foreignColumns: [sessionStreams.streamId],
      name: "session_events_stream_fk",
    }).onDelete("cascade"),
  ],
);
