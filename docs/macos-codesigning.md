# macOS Codesigning

All projects (Flow, Seq, Lin, Rise) sign binaries with a Developer ID Application certificate. This makes macOS TCC grants (Accessibility, Screen Recording, Input Monitoring) survive rebuilds and work for daemon processes.

## Why this matters

macOS ties TCC permission grants to code signatures. When a binary is ad-hoc signed (`--sign -`) or signed with an Apple Development certificate, the signature changes on every rebuild. That means:

- Accessibility grants break after every `f deploy`
- Input Monitoring grants break after every rebuild
- Screen Recording grants break after every rebuild
- Daemon processes (like `seqd`) that aren't launched from a terminal never inherit terminal TCC grants at all

Developer ID Application signing solves all of this. The identity is stable across rebuilds, so TCC grants persist.

## How it works

### Shared helper: `~/.config/flow/codesign.sh`

All projects source a single shared script that handles identity detection:

```bash
source "${HOME}/.config/flow/codesign.sh"
flow_codesign /path/to/binary
```

The helper resolves the signing identity in this order:

1. `$FLOW_CODESIGN_IDENTITY` env var (explicit override)
2. `Developer ID Application` from the keychain
3. `Apple Development` from the keychain (fallback)
4. Skip silently (no certificate available)

The identity is resolved once per shell session (first `source` call queries the keychain, subsequent calls reuse the cached value). Signing failure never breaks a build - all calls are best-effort.

### Per-project integration

**Flow** (`scripts/deploy.sh`)
- Signs `f` and `lin` binaries after `cargo build`, before copying to `~/bin`
- Since copies are made from the signed source binaries, all installed copies (`~/bin/f`, `~/bin/lin`, `~/.local/bin/f`, etc.) carry the signature

**Seq** (`cli/cpp/run.sh`)
- Signs `seq` binary after clang build
- Signs `libseqmem.dylib` if present (Swift memory engine)
- Signs the `SeqDaemon.app` bundle (required for daemon TCC grants)
- Respects `$SEQ_CODE_SIGN_IDENTITY` override (maps to `$FLOW_CODESIGN_IDENTITY` internally)

**Lin** (`mac/flow.toml`)
- `release-mac` task: sources the helper, signs `/Applications/Lin.app` with `--deep` (required for `.app` bundles)
- `r` task (quick incremental build): same logic, falls back to ad-hoc if no identity found

**Rise** (`scripts/deploy-cli.sh`, `ffi/cpp/build.sh`)
- `deploy-cli.sh`: signs `rise-bin` after copying to `~/.local/bin` and `~/bin`
- `build.sh`: signs `librise.dylib` after clang build

## Verifying signatures

Check that a binary is signed with Developer ID:

```bash
codesign -dvv /path/to/binary 2>&1 | grep Authority
```

Expected output:

```
Authority=Developer ID Application: Nikita Voloboev (6J8K2M6486)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
```

Quick verification commands for each project:

```bash
# Flow
codesign -dvv ~/bin/f 2>&1 | grep Authority

# Seq
codesign -dvv ~/code/seq/cli/cpp/out/bin/seq 2>&1 | grep Authority
codesign -dvv ~/code/seq/cli/cpp/out/bin/SeqDaemon.app 2>&1 | grep Authority

# Lin
codesign -dvv /Applications/Lin.app 2>&1 | grep Authority

# Rise
codesign -dvv ~/.local/bin/rise-bin 2>&1 | grep Authority
```

If you see `Authority=Apple Development` instead of `Developer ID Application`, the Developer ID certificate is not in the keychain. If you see no Authority line or `(code or signature modified)`, the binary was modified after signing.

## Troubleshooting

### TCC grants break after rebuild

1. Verify the binary is signed: `codesign -dvv <binary> 2>&1 | grep Authority`
2. If unsigned or ad-hoc, check that `~/.config/flow/codesign.sh` exists and is sourced by the build script
3. Check that the Developer ID certificate is in the keychain: `security find-identity -p codesigning -v | grep "Developer ID"`
4. Rebuild and verify again

### "Developer ID Application" not found

The certificate must be installed in the login keychain. If you have it in a `.p12` file:

```bash
security import developer-id.p12 -k ~/Library/Keychains/login.keychain-db -T /usr/bin/codesign
```

If you only have an Apple Development certificate, the helper falls back to it. TCC grants will still work but may be less stable across Xcode updates.

### seqd loses Accessibility after rebuild

The daemon runs inside `SeqDaemon.app` specifically so it gets its own TCC entry. After rebuilding:

1. Verify: `codesign -dvv ~/code/seq/cli/cpp/out/bin/SeqDaemon.app 2>&1 | grep Authority`
2. If correct, the old TCC grant should still apply. If not, re-grant in System Settings > Privacy & Security > Accessibility
3. Test: `printf 'AX_STATUS\n' | nc -U /tmp/seqd.sock` should print `1`

### Lin loses Screen Recording after rebuild

1. Verify: `codesign -dvv /Applications/Lin.app 2>&1 | grep Authority`
2. If signed with Developer ID, the TCC grant should persist. If it doesn't, reset and re-grant:
   ```bash
   tccutil reset ScreenCapture io.linsa
   ```
   Then reopen Lin and grant Screen Recording again.

### Overriding the identity

Set `$FLOW_CODESIGN_IDENTITY` before building to use a specific certificate:

```bash
export FLOW_CODESIGN_IDENTITY="Developer ID Application: Someone Else (XXXXXXXXXX)"
f deploy
```

For Seq specifically, `$SEQ_CODE_SIGN_IDENTITY` also works (it takes precedence).

### Checking what certificates are available

```bash
security find-identity -p codesigning -v
```

This lists all valid codesigning identities in your keychains. The helper picks the first `Developer ID Application` match, or the first `Apple Development` match if no Developer ID is available.

## Architecture notes

- The shared helper lives in `~/.config/flow/` (Flow's config directory) because it's a cross-project concern managed by Flow
- `.app` bundles (SeqDaemon.app, Lin.app) require `--deep` flag to sign embedded frameworks and executables
- Signing happens on the source binary before copies are made, so all install locations get the signed version
- The helper is sourced with `2>/dev/null || true` so missing file never breaks builds (e.g. on Linux or CI where no keychain exists)
