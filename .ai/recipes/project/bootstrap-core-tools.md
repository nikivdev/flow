---
title: Bootstrap Core CLI Stack
description: Install rise, seq, and seqd via flow install auto backend.
tags: [bootstrap, install, rise, seq]
---

Bootstrap the core toolchain onto the same bin directory as `f`.

```sh
set -euo pipefail
cd ~/code/flow
bin_dir="${FLOW_BIN_DIR:-$HOME/.flow/bin}"
mkdir -p "$bin_dir"
f install rise --backend auto --bin-dir "$bin_dir" --force
f install seq --backend auto --bin-dir "$bin_dir" --force
f install seqd --backend auto --bin-dir "$bin_dir" --force
ls -la "$bin_dir" | rg ' f$| rise$| seq$| seqd$| lin$' || true
```

