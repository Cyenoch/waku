# Session titles

How each session in the sidebar gets its name.

WakuWaku talks to HTTP model endpoints through the built-in Rust harness. It does
not launch Codex, Claude Code, Amp, Grok Build, Cursor, OpenCode, Pi, or any
other external provider CLI or `app-server` process, and those agents are not
available to name a session. Titles are therefore local unless a future
provider stream actually sends one.

## The two title fields

[`AgentSession`](../crates/wakuwaku-protocol/src/model.rs) carries two:

| Field | Owner | Set by |
| --- | --- | --- |
| `title` | the user | inline rename; wins whenever it differs from `DEFAULT_TITLE` |
| `auto_title` | fallback | first-prompt local title, a fork label, or `DriverEvent::AutoTitleUpdated` |

[`display_title`](../crates/wakuwaku-protocol/src/model.rs) resolves them: an
explicit `title` first, then `auto_title`, then `DEFAULT_TITLE` (`"New task"`,
[`AgentSession::DEFAULT_TITLE`](../crates/wakuwaku-protocol/src/model.rs)). A
provider title therefore never overwrites a name the user typed.

## The local fallback

[`set_title_from_prompt`](../crates/wakuwaku-protocol/src/model.rs) takes the
**first seven words** of the first prompt, capped at 54 characters, and writes
them into `auto_title`. It is called once per session from
[`runtime.rs`](../src/app/runtime.rs) and no-ops if the session already has a
second message, a user title, or any `auto_title`.

This is a placeholder, not a generated title. It shares the `auto_title` field
so a later provider-owned title can replace it silently. Unwinding a first
prompt that never reached the provider clears that fallback.

A fork resets `title` to `DEFAULT_TITLE` and stores the fork label in
`auto_title` via [`fork_through_turn`](../crates/wakuwaku-protocol/src/model.rs).

## Provider-owned titles

The wire still has `DriverEvent::AutoTitleUpdated(Option<String>)`. The desktop
applies it in [`streaming.rs`](../src/app/streaming.rs) through
[`set_auto_title`](../crates/wakuwaku-protocol/src/model.rs), which trims, maps
empty to `None`, and never overwrites a user-owned title. The event is in the
stream `force_save` set in [`runtime.rs`](../src/app/runtime.rs), so a title
persists the moment it lands.

The embedded HTTP driver does not emit `AutoTitleUpdated`. Sessions keep the
prompt fallback unless the user renames them.

## Adding a provider

Do not spawn a second model call or an external harness to invent a title.
HTTP endpoints in this tree do not generate session names. If a future stream
carries a real title, forward it as `AutoTitleUpdated` and stop. Failure must
be silent; the local fallback must hold.
