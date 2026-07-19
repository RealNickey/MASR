# GitHub Actions CI/CD rollout plan

## Summary

Finish the GitHub Actions-only pipeline on `codex/github-actions-cicd`, prove the
full `dev` validation matrix, then promote the verified work through the normal
PR flow. Keep release signing key material secret; only consider the user's
password fallback if a signed-release log demonstrates the secret is not being
passed to Tauri.

## Current context

- PR #22 (`codex/github-actions-cicd` -> `dev`) contains the new reusable
  cross-platform package workflow and release workflow.
- The default branch is currently `dev`, so the new package workflow cannot be
  manually dispatched until PR #22 is merged into `dev`. The older default
  branch "Build" workflow may still report a bootstrap failure until that merge;
  it is deleted by this PR and is not the new pipeline.
- Manual quality checks already queued during this rollout include Code quality,
  Rust tests, Playwright, and Nix. Inspect their current conclusions before any
  promotion.
- The review found real workflow hardening gaps: runner-local Cargo setup,
  literal version transport, pinned and checksum-verified downloaded tools,
  post-repack updater signing, strict release tags, least-privilege reusable
  secrets, and rerun-safe draft publishing.

## System impact

`build.yml` is the shared source of truth for both unsigned PR package
validation and signed release builds. It must set runner-only build state before
the cache/build, produce a final Linux AppImage before updater archiving and
signing, and never execute unverified downloads. `release.yml` owns version
selection and the `runtime-depot` release lifecycle, so its validation and
rerun handling must be deterministic. No application runtime behavior changes.

## Implementation steps

1. Verify every review comment against the current files; apply only confirmed
   workflow and UI cleanups, preserving unrelated user files.
2. Replace mutable/unverified ONNX Runtime and AppImageKit installation flows
   with pinned URLs plus hard-coded SHA-256 verification before extraction or
   execution. Rebuild the Linux updater archive after AppImage repacking and
   sign that final archive only for release builds.
3. Move `CARGO_TARGET_DIR` into an early runner setup step using `RUNNER_TEMP`
   and `GITHUB_ENV`; pass the isolated release version through an environment
   variable rather than interpolating it in a shell command.
4. Make release preparation strict and rerun-safe: anchored semantic tag
   validation, explicit Tauri signing-secret mappings, reuse an existing draft,
   remove a stale tag with no release, and overwrite assets only after that
   lifecycle decision.
5. Run focused local workflow/static checks, commit and push the narrow patch,
   then rerun the existing quality workflows as needed.
6. Once quality gates are green, merge PR #22 into `dev` and run the new
   unsigned Windows x64/macOS Intel/macOS Apple Silicon/Linux x64 package
   matrix. Diagnose and patch failures until all four succeed.
7. Open the normal `dev` -> `main` PR after the `dev` matrix is green. Merge
   only when its required checks pass; monitor the first signed release,
   `runtime-depot` assets, signatures, and `latest.json`. Test Windows updater
   acceptance if a Windows environment is available.

## Verification

- Static: YAML parsing, `bun run format:check`, `bun run lint`,
  `bun run check:translations`, and `bun run build`.
- CI: Rust tests, Playwright, Nix eval/build, code quality, and the four-target
  package matrix on `dev`.
- Release: inspect the `runtime-depot` release assets and generated
  `latest.json`; check that each updater URL and signature names a final,
  published artifact.

## Fresh-chat handoff

Start by reading this file, `git status --short`, PR #22 checks, and the latest
GitHub Actions runs. Do not hard-code `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
unless the signed release has a reproducible log proving GitHub did not inject
the explicitly-mapped secret. Keep the key secret in all cases.
