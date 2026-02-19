# db

Manage database workflows and backends (Jazz + Postgres).

## Usage

```bash
f db <provider> <action>
```

## Jazz

Create Jazz2 app credentials and populate env vars for the current project.

```bash
f db jazz new --kind mirror --name gitedit-mirror
```

## Postgres

Run Drizzle migrations for the default Postgres project (`~/org/la/la/server`).

```bash
f db postgres migrate
f db postgres migrate --generate
f db postgres generate
```

Environment resolution order for `DATABASE_URL`:

1. `--database-url` flag
2. `DATABASE_URL`
3. `PLANETSCALE_DATABASE_URL` / `PSCALE_DATABASE_URL`
4. `<project>/.env` (DATABASE_URL)

Use `--project` to override the Postgres project directory.
