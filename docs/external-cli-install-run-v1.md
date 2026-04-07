 # Flow External CLI Install + Run v1

 ## Goal

 Make external language-specific CLIs feel native in Flow.

 The user-facing shape is:

 - `f cli <id> [-- <args>...]`
 - `f install <path-to-cli>`
 - `f install <remote-cli-uuid>`

 This should sit on top of the existing local manifest bridge, not replace it.

 Current grounding:

 - `src/external_cli.rs` already discovers local manifests from fixed roots.
 - `src/install.rs` and `src/main.rs` already give Flow one install entrypoint.
 - `src/config.rs` already defines the global Flow config/state root under `~/.config/flow`.

 ## Design principles

 - `f install` is the intake path.
 - `f cli` is the resolver and runner.
 - Human-facing invocation should use a stable tool id like `codex-session-browser`.
 - Remote installation should use an immutable install identifier, not a mutable human alias.
 - Local path installs should link to source, not copy it.
 - Existing package install behavior for registry, parm, and flox must keep working.

 ## Proposed user surface

 ### Run a CLI

 ```sh
 f cli codex-session-browser -- browse --repo ~/repos/openai/codex
 ```

 Rules:

 - `f cli <id>` resolves a registered or discovered external CLI by `id`.
 - Everything after `--` is passed directly to the tool.
 - Flow does not reinterpret tool-specific args.

 Add small management helpers:

 - `f cli list`
 - `f cli which <id>`
 - `f cli doctor <id>`

 ### Install from a local path

 ```sh
 f install ~/code/lang/go/cli/codex-session-browser
 ```

 Rules:

 - If the argument is an existing directory or manifest path, Flow treats it as an external CLI source install.
 - Flow validates `flow-tool.toml`.
 - Flow creates a registration record that points at the source directory.
 - Flow does not copy source into Flow-managed storage for this mode.

 This is effectively `link` mode, but the user does not need a separate command.

 ### Install from a remote UUID

 ```sh
 f install cli_01jv...
 ```

 Rules:

 - If the argument matches Flow's remote CLI install id format, Flow resolves it through a remote CLI catalog.
 - Flow downloads a source snapshot or release bundle into Flow-managed storage.
 - Flow registers the installed tool under its manifest `id`.

 I would not make bare UUIDs the only format. A prefix such as `cli_<id>` or `cli:01jv...` is safer because `f install` already accepts normal package names.

 ## Naming model

 Use two identifiers with different jobs.

 ### 1. `id`

 Human-facing, stable callable name from `flow-tool.toml`.

 Example:

 ```toml
 id = "codex-session-browser"
 ```

 This is what `f cli` uses.

 ### 2. `install_id`

 Immutable remote catalog id.

 Example:

 - `cli_01jv8n...`

 This is what `f install` uses for remote installs.

 Why both:

 - the callable id should stay readable
 - the remote catalog needs a collision-proof immutable identity
 - the tool can be renamed in UX without breaking historic install references

 ## Resolution order for `f cli`

 `f cli <id>` should resolve in this order:

 1. registered local links
 2. registered remote installs
 3. dev-mode discovery roots from `src/external_cli.rs`

 That gives two important behaviors:

 - explicitly installed tools win
 - tools under `~/code/lang/go/cli/*` and `~/code/lang/rust/cli/*` still work during development without an install step

 ## Storage layout

 Use Flow's global state/config root.

 Suggested layout:

 - `~/.config/flow/cli/links/<id>.toml`
 - `~/.config/flow/cli/installs/<install_id>/...`
 - `~/.config/flow/cli/index.json`

 ### Link record

 A link record should contain:

 - `id`
 - `source_root`
 - `manifest_path`
 - `installed_at`
 - optional `install_id`
 - optional `description`

 This is the record created by `f install <path>`.

 ### Remote install record

 A remote install directory should contain:

 - downloaded source or unpacked bundle
 - a copy of the resolved manifest
 - install metadata
 - version / published-at metadata

 This is the record created by `f install <remote-cli-uuid>`.

 ## Manifest contract

 Keep `flow-tool.toml` as the tool-owned contract.

 Current useful fields already exist:

 ```toml
 version = 1
 id = "codex-session-browser"
 language = "go"
 binary_name = "codex-session-browser"
 description = "Browse Codex sessions for a repo and print the selected session ID."

 [exec]
 run = ["go", "run", "."]
 ```

 For this install/run design, the manifest should remain source-oriented.

 The install registry should not redefine execution. It should point back to the manifest.

 ### Optional future fields

 Not required for v1, but reasonable later:

 - `aliases = ["csb"]`
 - `min_flow_version = "0.1.0"`
 - `[exec].build`
 - `[install]` metadata for remote packaging hints

 ## `f install` decision tree

 `f install <arg>` should behave like this:

 1. if `<arg>` is an existing path:
    - install as external CLI link
 2. else if `<arg>` matches remote CLI install id format:
    - install from remote CLI catalog
 3. else:
    - keep existing install behavior (`registry` / `parm` / `flox` auto resolution)

 This preserves the current tool/package install surface while adding the new CLI path cleanly.

 ## Flow implementation shape

 ### 1. Generalize `external_cli.rs`

 Evolve it from pure root scanning into a resolver module that can read:

 - registrations under `~/.config/flow/cli/...`
 - current dev roots

 Suggested API shape:

 - `resolve_external_cli_tool(id: &str) -> Result<ResolvedExternalCliTool>`
 - `list_external_cli_tools() -> Result<Vec<ResolvedExternalCliTool>>`
 - `install_external_cli_link(path: &Path) -> Result<InstalledCliRecord>`
 - `install_external_cli_remote(install_id: &str) -> Result<InstalledCliRecord>`

 ### 2. Add a top-level `Cli` command

 Add a new command family:

 - `f cli <id> [-- <args>...]`
 - `f cli list`
 - `f cli which <id>`
 - `f cli doctor <id>`

 `f ai codex browse` should then stop owning raw manifest resolution and instead call the same resolver/runner underneath.

 ### 3. Extend `f install`

 Do not create a second install command.

 Keep `f install` as the front door, but teach `install::run` to detect:

 - local path installs
 - remote CLI install ids

 That can be implemented either as:

 - a new `InstallBackend::Cli`, plus auto-detection
 - or a pre-backend dispatch before current backend selection

 I prefer pre-backend dispatch because local paths and remote CLI ids are not really "backends" in the same sense as flox or parm.

 ## Remote catalog shape

 Do not overload the existing binary registry manifest in `src/registry.rs`.

 That registry is target/binary oriented. External CLIs are source/manifest oriented.

 Use a separate remote catalog document for CLIs.

 Suggested remote record:

 ```json
 {
   "install_id": "cli_01jv8n...",
   "id": "codex-session-browser",
   "version": "0.1.0",
   "published_at": "2026-04-02T12:00:00Z",
   "source": {
     "kind": "tarball",
     "url": "https://.../codex-session-browser.tgz",
     "sha256": "..."
   },
   "manifest": {
     "version": 1,
     "id": "codex-session-browser",
     "language": "go",
     "binary_name": "codex-session-browser",
     "exec": {
       "run": ["go", "run", "."]
     }
   }
 }
 ```

 The important boundary is:

 - binary registry: compiled artifacts by target triple
 - CLI catalog: manifest-driven source tools that Flow knows how to run

 ## Conflict rules

 If `f install <path>` tries to register an `id` that already exists:

 - same `source_root`: report already installed
 - different `source_root`: fail unless `--force` or an explicit replace flag is passed

 If a remote install provides an `id` that already exists:

 - prefer the installed record marked active
 - keep the conflict explicit in `f cli list`
 - do not silently shadow an existing tool

 ## Why this is better than the current special-case bridge

 Current state is enough for development, but not for ownership.

 The missing pieces today are:

 - no first-class installed-tool registry
 - no single `f cli` runner surface
 - no path-link install contract
 - no remote install contract

 This design fills those gaps without throwing away the working manifest bridge.

 ## Recommended rollout

 ### Phase 1

 - keep current root scanning
 - add `f cli <id>` on top of `external_cli.rs`
 - add `f install <path>` as a link registration
 - add `f cli list` and `f cli which`

 ### Phase 2

 - migrate `f ai codex browse` to use the shared `f cli` resolver path
 - keep direct resolver calls only as library reuse under the same module

 ### Phase 3

 - add remote CLI catalog
 - support `f install <remote-cli-uuid>`

 ### Phase 4

 - optional `f cli upgrade`, `f cli uninstall`, and alias support

 ## Recommendation

 Yes, this should go through Flow.

 But the right shape is:

 - `f install` registers or fetches tools
 - `f cli` runs them
 - `flow-tool.toml` stays the source-of-truth execution contract
 - remote CLI install ids are immutable and distinct from callable tool ids

 That gives Flow a clean tool story without collapsing local source tools, binary package installs, and remote catalog installs into one ambiguous mechanism.
