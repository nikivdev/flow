#!/usr/bin/env bash
set -euo pipefail

# Project-specific trim rules for Flow vendored crates.
# Called by scripts/vendor/apply-trims.sh as: apply_vendor_trims "<crate>"

apply_reqwest_trims() {
  local file="lib/vendor/reqwest/Cargo.toml"
  [[ -f "$file" ]] || return 0

  # Keep hyper surfaces as explicit as possible; avoid implicit default feature fan-out.
  perl -0777 -i -pe '
    s/(\[target\.\x27cfg\(not\(target_arch = "wasm32"\)\)\x27\.dependencies\.hyper\]\nversion = "1\.1"\nfeatures = \[\n    "http1",\n    "client",\n\]\n)(?!default-features = false\n)/$1default-features = false\n/s;
    s/(\[target\.\x27cfg\(not\(target_arch = "wasm32"\)\)\x27\.dependencies\.hyper-util\]\nversion = "0\.1\.12"\nfeatures = \[\n    "http1",\n    "client",\n    "client-legacy",\n    "client-proxy",\n    "tokio",\n\]\n)(?!default-features = false\n)/$1default-features = false\n/s;
  ' "$file"
}

apply_axum_trims() {
  local file="lib/vendor/axum/Cargo.toml"
  [[ -f "$file" ]] || return 0

  perl -0777 -i -pe '
    s/(\[dependencies\.hyper\]\nversion = "1\.1\.0"\noptional = true\n)(?!default-features = false\n)/$1default-features = false\n/s;
    s/(\[dependencies\.hyper-util\]\nversion = "0\.1\.3"\nfeatures = \[\n    "tokio",\n    "server",\n    "service",\n\]\noptional = true\n)(?!default-features = false\n)/$1default-features = false\n/s;
  ' "$file"
}

apply_ratatui_trims() {
  local root="lib/vendor/ratatui"
  [[ -d "$root" ]] || return 0

  rm -rf \
    "$root/benches" \
    "$root/examples" \
    "$root/tests"

  rm -f \
    "$root/Cargo.lock" \
    "$root/.cz.toml" \
    "$root/.editorconfig" \
    "$root/.gitignore" \
    "$root/.markdownlint.yaml" \
    "$root/bacon.toml" \
    "$root/cliff.toml" \
    "$root/clippy.toml" \
    "$root/codecov.yml" \
    "$root/committed.toml" \
    "$root/deny.toml" \
    "$root/FUNDING.json" \
    "$root/MAINTAINERS.md" \
    "$root/RELEASE.md" \
    "$root/SECURITY.md" \
    "$root/BREAKING-CHANGES.md"

  # Rust 1.90+ warns on elided lifetime name mismatches in these signatures.
  local terminal_file="$root/src/terminal/terminal.rs"
  local text_line_file="$root/src/text/line.rs"
  local text_text_file="$root/src/text/text.rs"
  local widgets_block_file="$root/src/widgets/block.rs"

  [[ -f "$terminal_file" ]] && perl -0777 -i -pe '
    s/pub fn get_frame\(&mut self\) -> Frame \{/pub fn get_frame(&mut self) -> Frame<'\''_> {/g;
    s/pub fn draw<F>\(&mut self, render_callback: F\) -> io::Result<CompletedFrame>/pub fn draw<F>(&mut self, render_callback: F) -> io::Result<CompletedFrame<'\''_>>/g;
    s/pub fn try_draw<F, E>\(&mut self, render_callback: F\) -> io::Result<CompletedFrame>/pub fn try_draw<F, E>(&mut self, render_callback: F) -> io::Result<CompletedFrame<'\''_>>/g;
  ' "$terminal_file"

  [[ -f "$text_line_file" ]] && perl -0777 -i -pe '
    s/pub fn iter\(&self\) -> std::slice::Iter<Span<'\''a>>/pub fn iter(&self) -> std::slice::Iter<'\''_, Span<'\''a>>/g;
    s/pub fn iter_mut\(&mut self\) -> std::slice::IterMut<Span<'\''a>>/pub fn iter_mut(&mut self) -> std::slice::IterMut<'\''_, Span<'\''a>>/g;
  ' "$text_line_file"

  [[ -f "$text_text_file" ]] && perl -0777 -i -pe '
    s/pub fn iter\(&self\) -> std::slice::Iter<Line<'\''a>>/pub fn iter(&self) -> std::slice::Iter<'\''_, Line<'\''a>>/g;
    s/pub fn iter_mut\(&mut self\) -> std::slice::IterMut<Line<'\''a>>/pub fn iter_mut(&mut self) -> std::slice::IterMut<'\''_, Line<'\''a>>/g;
    s/fn to_text\(&self\) -> Text \{/fn to_text(&self) -> Text<'\''_> {/g;
  ' "$text_text_file"

  [[ -f "$widgets_block_file" ]] && perl -0777 -i -pe '
    s/\) -> impl DoubleEndedIterator<Item = &Line> \{/) -> impl DoubleEndedIterator<Item = &Line<'\''_>> {/g;
  ' "$widgets_block_file"
}

apply_crossterm_trims() {
  local root="lib/vendor/crossterm"
  [[ -d "$root" ]] || return 0

  local lib_file="$root/src/lib.rs"
  local unix_file="$root/src/terminal/sys/unix.rs"
  local filter_file="$root/src/event/filter.rs"

  [[ -f "$lib_file" ]] && perl -0777 -i -pe '
    s/\n#\[cfg\(all\(winapi, not\(feature = "winapi"\)\)\)\]\ncompile_error!\("Compiling on Windows with \\"winapi\\" feature disabled\. Feature \\"winapi\\" should only be disabled when project will never be compiled on Windows\."\);\n//g;
    s/\n#\[cfg\(all\(crossterm_winapi, not\(feature = "crossterm_winapi"\)\)\)\]\ncompile_error!\("Compiling on Windows with \\"crossterm_winapi\\" feature disabled\. Feature \\"crossterm_winapi\\" should only be disabled when project will never be compiled on Windows\."\);\n//g;
  ' "$lib_file"

  [[ -f "$unix_file" ]] && perl -0777 -i -pe '
    s/File::open\("\/dev\/tty"\)\.map\(\|file\| \(FileDesc::Owned\(file\.into\(\)\)\)\)/File::open("\/dev\/tty").map(|file| FileDesc::Owned(file.into()))/g;
  ' "$unix_file"

  [[ -f "$filter_file" ]] && perl -0777 -i -pe '
    if (!/\#\[allow\(dead_code\)\]\s*pub\(crate\) struct InternalEventFilter;/s) {
      s/\#\[derive\(Debug, Clone\)\]\s*pub\(crate\) struct InternalEventFilter;/#[derive(Debug, Clone)]\n#[allow(dead_code)]\npub(crate) struct InternalEventFilter;/s;
    }
  ' "$filter_file"
}

apply_portable_pty_trims() {
  local file="lib/vendor/portable-pty/src/unix.rs"
  [[ -f "$file" ]] || return 0

  perl -0777 -i -pe '
    s/\n[ \t]*#\[cfg_attr\(feature = "cargo-clippy", allow\(clippy::unnecessary_mut_passed\)\)\]//g;
    s/\n[ \t]*#\[cfg_attr\(feature = "cargo-clippy", allow\(clippy::cast_lossless\)\)\]//g;
  ' "$file"
}

apply_x25519_dalek_trims() {
  local file="lib/vendor/x25519-dalek/src/lib.rs"
  [[ -f "$file" ]] || return 0

  perl -0777 -i -pe '
    s/\n#!\[cfg_attr\(feature = "bench", feature\(test\)\)\]//g;
  ' "$file"
}

apply_vendor_trims() {
  local crate="${1:-}"
  if [[ -n "$crate" ]]; then
    case "$crate" in
      reqwest) apply_reqwest_trims ;;
      axum) apply_axum_trims ;;
      ratatui) apply_ratatui_trims ;;
      crossterm) apply_crossterm_trims ;;
      portable-pty) apply_portable_pty_trims ;;
      x25519-dalek) apply_x25519_dalek_trims ;;
      *) ;;
    esac
    return
  fi

  apply_reqwest_trims
  apply_axum_trims
  apply_ratatui_trims
  apply_crossterm_trims
  apply_portable_pty_trims
  apply_x25519_dalek_trims
}
