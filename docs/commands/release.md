# f release

Release a project based on `flow.toml` defaults or explicit subcommands.

## Usage

```bash
f release
f release registry
f release gh
```

## Registry releases

```bash
f release registry
```

### flow.toml

```toml
[release]
default = "registry"
versioning = "calver"

[release.registry]
url = "https://myflow.sh"
package = "flow"
bins = ["flow", "f", "lin"]
default_bin = "flow"
token_env = "FLOW_REGISTRY_TOKEN"
latest = true
```

### Options

- `--version <VERSION>`: publish a specific version.
- `--registry <URL>`: override the registry base URL.
- `--bin <NAME>`: override the binaries to upload (repeatable).
- `--no-build`: skip building binaries.
- `--latest` / `--no-latest`: control latest pointer updates.

## GitHub releases

```bash
f release gh
```
