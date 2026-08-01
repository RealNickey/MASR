# MASR release pipeline

Development lands through reviewed pull requests into `dev`. A reviewed
`dev` → `main` pull request must make one matching patch-version bump in
`package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json`.
CI/CD-only pull requests are the exception: they do not change product
versions and therefore bypass that promotion-policy job after being classified
by the lightweight validation gate.
Opening or updating that PR runs the normal CI gates. A merge to `main` starts
the publishing workflow, which checks out and rebuilds the exact `main` SHA;
it never publishes a PR head or a reused package.

`MASR CI` runs frontend checks plus Windows x64 package, Rust tests, Malayalam
model smoke, and ONNX Runtime bundle inspection for application changes.
Linux x64 and macOS Apple Silicon report the corresponding readiness checks on
ordinary PRs. A CI/CD-only diff runs `CI/CD validation` (Prettier, actionlint,
and local-action-reference validation) and keeps the protected `Windows x64
beta gate` status successful without provisioning a Windows runner.

`Publish MASR release` runs only from `main` (or a manual recovery dispatch of
the workflow on `main`). It builds, signs, package-inspects, and smoke-tests
Windows x64, Linux x64, and macOS Apple Silicon. It publishes only the updater
assets uploaded by that exact run, then writes `runtime-depot`'s `latest.json`
from the same three signed assets. macOS Intel is deliberately not built or
claimed.

## Required repository configuration

Apply the server-side branch rules in
[BRANCH_PROTECTION.md](BRANCH_PROTECTION.md). Workflow YAML cannot itself stop
a repository administrator from directly pushing to `main`.

## Required secrets

Configure these in `RealNickey/MASR`, never in the public depot:

| Secret                               | Scope and purpose                                                                       |
| ------------------------------------ | --------------------------------------------------------------------------------------- |
| `TAURI_SIGNING_PRIVATE_KEY`          | Private updater key, available to the trusted Windows, Linux, and macOS package jobs.   |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for that key, available to the trusted Windows, Linux, and macOS package jobs. |
| `RUNTIME_DEPOT_TOKEN`                | Least-privilege token with Contents access limited to `RealNickey/runtime-depot`.       |

The private key must match `plugins.updater.pubkey` in
`src-tauri/tauri.conf.json`.

Verify that `RUNTIME_DEPOT_TOKEN` is a fine-grained token (or equivalent
machine credential) with **Contents: Read and write** access to
`RealNickey/runtime-depot` only. The release workflow uses that token only in
its post-merge publish job; dry runs cannot access it.
