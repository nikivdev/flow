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
- Built-in aliases:
  - `f install seqd` resolves registry package `seq` with binary `seqd`.
  - `f install lin` resolves registry package `flow` with binary `lin`.
- If a package name is ambiguous for `parm`, set `FLOW_INSTALL_OWNER` (env or Flow personal env store) or pass `owner/repo` directly.

## Bootstrap from installer

The hosted installer can bootstrap core tools after installing flow:

- `FLOW_BOOTSTRAP_TOOLS="rise seq seqd"` (default) installs those with `f install ... --backend auto`.
- `FLOW_BOOTSTRAP_TOOLS=0` disables this.
- `FLOW_BOOTSTRAP_INSTALL_PARM=1` (default) attempts to install `parm` for robust GitHub fallback.

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
