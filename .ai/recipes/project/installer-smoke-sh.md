---
title: Installer Smoke Via sh
description: Smoke test myflow installer snippet with pipe-to-sh entrypoint.
tags: [install, smoke, sh]
---

Run the hosted installer in sh mode (supports curl | sh bootstrap).

```sh
set -euo pipefail
curl -fsSL https://myflow.sh/install.sh | sh
```
