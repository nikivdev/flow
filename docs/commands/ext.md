# f ext

Manage Flow config extensions under `~/.flow/extensions`.

This is the extension/operator surface for the centralized Flow config system:

- root config: `~/.flow/config.ts`
- runtime helper: `~/.flow/runtime/flow-config.ts`
- extension root: `~/.flow/extensions/<name>/`

Extensions are loaded by `f config build` / `f config apply` when their names
appear in `extensions: [...]` inside `~/.flow/config.ts`.

For backwards compatibility, `f ext <path>` still supports the older external
directory import workflow into `<project>/ext/<name>`.

## Commands

### f ext list

List built-in and filesystem extensions and whether they are enabled.

```bash
f ext list
f ext list --json
```

### f ext doctor

Show root-config wiring for extensions:

- `~/.flow/config.ts`
- `~/.flow/extensions`
- `~/.flow/runtime/flow-config.ts`

and the currently discoverable extensions.

```bash
f ext doctor
f ext doctor --json
```

### f ext init

Create a new extension scaffold under `~/.flow/extensions/<name>/`.

Generated files:

- `extension.ts`
- `README.md`

The scaffold imports:

- `defineFlowExtension` from `~/.flow/runtime/flow-config.ts`

```bash
f ext init my-extension
f ext init my-extension --force
```

### f ext enable

Enable an extension by name in `~/.flow/config.ts`.

```bash
f ext enable my-extension
```

### f ext disable

Disable an extension by name in `~/.flow/config.ts`.

```bash
f ext disable my-extension
```

### f ext import

Explicit legacy import form. Copies an external directory into `<project>/ext/`
and adds `ext/` to `.gitignore`.

```bash
f ext import ~/some/external/repo
```

Bare path form is still accepted:

```bash
f ext ~/some/external/repo
```

## Extension Shape

An extension file can export:

```ts
import { defineFlowExtension } from "../../runtime/flow-config"

export default defineFlowExtension({
  name: "my-extension",
  flow: { ... },
  lin: { ... },
  ai: { ... },
  hive: { ... }, // migration-only; Flow no longer applies ~/.hive/config.json
  zerg: { ... },
  generatedFiles: [
    {
      path: "~/.some/target/file.json",
      content: "{...}",
    },
  ],
  doctor: ["human-readable check or note"],
})
```

## Notes

- built-in extension currently supported: `lin-compat`
- `f ext enable` rewrites `~/.flow/config.ts`
- `f ext init` also ensures the runtime helper exists
- generated file path collisions fail the build
