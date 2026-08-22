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
