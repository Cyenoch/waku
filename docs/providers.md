# Provider endpoints

Waku uses the built-in Rust harness to connect directly to configured HTTP
model endpoints. It supports OpenAI Responses, OpenAI Chat Completions, and
Anthropic wire formats. It never launches, detects, installs, or authenticates
third-party CLIs.

## Built-in providers

The daemon always knows these identities. Sign in from Settings; provider APIs
discover their model catalogs live.

| ID | Auth | Base URL |
| --- | --- | --- |
| `openai-responses` / `openai-chat` | Official OpenAI API key (`OPENAI_API_KEY`) | `https://api.openai.com/v1` |
| `openai-codex` | ChatGPT subscription OAuth: browser PKCE with loopback `:1455`, or device fallback | `https://chatgpt.com/backend-api/codex` |
| `anthropic` | API key (`ANTHROPIC_API_KEY`) | `https://api.anthropic.com/v1` |
| `opencode-zen` | API key; stored separately from Go (`OPENCODE_API_KEY` environment fallback) | `https://opencode.ai/zen/v1` |
| `opencode-go` | API key; stored separately from Zen (`OPENCODE_API_KEY` environment fallback) | `https://opencode.ai/zen/go/v1` |
| `xai` | xAI API key (`XAI_API_KEY`) | `https://api.x.ai/v1` |
| `xai-oauth` | SuperGrok / X Premium+ device OAuth (`XAI_OAUTH_TOKEN` environment fallback) | `https://api.x.ai/v1` |

Codex browser and device verification pages, and xAI device verification,
open through the system default browser only. `XAI_API_KEY` never authenticates
SuperGrok. Go and Zen never share a stored key.

## Custom endpoints

Open **Settings → Providers** for a configurable endpoint:

- **ID**, **Name**, and **Base URL** (including `/v1`)
- **API format**: OpenAI Responses, OpenAI Chat Completions, or Anthropic
- **API-key environment**: daemon environment-variable name; Waku never stores the secret
- **Headers**: non-secret only
- **Models**: explicit IDs used only after live discovery returns an error

## Discovery and selection

Sessions persist `ProviderId` and model ID. Provider APIs are queried at
`GET {base}/models` (Codex uses its catalog envelope). A successful discovery
response, including an empty catalog, is authoritative. Discovery errors use
the last-good cache, or an applicable provider seed; custom endpoint model IDs
are the fallback only after that discovery error. Catalog entries carry
compatibility metadata, so unsupported models remain unavailable and cannot be
selected. Typed service tier is available only for supported entries in the
official OpenAI API catalogs, not merely because an endpoint speaks an OpenAI
format.

## Secrets

API keys entered in Waku are stored in the macOS Keychain. Login fails
explicitly when that store is unavailable. Environment-variable credentials are
read at runtime; `settings.json` never contains tokens or keys.

## Safety

The default access mode is **Ask**. Legacy access values are migrated safely:
removed `Auto` becomes Ask, and legacy combined Plan becomes Ask plus
Interaction Plan. Users may explicitly choose Auto-accept edits or Full access
for a session. Missing fields never imply full access.

[CHANGELOG.md#124A]
PUT 17.=21:
## [unreleased]

- Use the built-in Rust HTTP harness for OpenAI Responses, OpenAI Chat Completions, and Anthropic instead of external provider CLIs
- Add official OpenAI API, ChatGPT Codex subscription, Anthropic, OpenCode Go/Zen, xAI API, and SuperGrok / X Premium+ authentication; Codex supports browser PKCE with device fallback, and xAI uses device OAuth through the system default browser
- Discover provider catalogs live with cache/seed fallback, prevent incompatible model selection, and limit typed service tier to official OpenAI API catalog entries
- Add configurable custom endpoints with explicit model IDs as a discovery-error fallback; keep API keys in the macOS Keychain and out of settings
- Remove the legacy Computer Use integration while retaining the generic Tool seam and use an append-only daemon usage ledger instead of external CLI transcript scans
- Default new sessions to Ask access and migrate legacy Auto/Plan modes safely

[CONTRIBUTING.md#8FCC]
PUT 8.=13:
The debug app requires:

- macOS or Linux (Wayland or X11)
- Rust 1.96 or newer
- Bun
- A reachable HTTP provider endpoint when testing a provider integration; no external agent CLI is required

PUT 35.=41:
On macOS the watcher builds and signs `target/debug/Waku Debug.app`; on Linux
it builds `target/debug/waku`. In both cases the provider daemon runs as the
separate `target/debug/waku-debug-daemon` process: provider-only edits rebuild
and hot-swap that process without relaunching the app, while desktop edits
rebuild and relaunch the app normally. Keep that watcher running while you
work. Do not start a second watcher or manually relaunch the debug app. Press
`Ctrl-C`, or quit the app, to stop it.

[docs/titles.md#1E4E]
PUT 1*:
# Session titles

Waku keeps session titles provider-neutral. A session starts with `New task`.
When the first prompt is submitted, Waku derives an `auto_title` from its first
seven words, capped at 54 characters. A title explicitly entered by the user
always wins; otherwise the UI shows the derived fallback or `New task`.

The wire model still accepts `DriverEvent::AutoTitleUpdated`, and a future
driver may replace the fallback with a provider-supplied title. The current
built-in HTTP harness driver does not emit provider-specific title events.

Waku does not launch title subprocesses, scan provider transcript files, read
external CLI stores, or maintain provider-specific title integrations. Title
failures therefore cannot block a turn: the local prompt fallback remains
visible until the user supplies an explicit title or a driver reports a newer
one.
