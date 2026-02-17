---
title: Install Rise Via Flow Auto
description: Validate that flow install rise resolves through auto backend.
tags: [install, rise, registry, parm]
---

Install rise using Flow auto backend and print binary path.

```sh
set -euo pipefail
cd ~/code/flow
tmpdir="$(mktemp -d /tmp/flow-install-rise.XXXXXX)"
trap 'rm -rf "$tmpdir"' EXIT
f install rise --backend auto --bin-dir "$tmpdir" --force
ls -l "$tmpdir"
"$tmpdir/rise" --version || true
```
