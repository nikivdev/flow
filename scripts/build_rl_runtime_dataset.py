#!/usr/bin/env python3
"""Build Harbor-ready RL dataset snapshots from Flow + Seq runtime traces."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SEQ_HIGH_SIGNAL_PATTERNS = [
    r"^seqd\.request$",
    r"^seqd\.run(\.|$)",
    r"^cli\.run(\.|$)",
    r"^cli\.agent$",
    r"^cli\.open_app_toggle(\.|$)",
    r"^seq\.sequence\.",
    r"^menu\.select\.",
    r"^open_url(\.|$)",
    r"^app\.activate$",
    r"^actions\.",
    r"^AX_(STATUS|PROMPT)$",
]
SEQ_HIGH_SIGNAL_RE = re.compile("|".join(f"(?:{p})" for p in SEQ_HIGH_SIGNAL_PATTERNS))
LONG_TOKEN_RE = re.compile(r"\b[A-Za-z0-9_\-]{32,}\b")


@dataclass
class DatasetRow:
    id: str
    source: str
    event_name: str
    at_ms: int
    success: bool
    duration_ms: int
    error_class: str
    record: dict[str, Any]


def _read_jsonl(path: Path, *, last: int = 0) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    if last > 0:
        lines = lines[-last:]
    out: list[dict[str, Any]] = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            out.append(payload)
    return out


def _now_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def _hash_id(parts: list[str]) -> str:
    joined = "||".join(parts)
    return hashlib.sha256(joined.encode("utf-8")).hexdigest()


def _bucket(row_id: str, seed: int) -> int:
    digest = hashlib.sha256(f"{seed}:{row_id}".encode("utf-8")).hexdigest()
    return int(digest[:8], 16) % 100


def _as_int(value: Any, default: int = 0) -> int:
    if isinstance(value, bool):
        return default
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return default


def _sanitize_text(value: Any) -> str:
    if not isinstance(value, str):
        return ""
    text = value.strip()
    text = LONG_TOKEN_RE.sub("[REDACTED]", text)
    return text


def _extract_captured_text(value: Any) -> str:
    if isinstance(value, dict):
        text = value.get("text")
        if isinstance(text, str):
            return _sanitize_text(text)
        return ""
    return _sanitize_text(value)


def _reward_components(success: bool, duration_ms: int) -> tuple[float, float, float]:
    success_score = 1.0 if success else 0.0
    # Keep this bounded and simple for initial training signal.
    efficiency = max(0.0, 1.0 - (min(duration_ms, 20_000) / 20_000.0))
    composite = (0.8 * success_score) + (0.2 * efficiency)
    return round(success_score, 6), round(efficiency, 6), round(composite, 6)


def _normalize_flow(rows: list[dict[str, Any]]) -> list[DatasetRow]:
    out: list[DatasetRow] = []
    for idx, row in enumerate(rows, start=1):
        event_type = str(row.get("event_type") or "")
        if not event_type.startswith("everruns."):
            continue

        stage = str(row.get("stage") or "")
        event_name = f"everruns.stage.{stage}" if event_type == "everruns.runtime_event" and stage else event_type

        session_id = str(row.get("session_id") or "")
        event_id = str(row.get("event_id") or f"flow-event-{idx}")
        ts_ms = max(0, _as_int(row.get("ts_unix_ms"), 0))
        duration_ms = max(0, _as_int(row.get("duration_ms"), 0))
        success = bool(row.get("ok", True))
        error_class = _sanitize_text(row.get("error_class"))

        if event_type == "everruns.qa_pair":
            prompt = _extract_captured_text(row.get("prompt_text"))
            response = _extract_captured_text(row.get("response_text"))
            if not prompt or not response:
                continue
            success_score, efficiency_score, composite = _reward_components(success, duration_ms)
            stable_id = _hash_id(
                [
                    "flow_qa",
                    session_id,
                    event_id,
                    str(ts_ms),
                    prompt[:256],
                    response[:256],
                ]
            )
            record = {
                "record_type": "assistant_sft_example",
                "id": stable_id,
                "source": "flow_rl_signals",
                "event_name": event_name,
                "at_ms": ts_ms,
                "success": success,
                "duration_ms": duration_ms,
                "error_class": error_class,
                "session_id": session_id,
                "prompt": prompt,
                "response": response,
                "reward_components": {
                    "success": success_score,
                    "efficiency": efficiency_score,
                },
                "reward_composite": composite,
                "metadata": {
                    "runtime": str(row.get("runtime") or ""),
                    "input_message_id": _sanitize_text(row.get("input_message_id")),
                    "event_id": event_id,
                },
            }
            out.append(
                DatasetRow(
                    id=stable_id,
                    source="flow_rl_signals",
                    event_name=event_name,
                    at_ms=ts_ms,
                    success=success,
                    duration_ms=duration_ms,
                    error_class=error_class,
                    record=record,
                )
            )
            continue

        stable_id = _hash_id(["flow", session_id, event_id, event_name, str(ts_ms)])
        success_score, efficiency_score, composite = _reward_components(success, duration_ms)

        record = {
            "record_type": "runtime_training_event",
            "id": stable_id,
            "source": "flow_rl_signals",
            "event_name": event_name,
            "at_ms": ts_ms,
            "success": success,
            "duration_ms": duration_ms,
            "error_class": error_class,
            "session_id": session_id,
            "reward_components": {
                "success": success_score,
                "efficiency": efficiency_score,
            },
            "reward_composite": composite,
            "metadata": {
                "runtime": str(row.get("runtime") or ""),
                "tool_call_id": _sanitize_text(row.get("tool_call_id")),
                "tool_name": _sanitize_text(row.get("tool_name")),
                "seq_op": _sanitize_text(row.get("seq_op")),
                "attrs": row.get("attrs", {}),
            },
        }
        out.append(
            DatasetRow(
                id=stable_id,
                source="flow_rl_signals",
                event_name=event_name,
                at_ms=ts_ms,
                success=success,
                duration_ms=duration_ms,
                error_class=error_class,
                record=record,
            )
        )
    return out


def _normalize_seq(rows: list[dict[str, Any]]) -> list[DatasetRow]:
    out: list[DatasetRow] = []
    for idx, row in enumerate(rows, start=1):
        name = str(row.get("name") or row.get("event") or row.get("kind") or "")
        if not name:
            continue

        if name == "agent.qa.pair":
            subject_raw = row.get("subject")
            subject_obj: dict[str, Any] = {}
            if isinstance(subject_raw, str):
                try:
                    parsed = json.loads(subject_raw)
                    if isinstance(parsed, dict):
                        subject_obj = parsed
                except json.JSONDecodeError:
                    subject_obj = {}
            elif isinstance(subject_raw, dict):
                subject_obj = subject_raw

            prompt = _sanitize_text(subject_obj.get("question"))
            response = _sanitize_text(subject_obj.get("answer"))
            if not prompt or not response:
                continue

            event_id = str(row.get("event_id") or f"seq-event-{idx}")
            session_id = str(row.get("session_id") or subject_obj.get("session_id") or "")
            ts_ms = max(0, _as_int(row.get("ts_ms"), 0))
            dur_us = max(0, _as_int(row.get("dur_us"), 0))
            duration_ms = dur_us // 1000
            success = bool(row.get("ok", True))
            error_class = ""
            stable_id = _hash_id(
                [
                    "seq_qa",
                    session_id,
                    event_id,
                    str(ts_ms),
                    prompt[:256],
                    response[:256],
                ]
            )
            success_score, efficiency_score, composite = _reward_components(success, duration_ms)
            record = {
                "record_type": "assistant_sft_example",
                "id": stable_id,
                "source": "seq_mem",
                "event_name": name,
                "at_ms": ts_ms,
                "success": success,
                "duration_ms": duration_ms,
                "error_class": error_class,
                "session_id": session_id,
                "prompt": prompt,
                "response": response,
                "reward_components": {
                    "success": success_score,
                    "efficiency": efficiency_score,
                },
                "reward_composite": composite,
                "metadata": {
                    "agent": _sanitize_text(subject_obj.get("agent")),
                    "project_path": _sanitize_text(subject_obj.get("project_path")),
                    "source_path": _sanitize_text(subject_obj.get("source_path")),
                    "line_offset": _as_int(subject_obj.get("offset"), 0),
                },
            }
            out.append(
                DatasetRow(
                    id=stable_id,
                    source="seq_mem",
                    event_name=name,
                    at_ms=ts_ms,
                    success=success,
                    duration_ms=duration_ms,
                    error_class=error_class,
                    record=record,
                )
            )
            continue

        if not SEQ_HIGH_SIGNAL_RE.search(name):
            continue

        event_id = str(row.get("event_id") or f"seq-event-{idx}")
        session_id = str(row.get("session_id") or "")
        ts_ms = max(0, _as_int(row.get("ts_ms"), 0))
        dur_us = max(0, _as_int(row.get("dur_us"), 0))
        duration_ms = dur_us // 1000
        success = bool(row.get("ok", True))
        error_class = ""

        stable_id = _hash_id(["seq", session_id, event_id, name, str(ts_ms)])
        success_score, efficiency_score, composite = _reward_components(success, duration_ms)
        subject = _sanitize_text(row.get("subject"))

        record = {
            "record_type": "runtime_training_event",
            "id": stable_id,
            "source": "seq_mem",
            "event_name": name,
            "at_ms": ts_ms,
            "success": success,
            "duration_ms": duration_ms,
            "error_class": error_class,
            "session_id": session_id,
            "reward_components": {
                "success": success_score,
                "efficiency": efficiency_score,
            },
            "reward_composite": composite,
            "metadata": {
                "event_id": event_id,
                "subject": subject,
                "content_hash": _sanitize_text(row.get("content_hash")),
            },
        }
        out.append(
            DatasetRow(
                id=stable_id,
                source="seq_mem",
                event_name=name,
                at_ms=ts_ms,
                success=success,
                duration_ms=duration_ms,
                error_class=error_class,
                record=record,
            )
        )
    return out


def _cap_by_event(rows: list[DatasetRow], *, max_per_event: int, seed: int) -> tuple[list[DatasetRow], dict[str, int]]:
    if max_per_event <= 0:
        return rows, {}
    grouped: dict[str, list[DatasetRow]] = defaultdict(list)
    for row in rows:
        grouped[row.event_name].append(row)

    kept: list[DatasetRow] = []
    dropped: dict[str, int] = {}
    for event_name, event_rows in grouped.items():
        ranked = sorted(
            event_rows,
            key=lambda r: hashlib.sha256(f"{seed}:{r.id}".encode("utf-8")).hexdigest(),
        )
        kept_rows = ranked[:max_per_event]
        kept.extend(kept_rows)
        if len(ranked) > max_per_event:
            dropped[event_name] = len(ranked) - max_per_event
    kept.sort(key=lambda r: (r.at_ms, r.id))
    return kept, dropped


def _write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=True))
            fh.write("\n")


def _build_report(
    rows: list[DatasetRow],
    train: list[DatasetRow],
    val: list[DatasetRow],
    test: list[DatasetRow],
    *,
    min_rows: int,
    min_unique_events: int,
    max_dominance: float,
) -> tuple[dict[str, Any], bool]:
    errors: list[str] = []
    warnings: list[str] = []

    total_rows = len(rows)
    if total_rows < max(1, min_rows):
        errors.append(f"rows below threshold: {total_rows} < {min_rows}")
    if not train:
        errors.append("train split is empty")

    event_counts = Counter(r.event_name for r in rows)
    record_type_counts = Counter(str(r.record.get("record_type") or "") for r in rows)
    sft_only = total_rows > 0 and set(record_type_counts.keys()) <= {"assistant_sft_example"}
    unique_events = len(event_counts)
    min_unique_gate = 1 if sft_only else max(1, min_unique_events)
    if unique_events < min_unique_gate:
        errors.append(f"unique event names below threshold: {unique_events} < {min_unique_gate}")

    dominant_name = ""
    dominant_ratio = 0.0
    if event_counts and total_rows > 0:
        dominant_name, dominant_count = event_counts.most_common(1)[0]
        dominant_ratio = dominant_count / total_rows
        dominance_gate = 1.0 if sft_only else max_dominance
        if dominant_ratio > dominance_gate:
            errors.append(
                f"event dominance too high: {dominant_name}={dominant_ratio:.3f} > {dominance_gate:.3f}"
            )

    success_count = sum(1 for r in rows if r.success)
    success_rate = (success_count / total_rows) if total_rows else 0.0
    if total_rows > 0 and (success_rate < 0.05 or success_rate > 0.98):
        warnings.append(f"success rate skewed: {success_rate:.3f}")

    report = {
        "schema_version": "flow_runtime_validation_v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "ok": len(errors) == 0,
        "counts": {
            "rows": total_rows,
            "train_rows": len(train),
            "val_rows": len(val),
            "test_rows": len(test),
            "unique_events": unique_events,
            "success_rate": round(success_rate, 6),
            "record_types": dict(record_type_counts),
            "sft_only": sft_only,
        },
        "dominance": {
            "event_name": dominant_name,
            "ratio": round(dominant_ratio, 6),
        },
        "errors": errors,
        "warnings": warnings,
    }
    return report, len(errors) == 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Build RL runtime dataset from flow + seq logs")
    parser.add_argument("--harbor-dir", default=str(Path("~/repos/laude-institute/harbor").expanduser()))
    parser.add_argument("--flow-signals", default="out/logs/flow_rl_signals.jsonl")
    parser.add_argument("--seq-mem", default=str(Path("~/repos/ClickHouse/ClickHouse/user_files/seq_mem.jsonl").expanduser()))
    parser.add_argument("--snapshot", default="", help="snapshot name; default timestamp")
    parser.add_argument("--flow-last", type=int, default=20_000)
    parser.add_argument("--seq-last", type=int, default=50_000)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--val-percent", type=int, default=10)
    parser.add_argument("--test-percent", type=int, default=10)
    parser.add_argument("--max-per-event", type=int, default=120)
    parser.add_argument("--min-rows", type=int, default=50)
    parser.add_argument("--min-unique-events", type=int, default=3)
    parser.add_argument("--max-dominance", type=float, default=0.90)
    parser.add_argument("--write-latest", action="store_true")
    parser.add_argument("--allow-quality-fail", action="store_true")
    args = parser.parse_args()

    harbor_dir = Path(args.harbor_dir).expanduser().resolve()
    snapshot = args.snapshot.strip() or _now_stamp()

    flow_rows_raw = _read_jsonl(Path(args.flow_signals).expanduser().resolve(), last=max(0, args.flow_last))
    seq_rows_raw = _read_jsonl(Path(args.seq_mem).expanduser().resolve(), last=max(0, args.seq_last))

    flow_rows = _normalize_flow(flow_rows_raw)
    seq_rows = _normalize_seq(seq_rows_raw)
    merged = flow_rows + seq_rows

    unique: dict[str, DatasetRow] = {}
    for row in merged:
        unique[row.id] = row
    deduped = list(unique.values())
    deduped.sort(key=lambda r: (r.at_ms, r.id))
    deduped, dropped_by_event = _cap_by_event(
        deduped,
        max_per_event=max(0, args.max_per_event),
        seed=args.seed,
    )

    val_pct = max(0, min(args.val_percent, 100))
    test_pct = max(0, min(args.test_percent, 100 - val_pct))
    train_rows: list[DatasetRow] = []
    val_rows: list[DatasetRow] = []
    test_rows: list[DatasetRow] = []

    for row in deduped:
        b = _bucket(row.id, args.seed)
        if b < test_pct:
            test_rows.append(row)
        elif b < test_pct + val_pct:
            val_rows.append(row)
        else:
            train_rows.append(row)

    raw_dir = harbor_dir / "data" / "flow_runtime" / snapshot
    prepared_dir = harbor_dir / "data" / "flow_runtime_prepared" / snapshot
    _write_jsonl(raw_dir / "events.jsonl", [r.record for r in deduped])
    _write_jsonl(prepared_dir / "train.jsonl", [r.record for r in train_rows])
    _write_jsonl(prepared_dir / "val.jsonl", [r.record for r in val_rows])
    _write_jsonl(prepared_dir / "test.jsonl", [r.record for r in test_rows])

    event_counts = Counter(r.event_name for r in deduped)
    _write_jsonl(
        prepared_dir / "event_counts.jsonl",
        [{"event_name": name, "count": count} for name, count in event_counts.most_common()],
    )

    manifest = {
        "schema_version": "flow_runtime_dataset_v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "snapshot": snapshot,
        "seed": args.seed,
        "split": {"val_percent": val_pct, "test_percent": test_pct},
        "cap": {"max_per_event": max(0, args.max_per_event), "dropped_by_event": dropped_by_event},
        "counts": {
            "flow_rows_raw": len(flow_rows_raw),
            "seq_rows_raw": len(seq_rows_raw),
            "flow_rows_mapped": len(flow_rows),
            "seq_rows_mapped": len(seq_rows),
            "deduped_rows": len(deduped),
            "train_rows": len(train_rows),
            "val_rows": len(val_rows),
            "test_rows": len(test_rows),
        },
        "paths": {
            "raw_events": str(raw_dir / "events.jsonl"),
            "train": str(prepared_dir / "train.jsonl"),
            "val": str(prepared_dir / "val.jsonl"),
            "test": str(prepared_dir / "test.jsonl"),
            "event_counts": str(prepared_dir / "event_counts.jsonl"),
        },
    }
    (raw_dir / "summary.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (prepared_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    report, ok = _build_report(
        deduped,
        train_rows,
        val_rows,
        test_rows,
        min_rows=max(1, args.min_rows),
        min_unique_events=max(1, args.min_unique_events),
        max_dominance=max(0.0, min(args.max_dominance, 1.0)),
    )
    (prepared_dir / "validation_report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    if args.write_latest:
        latest_raw = harbor_dir / "data" / "flow_runtime" / "latest"
        latest_prepared = harbor_dir / "data" / "flow_runtime_prepared" / "latest"
        _write_jsonl(latest_raw / "events.jsonl", [r.record for r in deduped])
        _write_jsonl(latest_prepared / "train.jsonl", [r.record for r in train_rows])
        _write_jsonl(latest_prepared / "val.jsonl", [r.record for r in val_rows])
        _write_jsonl(latest_prepared / "test.jsonl", [r.record for r in test_rows])
        _write_jsonl(
            latest_prepared / "event_counts.jsonl",
            [{"event_name": name, "count": count} for name, count in event_counts.most_common()],
        )
        (latest_raw / "summary.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        (latest_prepared / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        (latest_prepared / "validation_report.json").write_text(
            json.dumps(report, indent=2) + "\n",
            encoding="utf-8",
        )

    print(f"Built flow runtime dataset snapshot: {snapshot}")
    print(f"  flow rows mapped: {len(flow_rows)}")
    print(f"  seq rows mapped:  {len(seq_rows)}")
    print(f"  deduped rows:     {len(deduped)}")
    print(f"  train/val/test:   {len(train_rows)}/{len(val_rows)}/{len(test_rows)}")
    print(f"  quality ok:       {ok}")
    print(f"  raw:              {raw_dir}")
    print(f"  prepared:         {prepared_dir}")

    if not ok and not args.allow_quality_fail:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
