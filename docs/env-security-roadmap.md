# Env Security Roadmap

This document defines the hardening path for Flow env storage so it is usable in
large orgs with strict secret-handling rules.

## Current State

- `f env set KEY=VALUE` writes to personal scope.
- `f env project set KEY=VALUE -e <env>` writes to project scope.
- Personal cloud env values are still stored in Flow's server-managed secret
  store and fetched over authenticated API calls.
- Project cloud env values are now sealed client-side before upload and
  decrypted locally after fetch.
- Project cloud reads auto-register the local device sealer when needed.
- Legacy plaintext project cloud values are still read as a compatibility
  fallback during migration.
- Local env values live under `~/.config/flow/env-local/`.
- On macOS, personal local env values are now stored in Keychain by default and
  Flow keeps only local references on disk.
- Project-local envs still use private `.env` files on disk because apps and
  deploy flows often need direct file materialization.
- Host deploys that still fetch project envs via service tokens keep an
  explicit plaintext cloud mirror until the host fetch path is upgraded.

## Security Goals

- No tracked repo file should ever be required for secret storage.
- Secret values should be encrypted or OS-protected at rest by default.
- Metadata should be separable from secret material.
- Cloud sharing must not require trusting the server with plaintext values.
- Reads should be auditable and scoped.
- Rotation and revocation must be first-class.

## Secret Model

Use three classes:

1. Secret value
   Examples: API keys, signing material, service tokens.
2. Sensitive metadata
   Examples: project IDs, team IDs, URLs that should not be public broadly.
3. Non-secret metadata
   Examples: team key `IDE`, environment name, feature flags safe to commit.

Rule:
- Secret values belong in Flow env storage.
- Non-secret metadata should prefer checked-in config.
- Sensitive metadata can live in Flow env storage if the repo should not carry it.

## Immediate Policy

For a Linear integration:

- `DESIGNER_LINEAR_API_KEY` is a secret and should stay in Flow personal env
  storage.
- `DESIGNER_LINEAR_TEAM_KEY=IDE` is not a secret and should move into forge
  config when the integration is wired.

## Phase 0

- Fix CLI/docs mismatches so users do not get fake command examples.
- Make personal vs project scope explicit in docs and examples.
- Enforce private local file permissions in code.

## Phase 1

- Use OS secure storage for local personal secrets by default.
- Keep only local references or metadata on disk.
- Add read gating for local secure-secret reads on macOS.
- Add migration for legacy plaintext personal local env files.

## Phase 2

- Add explicit secret classification:
  - `secret`
  - `sensitive`
  - `public`
- Store descriptions/metadata separately from values.
- Add a local inspection command that shows where each key is stored without
  printing the value.

## Phase 3

Completed for project envs:
- client-side envelope encryption for cloud-shared project values
- reuse of Flow's existing sealing primitives used by SSH key storage
- device/user recipient fanout based on registered project sealers
- ciphertext-only project env storage on the server

Still open:
- group recipients
- richer classification/policy enforcement at write time
- better device recovery/re-share workflows
- eliminating the temporary plaintext compatibility mirror for service-token
  host fetches

## Phase 4

- Add org-grade controls:
  - scoped service tokens
  - access logs
  - rotation workflows
  - revocation
  - break-glass recovery
  - policy checks for forbidden repo-local secret paths

## Constraints

- Project envs that must become `.env` files for local runtime or deploys still
  need a materialization path.
- Cloud sharing security should not regress existing deploy workflows.
- Compatibility escape hatches are acceptable, but secure defaults must win.
