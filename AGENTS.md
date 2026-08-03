# AGENTS.md

## Scope

MASR is a cross-platform Tauri 2 desktop app for transcription and meeting workflows. It supports Windows, macOS, and Linux. Treat the current implementation, build configuration, and CI workflows as the source of truth; older Handy-era documentation is historical context only.

Before changing a feature, inspect its existing end-to-end path. Preserve unrelated worktree changes.

## Development and verification

Use Bun for JavaScript tooling. See [BUILD.md](BUILD.md) before platform-specific build work.

```bash
# Install and run
bun install
bun run tauri dev

# Frontend-only development and build
bun run dev
bun run build

# Build the desktop application
bun run tauri build

# Targeted quality checks
bun run lint
bun run format:check
bun run test:playwright
cd src-tauri && cargo test
```

If macOS development fails on CMake policy compatibility, run:

```bash
CMAKE_POLICY_VERSION_MINIMUM=3.5 bun run tauri dev
```

Run the smallest relevant existing checks first. A local toolchain prerequisite failure should be reported distinctly from a product failure.

## Repository map

Backend paths below are relative to `src-tauri/src/`; frontend paths begin with `src/`.

| Workstream                  | Read together                                                                                                                            | Keep intact                                                                      |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| App lifecycle and windows   | `src-tauri/src/lib.rs`, `vite.config.ts`, `src-tauri/capabilities/`                                                                      | Tauri-managed state, registered commands/events, and capability permissions      |
| Settings                    | `src-tauri/src/settings.rs`, Rust update command, `src/bindings.ts`, `src/stores/settingsStore.ts`, settings UI                          | Persisted schema, migrations, and frontend/backend parity                        |
| Recording and transcription | `src-tauri/src/actions.rs`, `transcription_coordinator.rs`, `managers/audio.rs`, `managers/model.rs`, `managers/transcription.rs`        | Central `transcribe`/`meeting`/idle coordination and model lifecycle             |
| Meetings and history        | `managers/meeting_assistant.rs`, `meeting_capture.rs`, `history.rs`, `src/components/settings/meetings/`, `src/primary/MeetingsView.tsx` | History compatibility, meeting prompt flow, and background local-file processing |
| Google features             | `managers/google_oauth.rs`, `google_api.rs`, `commands/google.rs`, meeting settings UI                                                   | Independent Gmail/Tasks and Calendar authorization                               |
| LLM output                  | `settings.rs`, `actions.rs`, `llm_client.rs`, post-processing settings UI                                                                | Credential handling, provider fallback, and request quota semantics              |
| MCP and memory              | `managers/mcp_server.rs`, `rag.rs`, `commands/mcp.rs`, `docs/mcp.md`                                                                     | Loopback-only access, bearer-token protection, and opt-in local indexing         |

`src/bindings.ts` is generated through the existing `tauri-specta` export path. Change the Rust contract first, then regenerate; do not hand-maintain generated bindings.

## Multi-window architecture

Vite and Tauri define four webviews:

| Entry                           | Window label        | Responsibility                           |
| ------------------------------- | ------------------- | ---------------------------------------- |
| `index.html`                    | `main`              | Settings and onboarding                  |
| `src/primary/index.html`        | `primary`           | Everyday Meetings and Transcription view |
| `src/overlay/index.html`        | `recording_overlay` | Recording controls                       |
| `src/meeting_prompt/index.html` | `meeting_prompt`    | Detected/upcoming meeting prompt         |

The `primary` and `main` windows are intentionally separate: the primary window is the daily surface, while configuration remains in the settings window. Do not merge them or move the prompt/overlay into either window without an explicit redesign.

For a new regular window, update all of the following:

1. Vite input in `vite.config.ts`
2. Tauri creation/lifecycle in `src-tauri/src/lib.rs`
3. `src-tauri/capabilities/default.json` and `src-tauri/capabilities/desktop.json`
4. Its stylesheet so Tailwind/theme tokens are loaded when needed

## Change rules

### Settings and persisted data

For a new setting, update the persisted type/default/migration, its Rust command, the Specta binding, `settingsStore`, the UI, and tests. Keep runtime CLI overrides non-persistent; `cli.rs` and `signal_handle.rs` route external triggers through the transcription coordinator.

History is durable SQLite data. When changing stored history, recordings, summaries, or vector data, add a backward-compatible migration in the existing history path and cover existing-data behavior.

### Models, recording, and meetings

Model catalog or inference changes usually span both `managers/model.rs` and `managers/transcription.rs`, plus UI and bindings. Preserve the existing shortcut/action/coordinator path instead of adding independent recording flags.

Meeting features reuse the audio, history, and meeting-assistant managers. Multi-track meeting-capture storage is a persistence foundation, not proof of native system-audio capture on every platform; implement and verify platform adapters before claiming that behavior.

Google authorization is feature-scoped:

- `gmail_tasks` handles follow-up email and task creation.
- `calendar` handles meeting prompts.

Disconnecting one feature must not disconnect the other.

### Providers, credentials, MCP, and RAG

Saved provider credentials override bundled environment defaults. Keep secrets in the credential-vault path and never expose them through frontend settings responses.

The app-provided-key quota is 20 requests/day, persisted user-wide, and consumed once per logical LLM action—not once per retry or fallback. BYOK credentials are not subject to that app-provided-key quota.

MCP must remain bound to `127.0.0.1` and bearer-token protected. Vector memory is opt-in, stores its index locally, and uses the user's configured Google/Gemini BYOK key; it must not silently fall back to app/environment keys.

## Platform contract

Keep all three desktop platforms supported. Use existing platform guards and validate the affected platform rather than making broad assumptions.

- Windows contains the Malayalam ASR path and active-window meeting detection.
- macOS has permission and Apple Intelligence availability gates.
- Linux has platform-specific overlay and typing-tool behavior.

Use native Tauri APIs for desktop capabilities, permission flows, and system dialogs when the feature already has a native path.

## Tests, CI, and releases

Add or update the relevant existing Playwright coverage for user-visible behavior; reuse the established Tauri/mock helpers rather than inventing parallel test plumbing.

For application changes, CI expects frontend quality/build checks and Windows package validation, with Linux and macOS readiness coverage. Workflow-only changes use the lightweight CI/CD validation path; preserve its required status behavior.

Release changes are stricter:

- A `dev` → `main` promotion requires matching version bumps in `package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json`.
- Releases from `main` validate Windows x64, Linux x64, and macOS Apple Silicon packages, signatures, installed payloads, and the updater manifest.
- Do not weaken the runtime-depot asset or `latest.json` checks when editing release workflows.

## Git and external actions

- Keep commits focused and use conventional prefixes such as `feat:`, `fix:`, `docs:`, `refactor:`, or `chore:`.
- Never push to or open a pull request against `cjpais/Handy`; use `RealNickey/MASR`.
- Before creating a pull request, issue, or discussion, read the applicable repository template and follow it exactly.
