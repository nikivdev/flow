# f install

Download a binary from a Flow registry and install it into your PATH.

## Usage

```bash
f install <name>
```

## Options

- `--registry <URL>`: registry base URL (defaults to `FLOW_REGISTRY_URL`).
- `--version <VERSION>`: install a specific version (defaults to latest).
- `--bin <NAME>`: binary name to install (defaults to manifest default or package name).
- `--bin-dir <PATH>`: install directory (defaults to `~/bin`).
- `--force`: overwrite an existing binary.
- `--no-verify`: skip checksum verification.

## Registry layout

The registry must expose:

- `GET /packages/<name>/latest.json`
- `GET /packages/<name>/<version>/manifest.json`
- `GET /packages/<name>/<version>/<target>/<bin>`

## Example

```bash
FLOW_REGISTRY_URL=https://myflow.sh f install flow
```
