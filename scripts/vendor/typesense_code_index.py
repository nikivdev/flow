#!/usr/bin/env python3
"""Index first-party + vendored code into Typesense for fast local search.

Inspired by opensrc's "source inventory + local context" workflow, but targeted at
Rust/Cargo vendoring. This script reads Flow vendoring metadata and builds:

- <prefix>_sources: source inventory (vendored crates + first-party repo)
- <prefix>_chunks: code chunks for full-text search
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, NoReturn
from urllib import error, parse, request

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError as exc:  # pragma: no cover
    raise SystemExit("python 3.11+ is required (missing tomllib)") from exc

def env_first(*names: str, default: str) -> str:
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return default


DEFAULT_TYPESENSE_URL = env_first("LINSA_TYPESENSE_URL", "TYPESENSE_URL", default="http://127.0.0.1:8108")
DEFAULT_TYPESENSE_API_KEY = env_first("LINSA_TYPESENSE_API_KEY", "TYPESENSE_API_KEY", default="ts_local_dev_key")
DEFAULT_PREFIX = "flow_code"
DEFAULT_SOURCE_INDEX = ".vendor/typesense/sources.json"

TEXT_EXTS = {
    ".rs",
    ".toml",
    ".md",
    ".txt",
    ".yaml",
    ".yml",
    ".json",
    ".sh",
    ".py",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".go",
    ".cpp",
    ".cc",
    ".c",
    ".h",
    ".hpp",
    ".proto",
}

FIRST_PARTY_DIRS = ["src", "crates", "scripts", "docs", "tests"]
EXCLUDE_DIRS = {
    ".git",
    ".jj",
    "target",
    "node_modules",
    ".vendor",
    "dist",
    "build",
    ".next",
    ".venv",
    "out",
}

RUST_SYMBOL_PATTERNS = [
    re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"),
    re.compile(r"^\s*(?:pub\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)"),
    re.compile(r"^\s*(?:pub\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)"),
    re.compile(r"^\s*(?:pub\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)"),
    re.compile(r"^\s*impl\s+(?:<[^>]+>\s*)?([A-Za-z_][A-Za-z0-9_]*)"),
]
GENERIC_SYMBOL_PATTERNS = [
    re.compile(r"^\s*def\s+([A-Za-z_][A-Za-z0-9_]*)"),
    re.compile(r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)"),
    re.compile(r"^\s*function\s+([A-Za-z_][A-Za-z0-9_]*)"),
]


@dataclass
class SourceEntry:
    source_id: str
    kind: str
    scope: str
    name: str
    version: str | None
    materialized_path: str
    upstream_repository: str | None
    history_head: str | None
    checksum: str | None
    synced_at_utc: str | None


def die(msg: str) -> NoReturn:
    raise SystemExit(msg)


def load_toml(path: Path) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def load_vendor_sources(project: Path) -> list[SourceEntry]:
    vendor_lock = load_vendor_lock(project)
    flow_vendor_meta = vendor_lock.get("flow_vendor", {})
    lock_crates = _as_list(vendor_lock.get("crate"))
    vendor_commit = _s(_as_dict(flow_vendor_meta).get("commit"))

    by_crate: dict[str, SourceEntry] = {}
    for item in lock_crates:
        row = _as_dict(item)
        crate = _s(row.get("name"))
        if crate is None:
            continue
        by_crate[crate] = SourceEntry(
            source_id=f"vendor:{crate}",
            kind="crate",
            scope="vendor",
            name=crate,
            version=None,
            materialized_path=_s(row.get("materialized_path")) or f"lib/vendor/{crate}",
            upstream_repository=None,
            history_head=vendor_commit,
            checksum=None,
            synced_at_utc=None,
        )

    manifest_dir = project / "lib/vendor-manifest"
    if manifest_dir.is_dir():
        for manifest in sorted(manifest_dir.glob("*.toml")):
            data = load_toml(manifest)
            crate = str(data.get("crate", manifest.stem))
            prev = by_crate.get(crate)
            by_crate[crate] = SourceEntry(
                source_id=f"vendor:{crate}",
                kind="crate",
                scope="vendor",
                name=crate,
                version=_s(data.get("version")),
                materialized_path=(
                    _s(data.get("materialized_path"))
                    or (prev.materialized_path if prev else None)
                    or f"lib/vendor/{crate}"
                ),
                upstream_repository=_s(data.get("upstream_repository")) or (prev.upstream_repository if prev else None),
                history_head=_s(data.get("history_head")) or (prev.history_head if prev else None),
                checksum=_s(data.get("cargo_registry_checksum")),
                synced_at_utc=_s(data.get("synced_at_utc")),
            )

    entries = sorted(by_crate.values(), key=lambda s: s.name)
    entries.append(
        SourceEntry(
            source_id="firstparty:flow",
            kind="repo",
            scope="firstparty",
            name=project.name,
            version=None,
            materialized_path=".",
            upstream_repository=None,
            history_head=None,
            checksum=None,
            synced_at_utc=None,
        )
    )
    return entries


def load_vendor_lock(project: Path) -> dict:
    path = project / "vendor.lock.toml"
    if not path.is_file():
        return {}
    return load_toml(path)


def _as_dict(value: object) -> dict:
    return value if isinstance(value, dict) else {}


def _as_list(value: object) -> list:
    return value if isinstance(value, list) else []


def _s(v: object) -> str | None:
    if v is None:
        return None
    s = str(v).strip()
    return s if s else None


def typesense_request(
    method: str,
    url: str,
    api_key: str,
    *,
    payload: bytes | None = None,
    content_type: str = "application/json",
) -> tuple[int, bytes]:
    headers = {"X-TYPESENSE-API-KEY": api_key}
    if payload is not None:
        headers["Content-Type"] = content_type
    req = request.Request(url=url, method=method, data=payload, headers=headers)
    try:
        with request.urlopen(req, timeout=30) as resp:
            return resp.status, resp.read()
    except error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Typesense {method} {url} failed ({exc.code}): {body}") from exc
    except error.URLError as exc:
        reason = getattr(exc, "reason", exc)
        raise RuntimeError(f"Typesense {method} {url} failed (connection): {reason}") from exc


def collection_url(base: str, name: str) -> str:
    return f"{base.rstrip('/')}/collections/{parse.quote(name)}"


def ensure_collection(base_url: str, api_key: str, name: str, fields: list[dict], dry_run: bool) -> None:
    if dry_run:
        return

    url = collection_url(base_url, name)
    try:
        status, _ = typesense_request("GET", url, api_key)
        if status == 200:
            return
    except RuntimeError as err:
        if "(404)" not in str(err):
            raise

    schema = {"name": name, "fields": fields}
    typesense_request(
        "POST",
        f"{base_url.rstrip('/')}/collections",
        api_key,
        payload=json.dumps(schema).encode("utf-8"),
    )


def import_jsonl(base_url: str, api_key: str, collection: str, docs: list[dict], dry_run: bool) -> int:
    if not docs:
        return 0
    if dry_run:
        return len(docs)

    jsonl = "\n".join(json.dumps(d, ensure_ascii=False) for d in docs) + "\n"
    url = f"{collection_url(base_url, collection)}/documents/import?action=upsert"
    _, body = typesense_request(
        "POST",
        url,
        api_key,
        payload=jsonl.encode("utf-8"),
        content_type="text/plain",
    )
    lines = [line for line in body.decode("utf-8", errors="replace").splitlines() if line.strip()]
    failed = 0
    for line in lines:
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not item.get("success", False):
            failed += 1
    if failed:
        raise RuntimeError(f"Typesense import reported {failed} failed docs in {collection}")
    return len(lines)


def iter_text_files(root: Path, *, exclude_vendor: bool) -> Iterable[Path]:
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        if any(part in EXCLUDE_DIRS for part in path.parts):
            continue
        if exclude_vendor and "lib" in path.parts and "vendor" in path.parts:
            continue
        if path.suffix.lower() in TEXT_EXTS:
            yield path


def extract_symbols(path: Path, lines: list[str]) -> list[str]:
    patterns = RUST_SYMBOL_PATTERNS if path.suffix == ".rs" else GENERIC_SYMBOL_PATTERNS
    symbols: list[str] = []
    seen: set[str] = set()
    for line in lines:
        for pat in patterns:
            m = pat.search(line)
            if not m:
                continue
            sym = m.group(1)
            if sym in seen:
                continue
            seen.add(sym)
            symbols.append(sym)
            if len(symbols) >= 24:
                return symbols
    return symbols


def chunk_lines(lines: list[str], chunk_size: int, overlap: int) -> Iterable[tuple[int, int, str]]:
    if not lines:
        return
    start = 0
    count = len(lines)
    while start < count:
        end = min(start + chunk_size, count)
        text = "\n".join(lines[start:end]).strip()
        if text:
            yield start + 1, end, text
        if end >= count:
            break
        start = max(end - overlap, start + 1)


def lang_for(path: Path) -> str:
    ext = path.suffix.lower().lstrip(".")
    return ext or "text"


def file_to_chunks(
    project: Path,
    file_path: Path,
    *,
    source: SourceEntry,
    chunk_lines_n: int,
    overlap: int,
) -> list[dict]:
    rel = file_path.relative_to(project).as_posix()
    raw = file_path.read_text(encoding="utf-8", errors="replace")
    lines = raw.splitlines()
    symbols = extract_symbols(file_path, lines)
    docs: list[dict] = []

    for line_start, line_end, content in chunk_lines(lines, chunk_lines_n, overlap):
        key = f"{source.source_id}|{rel}|{line_start}|{line_end}"
        doc_id = hashlib.sha1(key.encode("utf-8")).hexdigest()
        docs.append(
            {
                "id": doc_id,
                "kind": "code",
                "project": project.name,
                "scope": source.scope,
                "source_id": source.source_id,
                "crate": source.name if source.scope == "vendor" else "",
                "rel_path": rel,
                "lang": lang_for(file_path),
                "symbols": symbols,
                "line_start": line_start,
                "line_end": line_end,
                "preview": content[:220],
                "content": content,
            }
        )
    return docs


def build_sources_docs(project: Path, sources: list[SourceEntry]) -> list[dict]:
    docs = []
    for src in sources:
        docs.append(
            {
                "id": src.source_id,
                "project": project.name,
                "kind": src.kind,
                "scope": src.scope,
                "name": src.name,
                "version": src.version or "",
                "materialized_path": src.materialized_path,
                "upstream_repository": src.upstream_repository or "",
                "history_head": src.history_head or "",
                "checksum": src.checksum or "",
                "synced_at_utc": src.synced_at_utc or "",
            }
        )
    return docs


def collect_chunk_docs(
    project: Path,
    sources: list[SourceEntry],
    *,
    chunk_lines_n: int,
    overlap: int,
    max_files: int,
) -> list[dict]:
    docs: list[dict] = []
    seen_files = 0

    # vendored sources
    for src in sources:
        if src.scope != "vendor":
            continue
        root = project / src.materialized_path
        if not root.is_dir():
            continue
        for file_path in iter_text_files(root, exclude_vendor=False):
            docs.extend(
                file_to_chunks(
                    project,
                    file_path,
                    source=src,
                    chunk_lines_n=chunk_lines_n,
                    overlap=overlap,
                )
            )
            seen_files += 1
            if max_files and seen_files >= max_files:
                return docs

    # first-party sources
    first = next((s for s in sources if s.scope == "firstparty"), None)
    if first is None:
        return docs

    for directory in FIRST_PARTY_DIRS:
        root = project / directory
        if not root.is_dir():
            continue
        for file_path in iter_text_files(root, exclude_vendor=True):
            docs.extend(
                file_to_chunks(
                    project,
                    file_path,
                    source=first,
                    chunk_lines_n=chunk_lines_n,
                    overlap=overlap,
                )
            )
            seen_files += 1
            if max_files and seen_files >= max_files:
                return docs

    return docs


def write_sources_index(project: Path, sources: list[SourceEntry], out_path: str) -> Path:
    path = project / out_path
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "project": project.name,
        "updated_at": _utc_now(),
        "sources": [
            {
                "source_id": s.source_id,
                "kind": s.kind,
                "scope": s.scope,
                "name": s.name,
                "version": s.version,
                "materialized_path": s.materialized_path,
                "upstream_repository": s.upstream_repository,
                "history_head": s.history_head,
                "checksum": s.checksum,
                "synced_at_utc": s.synced_at_utc,
            }
            for s in sources
        ],
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return path


def _utc_now() -> str:
    from datetime import datetime, timezone

    return datetime.now(tz=timezone.utc).isoformat()


def cmd_index(args: argparse.Namespace) -> None:
    project = Path(args.project).expanduser().resolve()
    if not (project / "Cargo.toml").is_file():
        die(f"not a cargo project: {project}")

    sources = load_vendor_sources(project)
    source_index = project / args.sources_index
    if not args.dry_run:
        source_index = write_sources_index(project, sources, args.sources_index)

    sources_collection = f"{args.prefix}_sources"
    chunks_collection = f"{args.prefix}_chunks"

    source_fields = [
        {"name": "id", "type": "string"},
        {"name": "project", "type": "string", "facet": True},
        {"name": "kind", "type": "string", "facet": True},
        {"name": "scope", "type": "string", "facet": True},
        {"name": "name", "type": "string", "facet": True},
        {"name": "version", "type": "string", "facet": True, "optional": True},
        {"name": "materialized_path", "type": "string", "optional": True},
        {"name": "upstream_repository", "type": "string", "optional": True},
        {"name": "history_head", "type": "string", "optional": True},
        {"name": "checksum", "type": "string", "optional": True},
        {"name": "synced_at_utc", "type": "string", "optional": True},
    ]
    chunk_fields = [
        {"name": "id", "type": "string"},
        {"name": "kind", "type": "string", "facet": True},
        {"name": "project", "type": "string", "facet": True},
        {"name": "scope", "type": "string", "facet": True},
        {"name": "source_id", "type": "string", "facet": True},
        {"name": "crate", "type": "string", "facet": True, "optional": True},
        {"name": "rel_path", "type": "string"},
        {"name": "lang", "type": "string", "facet": True},
        {"name": "symbols", "type": "string[]", "optional": True},
        {"name": "line_start", "type": "int32", "optional": True},
        {"name": "line_end", "type": "int32", "optional": True},
        {"name": "preview", "type": "string", "optional": True},
        {"name": "content", "type": "string"},
    ]

    ensure_collection(args.url, args.api_key, sources_collection, source_fields, args.dry_run)
    ensure_collection(args.url, args.api_key, chunks_collection, chunk_fields, args.dry_run)

    source_docs = build_sources_docs(project, sources)
    indexed_sources = import_jsonl(args.url, args.api_key, sources_collection, source_docs, args.dry_run)

    chunk_docs = collect_chunk_docs(
        project,
        sources,
        chunk_lines_n=args.chunk_lines,
        overlap=args.chunk_overlap,
        max_files=args.max_files,
    )

    indexed_chunks = 0
    for i in range(0, len(chunk_docs), args.batch_size):
        batch = chunk_docs[i : i + args.batch_size]
        indexed_chunks += import_jsonl(args.url, args.api_key, chunks_collection, batch, args.dry_run)

    print(f"project:          {project}")
    print(f"sources index:    {source_index}")
    print(f"typesense url:    {args.url}")
    print(f"sources docs:     {indexed_sources}")
    print(f"chunk docs:       {indexed_chunks}")
    print(f"sources coll:     {sources_collection}")
    print(f"chunks coll:      {chunks_collection}")
    if args.dry_run:
        print("mode:             dry-run (no writes to Typesense)")


def _build_filter(args: argparse.Namespace) -> str | None:
    filters: list[str] = []
    if args.scope:
        filters.append(f"scope:={args.scope}")
    if args.crate:
        field = "name" if args.collection == "sources" else "crate"
        filters.append(f"{field}:={args.crate}")
    if args.lang and args.collection != "sources":
        filters.append(f"lang:={args.lang}")
    if args.path_prefix:
        field = "materialized_path" if args.collection == "sources" else "rel_path"
        filters.append(f"{field}:{args.path_prefix}")
    return " && ".join(filters) if filters else None


def cmd_search(args: argparse.Namespace) -> None:
    collection = f"{args.prefix}_{args.collection}"
    if args.collection == "sources":
        query_by = "name,materialized_path,upstream_repository,version,checksum,history_head"
        highlight_fields = "name,materialized_path,upstream_repository"
    else:
        query_by = "content,rel_path,crate,symbols,preview"
        highlight_fields = "content,preview"

    params = {
        "q": args.query,
        "query_by": query_by,
        "per_page": str(args.limit),
        "highlight_fields": highlight_fields,
    }
    filter_by = _build_filter(args)
    if filter_by:
        params["filter_by"] = filter_by

    url = f"{collection_url(args.url, collection)}/documents/search?{parse.urlencode(params)}"
    _, body = typesense_request("GET", url, args.api_key)
    data = json.loads(body.decode("utf-8"))
    if args.json:
        print(json.dumps(data, indent=2))
        return

    found = data.get("found", 0)
    hits = data.get("hits", [])
    print(f"collection: {collection}")
    print(f"found:      {found}")
    print()

    for idx, hit in enumerate(hits, start=1):
        doc = hit.get("document", {})

        if args.collection == "sources":
            scope = doc.get("scope", "")
            name = doc.get("name", "")
            version = doc.get("version", "")
            materialized_path = doc.get("materialized_path", "")
            upstream = doc.get("upstream_repository", "")
            checksum = doc.get("checksum", "")
            synced_at = doc.get("synced_at_utc", "")
            header = f"{idx:02d}. {scope}::{name}"
            if version:
                header += f" v{version}"
            print(header)
            print(f"    path: {materialized_path}")
            if upstream:
                print(f"    upstream: {upstream}")
            if checksum:
                print(f"    checksum: {checksum}")
            if synced_at:
                print(f"    synced_at_utc: {synced_at}")
            continue

        rel_path = doc.get("rel_path") or doc.get("materialized_path") or ""
        scope = doc.get("scope", "")
        crate = doc.get("crate", "")
        line_start = doc.get("line_start")
        line_end = doc.get("line_end")

        line_part = ""
        if line_start and line_end:
            line_part = f" [{line_start}-{line_end}]"

        header = f"{idx:02d}. {scope}"
        if crate:
            header += f"::{crate}"
        header += f" {rel_path}{line_part}"
        print(header)

        snippet = doc.get("preview") or doc.get("content") or ""
        snippet = str(snippet).replace("\n", " ").strip()
        if len(snippet) > 220:
            snippet = snippet[:220] + "..."
        print(f"    {snippet}")


def cmd_sources(args: argparse.Namespace) -> None:
    project = Path(args.project).expanduser().resolve()
    sources = load_vendor_sources(project)
    out = {
        "project": project.name,
        "updated_at": _utc_now(),
        "sources": [
            {
                "source_id": s.source_id,
                "kind": s.kind,
                "scope": s.scope,
                "name": s.name,
                "version": s.version,
                "materialized_path": s.materialized_path,
                "upstream_repository": s.upstream_repository,
                "history_head": s.history_head,
                "checksum": s.checksum,
                "synced_at_utc": s.synced_at_utc,
            }
            for s in sources
        ],
    }
    if args.write:
        path = write_sources_index(project, sources, args.sources_index)
        print(path)
    else:
        print(json.dumps(out, indent=2))


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Typesense code index/search for Flow vendored + first-party code")
    p.add_argument("--project", default=".", help="Project root (default: current directory)")
    p.add_argument("--url", default=DEFAULT_TYPESENSE_URL, help="Typesense URL")
    p.add_argument("--api-key", default=DEFAULT_TYPESENSE_API_KEY, help="Typesense API key")
    p.add_argument("--prefix", default=DEFAULT_PREFIX, help="Collection prefix")
    p.add_argument(
        "--sources-index",
        default=DEFAULT_SOURCE_INDEX,
        help="Path (relative to project) for generated sources index JSON",
    )

    sub = p.add_subparsers(dest="command", required=True)

    p_index = sub.add_parser("index", help="Index first-party + vendored code")
    p_index.add_argument("--chunk-lines", type=int, default=120, help="Lines per chunk")
    p_index.add_argument("--chunk-overlap", type=int, default=20, help="Overlapped lines between chunks")
    p_index.add_argument("--batch-size", type=int, default=250, help="Import batch size")
    p_index.add_argument("--max-files", type=int, default=0, help="Debug limit (0 = no limit)")
    p_index.add_argument("--dry-run", action="store_true", help="Do not write to Typesense")
    p_index.set_defaults(func=cmd_index)

    p_search = sub.add_parser("search", help="Search indexed code/sources")
    p_search.add_argument("query", help="Search query")
    p_search.add_argument("--collection", choices=["chunks", "sources"], default="chunks")
    p_search.add_argument("--scope", choices=["vendor", "firstparty"])
    p_search.add_argument("--crate", help="Filter by vendored crate name")
    p_search.add_argument("--lang", help="Filter by language (rs, toml, md, ...)")
    p_search.add_argument("--path-prefix", help="Filter by path prefix (rel_path or materialized_path)")
    p_search.add_argument("--limit", type=int, default=20)
    p_search.add_argument("--json", action="store_true", help="Print raw Typesense JSON")
    p_search.set_defaults(func=cmd_search)

    p_sources = sub.add_parser("sources", help="Show/write opensrc-style source inventory")
    p_sources.add_argument("--write", action="store_true", help="Write sources index file and print path")
    p_sources.set_defaults(func=cmd_sources)

    return p


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    try:
        args.func(args)
    except RuntimeError as err:
        die(str(err))


if __name__ == "__main__":
    main()
