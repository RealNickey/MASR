# Required GitHub branch protection

Configure these repository settings before treating the beta pipeline as a
release control:

- `main`: require pull requests, require `CI/CD validation`, the
  `dev to main promotion policy` (for application changes), and `Windows x64
beta gate`, require one approving review, dismiss stale approvals, and
  disallow direct pushes (including administrators). For CI/CD-only changes,
  `CI/CD validation` and the successful lightweight replacement execution of
  `Windows x64 beta gate` are the required checks.
- `dev`: require pull requests and the lightweight `Windows x64 beta gate`
  check; pushes and PRs targeting `dev` run fast/basic validation only. The
  Linux/macOS smoke matrix is reserved for `main` promotion checks and remains
  non-blocking while it is a readiness report.
- Permit only `dev` as the source branch for a `main` promotion. The workflow
  verifies the coordinated package/Cargo/Tauri patch bump, while this setting
  provides the actual server-side direct-push protection.

GitHub branch protection is repository state rather than source code, so this
file records the required configuration for an administrator to apply.
