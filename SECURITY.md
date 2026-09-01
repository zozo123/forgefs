# Security Policy

ForgeFS is security-sensitive infrastructure. Please do not open public issues for suspected vulnerabilities that could expose data, bypass capabilities, corrupt repository state, or weaken sealed verification.

## Reporting

Use GitHub's private security advisory / vulnerability reporting flow for this repository when available. Include:

- affected commit or release;
- minimal reproduction steps;
- expected vs. observed security boundary;
- impact and attacker prerequisites;
- any proposed fix or test, if known.

## Supported versions

Until the first stable release, only the current `main` branch is supported for security fixes.

## Security invariants

Security fixes must preserve ForgeFS's core guarantees: immutable content-addressed bytes, monotonic capability attenuation, snapshot-consistent agent work, explicit conflicts, fail-closed verification, and trusted sealed releases.

## Dependency upgrades

Treat major dependency upgrades on the trusted path as security-sensitive changes, not routine batch maintenance.

- Land at most one major crate upgrade per pull request and rebase it onto current green `main` before evaluation.
- Exception, for crates whose traits are too coupled to compile apart: bump the set in one pull request and state the coupling in the description. `sha2` and `hmac` must agree on a single `digest` major version, so bumping either alone fails to satisfy `Sha256: hmac::digest::core_api::CoreProxy`; #401 moved both together and superseded the split Dependabot PRs #397 and #398. Such a set is still held to the review bar of a single major upgrade.
- Require the workspace tests with the committed lockfile plus clippy with warnings denied; the repository's Rust, MSRV, and e2e checks must be green before merge.
- Review security/correctness call sites affected by API changes, especially CAS/ref updates, capability verification, identifiers/randomness, object encoding, and metadata durability.
- Never merge several independently green lockfile pull requests without revalidating their combined current-`main` merge result.
- If a generated lockfile merge conflicts or becomes invalid, recreate it from the manifests rather than hand-splicing dependency records.
- Workflow/action-only upgrades may proceed separately when their CI is green.
