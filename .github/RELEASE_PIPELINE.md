# MASR release pipeline

Development lands on `dev`. Merge `dev` into `main` to start **Publish
release**. It builds Windows x64, macOS Apple Silicon, macOS Intel, and Linux
x64 from the merged MASR source, then releases the resulting assets to
`RealNickey/runtime-depot`.

Each release gets a monotonically increasing SemVer patch version from the
development baseline in `package.json` plus the GitHub Actions run number. The
workflow applies that version to `package.json`, `src-tauri/tauri.conf.json`,
and `src-tauri/Cargo.toml` inside each isolated build; it never creates a noisy
version-only commit on `main`.

After all platform builds pass, the workflow creates `latest.json` from the
generated Tauri signatures and uploads it with every installer to the depot
release. The app's existing updater endpoint then automatically discovers it.

## Required repository secrets

Configure these in `RealNickey/MASR`, never in the public depot:

| Secret                               | Purpose                                                                                            |
| ------------------------------------ | -------------------------------------------------------------------------------------------------- |
| `RUNTIME_DEPOT_TOKEN`                | Token with Contents read/write on `RealNickey/runtime-depot`; creates releases and uploads assets. |
| `TAURI_SIGNING_PRIVATE_KEY`          | Complete private updater key from `bun tauri signer generate`.                                     |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for the updater key.                                                                      |

The private key must match `plugins.updater.pubkey` in
`src-tauri/tauri.conf.json`. Apple notarization and Azure Artifact Signing are
optional—not prerequisites. Without them, macOS and Windows can show their
standard unverified-publisher prompts, but the updater remains cryptographically
verified by the Tauri signature included in `latest.json`.

## Branch policy

`dev` is the default development branch. Require the Development Build workflow
for pull requests into it. Only merge reviewed work from `dev` into `main`; a
push to `main` is a production publication.
