#!/usr/bin/env python3
"""Audit rough edges in Cargo-first vendoring setup.

This script is intentionally strict about structural invariants and surfaces
actionable warnings for optimization workflow gaps.
"""

from __future__ import annotations

import argparse
import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError as exc:  # pragma: no cover
    raise SystemExit("python 3.11+ is required (missing tomllib)") from exc


@dataclass
class Finding:
    severity: str  # error | warn | info
    code: str
    message: str
    hint: str | None = None


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def read_manifest_crate(path: Path) -> str:
    try:
        data = load_toml(path)
    except Exception:
        return path.stem
    crate = str(data.get("crate", path.stem)).strip()
    return crate or path.stem


def list_lock_crates(vendor_lock: dict[str, Any]) -> list[dict[str, str]]:
    out: list[dict[str, str]] = []
    for row in vendor_lock.get("crate", []):
        if not isinstance(row, dict):
            continue
        name = str(row.get("name", "")).strip()
        if not name:
            continue
        out.append(
            {
                "name": name,
                "manifest_path": str(row.get("manifest_path", f"lib/vendor-manifest/{name}.toml")).strip(),
                "materialized_path": str(row.get("materialized_path", f"lib/vendor/{name}")).strip(),
                "repo_path": str(row.get("repo_path", f"crates/{name}")).strip(),
            }
        )
    return out


def read_patch_paths(cargo_toml: dict[str, Any]) -> dict[str, str]:
    patch = cargo_toml.get("patch", {})
    if not isinstance(patch, dict):
        return {}
    crates_io = patch.get("crates-io", {})
    if not isinstance(crates_io, dict):
        return {}

    out: dict[str, str] = {}
    for name, value in crates_io.items():
        if isinstance(value, dict):
            path = value.get("path")
            if isinstance(path, str):
                out[name] = path
    return out


def latest_mtime(paths: list[Path]) -> float:
    latest = 0.0
    for path in paths:
        if not path.exists():
            continue
        latest = max(latest, path.stat().st_mtime)
    return latest


def build_report(project: Path) -> tuple[dict[str, Any], list[Finding]]:
    findings: list[Finding] = []
    metrics: dict[str, Any] = {
        "project": str(project),
        "vendored_crates": 0,
        "vendor_manifests": 0,
        "vendor_patch_entries": 0,
        "direct_dependencies": 0,
        "direct_non_vendored_dependencies": 0,
        "direct_non_vendored_list": [],
    }

    vendor_lock_path = project / "vendor.lock.toml"
    cargo_toml_path = project / "Cargo.toml"
    cargo_lock_path = project / "Cargo.lock"

    if not vendor_lock_path.is_file():
        findings.append(
            Finding(
                "error",
                "missing_vendor_lock",
                f"missing {vendor_lock_path}",
                "run bootstrap/inhouse flow to create vendor.lock.toml",
            )
        )
        return metrics, findings

    if not cargo_toml_path.is_file():
        findings.append(
            Finding(
                "error",
                "missing_cargo_toml",
                f"missing {cargo_toml_path}",
            )
        )
        return metrics, findings

    if not cargo_lock_path.is_file():
        findings.append(
            Finding(
                "error",
                "missing_cargo_lock",
                f"missing {cargo_lock_path}",
                "run cargo check to generate Cargo.lock",
            )
        )
        return metrics, findings

    vendor_lock = load_toml(vendor_lock_path)
    cargo_toml = load_toml(cargo_toml_path)
    cargo_lock = load_toml(cargo_lock_path)

    lock_section = "vendor" if "vendor" in vendor_lock else "flow_vendor" if "flow_vendor" in vendor_lock else None
    if lock_section is None:
        findings.append(
            Finding(
                "error",
                "missing_vendor_section",
                "vendor.lock.toml has no [vendor] or [flow_vendor] section",
            )
        )
    else:
        missing_keys = [
            key
            for key in ("repo", "branch", "checkout")
            if not str(vendor_lock.get(lock_section, {}).get(key, "")).strip()
        ]
        if missing_keys:
            findings.append(
                Finding(
                    "error",
                    "vendor_section_incomplete",
                    f"{lock_section} missing keys: {', '.join(missing_keys)}",
                )
            )

    lock_crates = list_lock_crates(vendor_lock)
    lock_crate_names = {row["name"] for row in lock_crates}
    metrics["vendored_crates"] = len(lock_crates)

    patch_paths = read_patch_paths(cargo_toml)
    vendor_patch_paths = {name: path for name, path in patch_paths.items() if path.startswith("lib/vendor/")}
    metrics["vendor_patch_entries"] = len(vendor_patch_paths)

    manifest_dir = project / "lib/vendor-manifest"
    manifest_files = sorted(manifest_dir.glob("*.toml")) if manifest_dir.is_dir() else []
    manifest_crates = {read_manifest_crate(path) for path in manifest_files}
    metrics["vendor_manifests"] = len(manifest_files)

    package_rows = cargo_lock.get("package", [])
    packages_by_name: dict[str, list[dict[str, Any]]] = {}
    if isinstance(package_rows, list):
        for row in package_rows:
            if not isinstance(row, dict):
                continue
            name = str(row.get("name", "")).strip()
            if not name:
                continue
            packages_by_name.setdefault(name, []).append(row)

    dep_table = cargo_toml.get("dependencies", {})
    if isinstance(dep_table, dict):
        direct_deps = sorted(dep_table.keys())
    else:
        direct_deps = []
    metrics["direct_dependencies"] = len(direct_deps)
    non_vendored = sorted(dep for dep in direct_deps if dep not in lock_crate_names)
    metrics["direct_non_vendored_dependencies"] = len(non_vendored)
    metrics["direct_non_vendored_list"] = non_vendored

    seen_lock = set()
    for row in lock_crates:
        crate = row["name"]
        seen_lock.add(crate)

        materialized = project / row["materialized_path"]
        if not (materialized / "Cargo.toml").is_file():
            findings.append(
                Finding(
                    "error",
                    "missing_materialized_crate",
                    f"{crate}: missing {materialized}/Cargo.toml",
                    "run scripts/vendor/vendor-repo.sh hydrate",
                )
            )

        # `manifest_path` in vendor.lock.toml points to the vendor-repo path
        # (`manifests/<crate>.toml`), while the local materialized manifest
        # lives in `lib/vendor-manifest/<crate>.toml`.
        expected_repo_manifest = f"manifests/{crate}.toml"
        if row["manifest_path"] and row["manifest_path"] != expected_repo_manifest:
            findings.append(
                Finding(
                    "warn",
                    "manifest_repo_path_unexpected",
                    f"{crate}: manifest_path={row['manifest_path']} (expected {expected_repo_manifest})",
                )
            )

        manifest_path = project / "lib/vendor-manifest" / f"{crate}.toml"
        if not manifest_path.is_file():
            findings.append(
                Finding(
                    "error",
                    "missing_vendor_manifest",
                    f"{crate}: missing local manifest {manifest_path}",
                    "re-run inhouse/sync for this crate",
                )
            )
        else:
            try:
                crate_manifest = load_toml(manifest_path)
            except Exception as exc:
                findings.append(
                    Finding(
                        "error",
                        "broken_vendor_manifest",
                        f"{crate}: failed to parse {manifest_path}: {exc}",
                    )
                )
                crate_manifest = {}

            manifest_crate = str(crate_manifest.get("crate", crate)).strip()
            manifest_version = str(crate_manifest.get("version", "")).strip()
            manifest_materialized = str(crate_manifest.get("materialized_path", row["materialized_path"])).strip()

            if manifest_crate and manifest_crate != crate:
                findings.append(
                    Finding(
                        "error",
                        "manifest_crate_mismatch",
                        f"{crate}: manifest crate={manifest_crate}",
                    )
                )

            if manifest_materialized and manifest_materialized != row["materialized_path"]:
                findings.append(
                    Finding(
                        "error",
                        "manifest_materialized_path_mismatch",
                        f"{crate}: manifest materialized_path={manifest_materialized}, lock={row['materialized_path']}",
                    )
                )

            if not str(crate_manifest.get("history_head", "")).strip():
                findings.append(
                    Finding(
                        "warn",
                        "missing_history_head",
                        f"{crate}: missing history_head in {manifest_path}",
                    )
                )
            if not str(crate_manifest.get("upstream_repository", "")).strip():
                findings.append(
                    Finding(
                        "warn",
                        "missing_upstream_repository",
                        f"{crate}: missing upstream_repository in {manifest_path}",
                    )
                )

            pkg_rows = packages_by_name.get(crate, [])
            versions = sorted({str(p.get("version", "")).strip() for p in pkg_rows if str(p.get("version", "")).strip()})
            if manifest_version and versions and len(versions) == 1 and manifest_version != versions[0]:
                findings.append(
                    Finding(
                        "error",
                        "manifest_version_mismatch",
                        f"{crate}: manifest version={manifest_version}, Cargo.lock version={versions[0]}",
                    )
                )

        patch_path = vendor_patch_paths.get(crate, "")
        if not patch_path:
            findings.append(
                Finding(
                    "error",
                    "missing_patch_entry",
                    f"{crate}: missing [patch.crates-io] path override",
                    "add crate path override in Cargo.toml",
                )
            )
        elif patch_path != row["materialized_path"]:
            findings.append(
                Finding(
                    "error",
                    "patch_path_mismatch",
                    f"{crate}: patch path={patch_path}, lock materialized_path={row['materialized_path']}",
                )
            )

        pkg_rows = packages_by_name.get(crate, [])
        if not pkg_rows:
            findings.append(
                Finding(
                    "error",
                    "missing_cargo_lock_entry",
                    f"{crate}: not present in Cargo.lock",
                )
            )
        else:
            sources = [str(p.get("source", "")).strip() for p in pkg_rows]
            if any(src.startswith("registry+") for src in sources):
                findings.append(
                    Finding(
                        "error",
                        "registry_source_for_vendored_crate",
                        f"{crate}: still resolves via registry source in Cargo.lock",
                        "run cargo update -p <crate> --precise <version> after patching",
                    )
                )
            versions = sorted({str(p.get("version", "")).strip() for p in pkg_rows if str(p.get("version", "")).strip()})
            if len(versions) > 1:
                findings.append(
                    Finding(
                        "error",
                        "multiple_lock_versions_for_vendored_crate",
                        f"{crate}: multiple versions in Cargo.lock ({', '.join(versions)})",
                    )
                )

    for crate in sorted(manifest_crates - lock_crate_names):
        findings.append(
            Finding(
                "warn",
                "manifest_not_in_lock",
                f"{crate}: manifest exists but crate not in vendor.lock.toml",
            )
        )

    for crate, path in sorted(vendor_patch_paths.items()):
        if crate not in lock_crate_names:
            findings.append(
                Finding(
                    "warn",
                    "patch_not_in_lock",
                    f"{crate}: patched to {path} but not listed in vendor.lock.toml",
                )
            )

    vendor_src_dir = project / "lib/vendor"
    if vendor_src_dir.is_dir():
        vendored_dirs = {p.name for p in vendor_src_dir.iterdir() if p.is_dir()}
        for extra in sorted(vendored_dirs - lock_crate_names):
            findings.append(
                Finding(
                    "warn",
                    "vendored_dir_not_in_lock",
                    f"lib/vendor/{extra} exists but crate is not in vendor.lock.toml",
                )
            )

    typesense_index = project / ".vendor/typesense/sources.json"
    if typesense_index.exists():
        watched = [vendor_lock_path, cargo_lock_path, project / "scripts/vendor/typesense_code_index.py"]
        watched.extend(manifest_files)
        if typesense_index.stat().st_mtime < latest_mtime(watched):
            findings.append(
                Finding(
                    "warn",
                    "stale_code_index",
                    "typesense sources index is older than vendoring inputs",
                    "run f vendor-code-index",
                )
            )
    else:
        findings.append(
            Finding(
                "info",
                "missing_code_index",
                "no .vendor/typesense/sources.json found",
                "run f vendor-code-index if you use vendor code search",
            )
        )

    return metrics, findings


def print_text(metrics: dict[str, Any], findings: list[Finding]) -> None:
    print(f"project: {metrics['project']}")
    print(f"vendored crates: {metrics['vendored_crates']}")
    print(f"vendor manifests: {metrics['vendor_manifests']}")
    print(f"vendor patch entries: {metrics['vendor_patch_entries']}")
    print(f"direct deps: {metrics['direct_dependencies']}")
    print(f"direct deps not yet vendored: {metrics['direct_non_vendored_dependencies']}")
    if metrics["direct_non_vendored_list"]:
        preview = ", ".join(metrics["direct_non_vendored_list"][:12])
        suffix = " ..." if len(metrics["direct_non_vendored_list"]) > 12 else ""
        print(f"non-vendored preview: {preview}{suffix}")
    print()

    if not findings:
        print("no findings")
        return

    for item in findings:
        print(f"[{item.severity}] {item.code}: {item.message}")
        if item.hint:
            print(f"  hint: {item.hint}")


def main() -> None:
    parser = argparse.ArgumentParser(description="Audit rough edges in vendored dependency workflow")
    parser.add_argument("--project", default=".", help="Project root (default: .)")
    parser.add_argument("--json", action="store_true", help="Emit report as JSON")
    parser.add_argument(
        "--strict-warnings",
        action="store_true",
        help="Exit non-zero on warnings (default only errors fail)",
    )
    args = parser.parse_args()

    project = Path(args.project).expanduser().resolve()
    metrics, findings = build_report(project)
    errors = sum(1 for f in findings if f.severity == "error")
    warnings = sum(1 for f in findings if f.severity == "warn")

    payload = {
        "metrics": metrics,
        "counts": {"errors": errors, "warnings": warnings, "total": len(findings)},
        "findings": [asdict(item) for item in findings],
    }

    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        print_text(metrics, findings)
        print()
        print(f"errors: {errors}")
        print(f"warnings: {warnings}")

    if errors > 0 or (args.strict_warnings and warnings > 0):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
