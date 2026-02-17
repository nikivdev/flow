---
title: Release Seq+Seqd To Registry
description: Stage seq/seqd binaries and publish package seq to myflow registry.
tags: [release, registry, seq]
---

```sh
set -euo pipefail
cd ~/code/seq
f release-registry-stage
f release registry --no-build
```
