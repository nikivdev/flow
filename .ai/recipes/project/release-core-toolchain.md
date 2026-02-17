---
title: Release Core Toolchain
description: Publish flow, rise, and seq/seqd to myflow registry.
tags: [release, registry, flow, rise, seq]
---

```sh
set -euo pipefail
cd ~/code/flow
f recipe run project:release-flow-registry
f recipe run project:release-rise-registry
f recipe run project:release-seq-registry
```
