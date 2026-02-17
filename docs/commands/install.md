# f install

Install a CLI/tool binary into your PATH.

Default backend behavior (`--backend auto`):
1. Flow registry (`myflow.sh` or `--registry`)
2. GitHub releases via `parm`
3. flox package install

## Usage

```bash
f install <name>
```

## Options

- `--registry <URL>`: registry base URL (defaults to `FLOW_REGISTRY_URL`).
- `--backend <auto|registry|parm|flox>`: choose install backend explicitly.
- `--version <VERSION>`: install a specific version (defaults to latest).
- `--bin <NAME>`: binary name to install (defaults to manifest default or package name).
- `--bin-dir <PATH>`: install directory (defaults to `~/bin`).
- `--force`: overwrite an existing binary.
- `--no-verify`: skip checksum verification.

## Auto backend notes

- `f install rise` can resolve through `parm` using built-in owner/repo mapping.
- If a package name is ambiguous for `parm`, set `FLOW_INSTALL_OWNER` (env or Flow personal env store) or pass `owner/repo` directly.

## Registry layout

The registry must expose:

- `GET /packages/<name>/latest.json`
- `GET /packages/<name>/<version>/manifest.json`
- `GET /packages/<name>/<version>/<target>/<bin>`

## Example

```bash
FLOW_REGISTRY_URL=https://myflow.sh f install flow
```

```bash
f install rise
```
