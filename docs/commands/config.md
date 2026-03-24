# f config

Compile the personal Flow root config and generated compatibility outputs.

Source of truth:

- `~/.flow/config.ts`

Generated outputs:

- `~/.config/flow/generated/config.snapshot.json`
- `~/.config/flow/generated/flow/config.ts`
- `~/.config/flow/generated/lin/config.ts`
- `~/.config/flow/generated/lin/intents.json`
- `~/.config/flow/generated/lin/mac.toml`
- `~/.config/flow/generated/ai/config.json`
- `~/.config/flow/generated/hive/config.json`

If `~/.flow/config.ts` does not exist yet, Flow currently falls back to:

- `~/config/i/lin/config.ts`

and generates a starter `~/.flow/config.ts` migration file on apply.

## Commands

### f config doctor

Shows whether the root config exists, whether a TypeScript runner is available,
whether a generated snapshot already exists, and what extension directories are discoverable.

```bash
f config doctor
f config doctor --json
```

### f config build

Loads the root config plus named extensions, computes the merged effective config,
and writes generated artifacts under `~/.config/flow/generated/`.

This does not overwrite consumer config locations.

```bash
f config build
f config build --json
```

### f config apply

Builds the snapshot and then applies the generated compatibility files into their
consumer locations. This is the cutover step that keeps existing tools working while
moving authoring into `~/.flow/config.ts`.

```bash
f config apply
f config apply --json
```

### f config eval

Prints the currently effective merged config view. If no snapshot exists yet, Flow
builds one first.

```bash
f config eval
f config eval --json
```

## Root Config Shape

The first supported root-config shape is:

```ts
export default {
  flow: { ... },
  lin: { ... },
  ai: { ... },
  hive: { ... },
  zerg: { ... },
  extensions: ["lin-compat"],
}
```

Filesystem extensions can live under:

- `~/.flow/extensions/<name>/extension.ts`
- `~/.flow/extensions/<name>/config.ts`

Each extension can contribute partial config fragments plus optional generated files.

## Notes

- Flow currently ships one built-in extension name: `lin-compat`
- explicit extensions must exist or `f config build` fails
- generated outputs are written atomically
