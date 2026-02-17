---
title: Release Flow To Registry
description: Publish flow to myflow registry with explicit version.
tags: [release, registry, myflow]
---

Cut a new registry release for flow.

```sh
set -euo pipefail
export FLOW_REGISTRY_URL="${FLOW_REGISTRY_URL:-https://myflow.sh}"
cd ~/code/flow
if [ -n "${FLOW_VERSION:-}" ]; then
  f release registry --version "$FLOW_VERSION" --registry "$FLOW_REGISTRY_URL"
else
  f release registry --registry "$FLOW_REGISTRY_URL"
fi
```
