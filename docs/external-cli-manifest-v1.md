# Flow External CLI Manifest v1

## Goal

Give Flow one small, language-agnostic way to execute CLIs that live outside
the Flow Rust repo.

The first target is `codex-session-browser`, but the contract is intentionally
generic so the same bridge can later execute tools under:

- `~/code/lang/go/cli/*`
- `~/code/lang/rust/cli/*`

## Discovery

Flow should discover external tools by scanning fixed roots for a manifest
named `flow-tool.toml`.

Initial roots:

- `~/code/lang/go/cli/*/flow-tool.toml`
- `~/code/lang/rust/cli/*/flow-tool.toml`

The manifest's parent directory is the tool `source_root`.

## Manifest

Example:

```toml
version = 1
id = "codex-session-browser"
language = "go"
binary_name = "codex-session-browser"
description = "Browse Codex sessions for a repo and print the selected session ID."

[exec]
run = ["go", "run", "."]
```

### Required fields

- `version`
  - integer schema version
  - v1 requires `1`
- `id`
  - stable Flow-facing tool identifier
  - must be unique across discovered manifests
- `language`
  - initial values: `go`, `rust`
- `binary_name`
  - preferred installed binary name if Flow later adds build/install support
- `[exec].run`
  - argv prefix Flow executes in `source_root`
  - Flow appends caller-provided args to this array

### Optional fields

- `description`
  - human-readable summary for help or debugging
- `[exec].build`
  - optional argv prefix for future install/build support
- `[exec].env`
  - optional environment overrides for the tool process

## Runner contract

For v1, Flow only needs this behavior:

1. discover the manifest
2. set `source_root` to the manifest parent directory
3. execute `[exec].run + passthrough_args` in `source_root`
4. capture stdout, stderr, and exit status
5. let the calling Flow command interpret stdout

This keeps the bridge generic. The bridge does not need to understand what the
tool prints.

## Caller-owned semantics

The Flow command that invokes the external tool owns the meaning of stdout.

Examples:

- `f ai codex browse` expects stdout to be the selected Codex session ID
- a future tool might print JSON to stdout instead
- another tool may be fully interactive and only use exit status

The bridge should not special-case those semantics.

## Go and Rust examples

Go:

```toml
version = 1
id = "codex-session-browser"
language = "go"
binary_name = "codex-session-browser"

[exec]
run = ["go", "run", "."]
```

Rust:

```toml
version = 1
id = "ctx"
language = "rust"
binary_name = "ctx"

[exec]
run = ["cargo", "run", "--quiet", "--"]
```

## v1 non-goals

- no plugin marketplace
- no dynamic remote installation
- no per-tool permission model
- no schema beyond local manifest discovery and argv execution
- no requirement that Flow build binaries before first use

## Initial consumer

The first tool using this contract is:

- source: `~/code/lang/go/cli/codex-session-browser`
- manifest: `~/code/lang/go/cli/codex-session-browser/flow-tool.toml`
- Flow caller: `f ai codex browse`
