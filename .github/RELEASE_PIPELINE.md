# MASR Windows-first beta release pipeline

Development lands through reviewed pull requests into `dev`. A reviewed
`dev` → `main` pull request must make one matching patch-version bump in
`package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json`.
Merging that PR publishes the exact committed source version.

`MASR CI` makes the Windows x64 package, Rust tests, Malayalam model smoke, and
ONNX Runtime bundle inspection blocking checks for `dev` and for promotion.
Linux x64 and macOS Apple Silicon run the same Malayalam CPU/model/package
checks as reported, non-blocking readiness jobs. They do not publish public
assets during this beta. macOS Intel is deliberately not built or claimed.

The `Publish Windows beta` workflow only runs for a merged `dev` → `main` PR in
`RealNickey/MASR`. It is the only workflow that receives updater-signing or
runtime-depot credentials. It publishes an updater manifest containing only the
Windows x64 asset.

## Required repository configuration

Apply the server-side branch rules in
[BRANCH_PROTECTION.md](BRANCH_PROTECTION.md). Workflow YAML cannot itself stop
a repository administrator from directly pushing to `main`.

## Required secrets

Configure these in `RealNickey/MASR`, never in the public depot:

| Secret                               | Scope and purpose                                                                 |
| ------------------------------------ | --------------------------------------------------------------------------------- |
| `TAURI_SIGNING_PRIVATE_KEY`          | Private updater key, available only to the trusted Windows release job.           |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for that key, available only to the trusted Windows release job.         |
| `RUNTIME_DEPOT_TOKEN`                | Least-privilege token with Contents access limited to `RealNickey/runtime-depot`. |

The private key must match `plugins.updater.pubkey` in
`src-tauri/tauri.conf.json`.
