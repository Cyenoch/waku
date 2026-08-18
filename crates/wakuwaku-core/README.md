# wakuwaku-core

`wakuwaku-core` is WakuWaku's daemon-only runtime. It integrates the built-in Rust HTTP
harness, provider authentication and model catalog/cache handling, task
persistence, attachment storage, workspace filesystem and Git services, the
append-only usage ledger, and daemon-owned settings. The harness supports
OpenAI Responses, OpenAI Chat Completions, and Anthropic wire formats; its
generic `Tool` seam remains available for WakuWaku's local tools. The removed legacy
Computer Use integration is not part of the runtime. It depends on the
serializable contract in [`wakuwaku-protocol`](../wakuwaku-protocol), but contains no
desktop transport or UI.

The transport is an authenticated WebSocket (loopback by default). Requests
have stable UUIDs for idempotency; session events carry monotonically
increasing sequence numbers and runtime-generation IDs. The server keeps a
bounded replay journal, and stale events or commands from a replaced runtime
are ignored.

`DaemonClient` lives in [`wakuwaku-client`](../wakuwaku-client), which is what WakuWaku
Desktop depends on. `serve` and `WakuBackend` are used by the `wakuwaku-daemon`
binary.

Configuration ownership is explicit:

- the desktop owns `~/.wakuwaku/app.json` in Release and checkout-local
  `temp/app.json` in Debug;
- the daemon owns `~/.wakuwaku/settings.json`.

Task SQLite rows and durable attachment materializations are daemon-owned as
well. Client-local attachment paths are upload inputs or caches only; provider
prompts and persisted messages use daemon-issued paths and references.
Projectless task directories are daemon-owned too and live beneath
`~/.wakuwaku/projects`.

The protocol types use Serde's tagged JSON representation and are exported by
`wakuwaku-protocol`, including checked-in TypeScript bindings.
