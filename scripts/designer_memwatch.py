#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import signal
import sqlite3
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field


STATE_ROOT = pathlib.Path.home() / ".config" / "flow-state" / "designer-memwatch"
INCIDENTS_ROOT = STATE_ROOT / "incidents"
SAMPLES_PATH = STATE_ROOT / "samples.jsonl"
CURRENT_PATH = STATE_ROOT / "current.json"
PID_PATH = STATE_ROOT / "daemon.pid"
LOG_PATH = STATE_ROOT / "daemon.log"
DB_PATH = STATE_ROOT / "telemetry.sqlite3"
DATE_FORMAT = "%Y%m%dT%H%M%SZ"
TOP_PROCESS_LIMIT = 10
DB_CONNECTION: sqlite3.Connection | None = None


@dataclass
class WorkspaceTarget:
    workspace_root: pathlib.Path
    app_path: pathlib.Path
    workspace_name: str
    branch: str | None = None


@dataclass
class ProcessInfo:
    pid: int
    ppid: int
    rss_kb: int
    vsz_kb: int
    cpu_percent: float
    elapsed: str
    command: str
    args: str
    reasons: list[str] = field(default_factory=list)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Monitor a live Designer workspace for memory pressure incidents.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    serve = subparsers.add_parser("serve", help="Run the long-lived memwatch loop.")
    add_shared_args(serve)
    serve.add_argument("--interval-sec", type=float, default=5.0)
    serve.add_argument("--heartbeat-sec", type=float, default=30.0)
    serve.add_argument("--cooldown-sec", type=float, default=300.0)

    start = subparsers.add_parser("start", help="Start the memwatch daemon in the background.")
    add_shared_args(start)
    start.add_argument("--interval-sec", type=float, default=5.0)
    start.add_argument("--heartbeat-sec", type=float, default=30.0)
    start.add_argument("--cooldown-sec", type=float, default=300.0)

    stop = subparsers.add_parser("stop", help="Stop the memwatch daemon.")
    stop.add_argument("--force", action="store_true")

    status = subparsers.add_parser("status", help="Show the memwatch daemon status.")
    status.add_argument("--json", action="store_true")

    capture = subparsers.add_parser("capture-now", help="Capture one sample immediately.")
    add_shared_args(capture)
    capture.add_argument("--force-incident", action="store_true")

    resolve = subparsers.add_parser(
        "resolve-workspace",
        help="Print the workspace that would be monitored.",
    )
    add_shared_args(resolve)

    latest = subparsers.add_parser(
        "latest-incident",
        help="Print the latest incident bundle directory if one exists.",
    )
    latest.add_argument("--json", action="store_true")
    return parser.parse_args()


def add_shared_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--workspace")
    parser.add_argument(
        "--process-rss-threshold-mb",
        type=int,
        default=int(os.environ.get("DESIGNER_MEMWATCH_PROCESS_RSS_MB_THRESHOLD", "2048")),
    )
    parser.add_argument(
        "--group-rss-threshold-mb",
        type=int,
        default=int(os.environ.get("DESIGNER_MEMWATCH_GROUP_RSS_MB_THRESHOLD", "4096")),
    )
    parser.add_argument(
        "--swap-threshold-mb",
        type=int,
        default=int(os.environ.get("DESIGNER_MEMWATCH_SWAP_MB_THRESHOLD", "6144")),
    )


def main() -> int:
    args = parse_args()
    if sys.platform != "darwin":
        raise SystemExit("designer-memwatch currently supports macOS only")

    STATE_ROOT.mkdir(parents=True, exist_ok=True)
    INCIDENTS_ROOT.mkdir(parents=True, exist_ok=True)

    if args.command == "latest-incident":
        return run_latest_incident(args)

    if args.command == "stop":
        return run_stop(args)

    if args.command == "status":
        return run_status(args)

    target = resolve_workspace_target(args.workspace)

    if args.command == "resolve-workspace":
        print(
            json.dumps(
                {
                    "workspaceRoot": str(target.workspace_root),
                    "appPath": str(target.app_path),
                    "workspaceName": target.workspace_name,
                    "branch": target.branch,
                },
                indent=2,
            )
        )
        return 0

    if args.command == "start":
        return run_start(target, args)

    if args.command == "capture-now":
        sample = collect_sample(target)
        write_sample(sample)
        reasons = classify_incident(args, sample)
        incident_path = None
        if args.force_incident or reasons:
            if args.force_incident and not reasons:
                reasons = ["forced-capture"]
            incident_path = write_incident_bundle(target, sample, reasons)
        print(
            json.dumps(
                {
                    "workspaceRoot": str(target.workspace_root),
                    "branch": target.branch,
                    "matchedProcessCount": sample["matchedProcessCount"],
                    "totalMatchedRssMb": sample["totalMatchedRssMb"],
                    "maxMatchedRssMb": sample["maxMatchedRssMb"],
                    "swapUsedMb": sample["system"]["swapUsedMb"],
                    "incidentReasons": reasons,
                    "incidentPath": str(incident_path) if incident_path else None,
                },
                indent=2,
            )
        )
        return 0

    return run_serve_loop(target, args)


def run_latest_incident(args: argparse.Namespace) -> int:
    if not INCIDENTS_ROOT.exists():
        return 1
    incidents = sorted([path for path in INCIDENTS_ROOT.iterdir() if path.is_dir()])
    if not incidents:
        return 1
    latest = incidents[-1]
    if args.json:
        print(json.dumps({"latestIncident": str(latest)}, indent=2))
    else:
        print(latest)
    return 0


def run_start(target: WorkspaceTarget, args: argparse.Namespace) -> int:
    existing_pid = load_pid()
    if existing_pid is not None and process_alive(existing_pid):
        print(f"designer-memwatch is already running [pid {existing_pid}]")
        return 0
    if existing_pid is not None:
        PID_PATH.unlink(missing_ok=True)

    command = [
        sys.executable,
        str(pathlib.Path(__file__).resolve()),
        "serve",
        "--workspace",
        str(target.workspace_root),
        "--interval-sec",
        str(args.interval_sec),
        "--heartbeat-sec",
        str(args.heartbeat_sec),
        "--cooldown-sec",
        str(args.cooldown_sec),
        "--process-rss-threshold-mb",
        str(args.process_rss_threshold_mb),
        "--group-rss-threshold-mb",
        str(args.group_rss_threshold_mb),
        "--swap-threshold-mb",
        str(args.swap_threshold_mb),
    ]
    with LOG_PATH.open("a", encoding="utf-8") as log_handle:
        child = subprocess.Popen(
            command,
            stdout=log_handle,
            stderr=log_handle,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
            cwd=str(pathlib.Path(__file__).resolve().parent.parent),
        )
    PID_PATH.write_text(f"{child.pid}\n", encoding="utf-8")
    time.sleep(1.0)
    if not process_alive(child.pid):
        PID_PATH.unlink(missing_ok=True)
        raise SystemExit(f"designer-memwatch exited immediately, see {LOG_PATH}")
    print(
        f"designer-memwatch started [pid {child.pid}] "
        f"workspace={target.workspace_root} log={LOG_PATH}"
    )
    return 0


def run_stop(args: argparse.Namespace) -> int:
    pid = load_pid()
    if pid is None:
        print("designer-memwatch is not running")
        return 0
    if not process_alive(pid):
        PID_PATH.unlink(missing_ok=True)
        print("designer-memwatch is not running")
        return 0
    sig = signal.SIGKILL if args.force else signal.SIGTERM
    os.kill(pid, sig)
    deadline = time.time() + 5
    while time.time() < deadline:
        if not process_alive(pid):
            PID_PATH.unlink(missing_ok=True)
            print(f"designer-memwatch stopped [pid {pid}]")
            return 0
        time.sleep(0.2)
    if not args.force:
        raise SystemExit(f"designer-memwatch did not stop after SIGTERM [pid {pid}]")
    PID_PATH.unlink(missing_ok=True)
    print(f"designer-memwatch stopped [pid {pid}]")
    return 0


def run_status(args: argparse.Namespace) -> int:
    pid = load_pid()
    running = pid is not None and process_alive(pid)
    if not running and pid is not None:
        PID_PATH.unlink(missing_ok=True)
    latest_incident = find_latest_incident()
    db_counts = load_db_counts()
    payload = {
        "running": running,
        "pid": pid if running else None,
        "logPath": str(LOG_PATH),
        "dbPath": str(DB_PATH),
        "dbCounts": db_counts,
        "currentPath": str(CURRENT_PATH),
        "latestIncident": str(latest_incident) if latest_incident else None,
    }
    if CURRENT_PATH.exists():
        payload["current"] = json.loads(CURRENT_PATH.read_text(encoding="utf-8"))
    if args.json:
        print(json.dumps(payload, indent=2))
        return 0
    state = "running" if running else "stopped"
    print(f"designer-memwatch: {state}")
    if running:
        print(f"pid: {pid}")
    print(f"log: {LOG_PATH}")
    print(f"db: {DB_PATH}")
    print(
        "db_counts: "
        f"samples={db_counts['samples']} "
        f"incidents={db_counts['incidents']} "
        f"events={db_counts['events']} "
        f"process_rows={db_counts['sampleProcesses']}"
    )
    if "current" in payload:
        current = payload["current"]
        print(f"workspace: {current['workspaceRoot']}")
        print(f"matched_processes: {current['matchedProcessCount']}")
        print(f"total_rss_mb: {current['totalMatchedRssMb']}")
        print(f"swap_used_mb: {current['system']['swapUsedMb']}")
    if payload["latestIncident"] is not None:
        print(f"latest_incident: {payload['latestIncident']}")
    return 0


def run_serve_loop(target: WorkspaceTarget, args: argparse.Namespace) -> int:
    last_heartbeat = 0.0
    last_incident_at = 0.0
    ready_message = (
        "memwatch-ready "
        f"workspace={target.workspace_root} "
        f"branch={target.branch or 'unknown'} "
        f"interval_sec={args.interval_sec} "
        f"process_rss_threshold_mb={args.process_rss_threshold_mb} "
        f"group_rss_threshold_mb={args.group_rss_threshold_mb} "
        f"swap_threshold_mb={args.swap_threshold_mb}"
    )
    emit_event(
        "info",
        "ready",
        ready_message,
        {
            "workspaceRoot": str(target.workspace_root),
            "branch": target.branch,
            "intervalSec": args.interval_sec,
            "processRssThresholdMb": args.process_rss_threshold_mb,
            "groupRssThresholdMb": args.group_rss_threshold_mb,
            "swapThresholdMb": args.swap_threshold_mb,
        },
    )
    while True:
        sample = collect_sample(target)
        write_sample(sample)
        now = time.time()
        reasons = classify_incident(args, sample)
        if reasons and now - last_incident_at >= args.cooldown_sec:
            incident_path = write_incident_bundle(target, sample, reasons)
            emit_event(
                "warn",
                "incident",
                "incident "
                f"path={incident_path} "
                f"reasons={','.join(reasons)} "
                f"matched_processes={sample['matchedProcessCount']} "
                f"total_rss_mb={sample['totalMatchedRssMb']} "
                f"max_rss_mb={sample['maxMatchedRssMb']} "
                f"swap_used_mb={sample['system']['swapUsedMb']}",
                {
                    "incidentPath": str(incident_path),
                    "reasons": reasons,
                    "matchedProcessCount": sample["matchedProcessCount"],
                    "totalMatchedRssMb": sample["totalMatchedRssMb"],
                    "maxMatchedRssMb": sample["maxMatchedRssMb"],
                    "swapUsedMb": sample["system"]["swapUsedMb"],
                },
            )
            last_incident_at = now
        if now - last_heartbeat >= args.heartbeat_sec:
            emit_event(
                "info",
                "heartbeat",
                "heartbeat "
                f"matched_processes={sample['matchedProcessCount']} "
                f"total_rss_mb={sample['totalMatchedRssMb']} "
                f"max_rss_mb={sample['maxMatchedRssMb']} "
                f"swap_used_mb={sample['system']['swapUsedMb']} "
                f"memory_free_pct={sample['system']['memoryFreePercent']}",
                {
                    "matchedProcessCount": sample["matchedProcessCount"],
                    "totalMatchedRssMb": sample["totalMatchedRssMb"],
                    "maxMatchedRssMb": sample["maxMatchedRssMb"],
                    "swapUsedMb": sample["system"]["swapUsedMb"],
                    "memoryFreePercent": sample["system"]["memoryFreePercent"],
                },
            )
            last_heartbeat = now
        time.sleep(args.interval_sec)


def resolve_workspace_target(explicit_workspace: str | None) -> WorkspaceTarget:
    raw_workspace = explicit_workspace or os.environ.get("DESIGNER_MEMWATCH_WORKSPACE")
    branch = None
    forge_status = None
    if raw_workspace is None:
        forge_status = run_json_command(["forge", "tip", "status", "designer", "--json"])
        raw_workspace = forge_status["selectedWorkspacePath"]
        branch = forge_status.get("selectedTipBranch")
    workspace_path = pathlib.Path(os.path.expanduser(raw_workspace)).resolve()
    if branch is None:
        forge_status = forge_status or run_optional_json_command(
            ["forge", "tip", "status", "designer", "--json"]
        )
        if forge_status is not None:
            selected_workspace = pathlib.Path(
                os.path.expanduser(forge_status["selectedWorkspacePath"])
            ).resolve()
            if selected_workspace == workspace_path:
                branch = forge_status.get("selectedTipBranch")
    if workspace_path.name == "designer" and workspace_path.parent.name == "ide":
        app_path = workspace_path
        workspace_root = workspace_path.parent.parent
    else:
        workspace_root = workspace_path
        app_path = workspace_root / "ide" / "designer"
    if not app_path.exists():
        raise SystemExit(f"Designer app path does not exist: {app_path}")
    return WorkspaceTarget(
        workspace_root=workspace_root,
        app_path=app_path,
        workspace_name=workspace_root.name,
        branch=branch,
    )


def collect_sample(target: WorkspaceTarget) -> dict:
    processes = load_process_table()
    matched = select_workspace_processes(processes, target)
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    system_snapshot = capture_system_snapshot()
    matched_processes = [serialize_process(proc) for proc in matched]
    top_processes = [serialize_process(proc) for proc in matched[:TOP_PROCESS_LIMIT]]
    sample = {
        "timestamp": now,
        "workspaceRoot": str(target.workspace_root),
        "appPath": str(target.app_path),
        "workspaceName": target.workspace_name,
        "branch": target.branch,
        "matchedProcessCount": len(matched),
        "totalMatchedRssMb": round(sum(proc.rss_kb for proc in matched) / 1024, 1),
        "maxMatchedRssMb": round((matched[0].rss_kb / 1024) if matched else 0.0, 1),
        "maxMatchedPid": matched[0].pid if matched else None,
        "topProcesses": top_processes,
        "system": system_snapshot["summary"],
    }
    sample["_matchedProcesses"] = matched_processes
    sample["_systemArtifacts"] = system_snapshot["artifacts"]
    return sample


def load_process_table() -> list[ProcessInfo]:
    completed = run_text_command(
        [
            "ps",
            "-axo",
            "pid=,ppid=,rss=,vsz=,%cpu=,etime=,comm=,args=",
        ]
    )
    rows: list[ProcessInfo] = []
    for line in completed.splitlines():
        parts = line.strip().split(None, 7)
        if len(parts) != 8:
            continue
        rows.append(
            ProcessInfo(
                pid=int(parts[0]),
                ppid=int(parts[1]),
                rss_kb=int(parts[2]),
                vsz_kb=int(parts[3]),
                cpu_percent=float(parts[4]),
                elapsed=parts[5],
                command=parts[6],
                args=parts[7],
            )
        )
    return rows


def select_workspace_processes(
    processes: list[ProcessInfo],
    target: WorkspaceTarget,
) -> list[ProcessInfo]:
    current_pid = os.getpid()
    by_pid = {proc.pid: proc for proc in processes}
    children: dict[int, list[int]] = {}
    for proc in processes:
        children.setdefault(proc.ppid, []).append(proc.pid)

    user_data_path = (
        target.workspace_root / ".reactron-user-data" / target.workspace_name
    ).as_posix()
    tokens = [
        target.workspace_root.as_posix(),
        target.app_path.as_posix(),
        user_data_path,
        target.workspace_name,
    ]
    if target.branch:
        tokens.append(target.branch)

    seed_pids: set[int] = set()
    for proc in processes:
        if proc.pid == current_pid or "designer_memwatch.py" in proc.args:
            continue
        reasons = [token_reason(token, proc.args) for token in tokens if token in proc.args]
        reasons = [reason for reason in reasons if reason]
        if reasons:
            proc.reasons = reasons
            seed_pids.add(proc.pid)

    matched: set[int] = set(seed_pids)
    stack = list(seed_pids)
    while stack:
        pid = stack.pop()
        for child_pid in children.get(pid, []):
            if child_pid in matched:
                continue
            matched.add(child_pid)
            child = by_pid[child_pid]
            parent = by_pid.get(pid)
            if parent and parent.reasons:
                child.reasons = parent.reasons + ["descendant"]
            else:
                child.reasons = ["descendant"]
            stack.append(child_pid)

    selected = [by_pid[pid] for pid in matched]
    selected.sort(key=lambda proc: proc.rss_kb, reverse=True)
    return selected


def token_reason(token: str, args: str) -> str | None:
    if token.endswith("/ide/designer"):
        return "designer-app-path"
    if "/.reactron-user-data/" in token:
        return "reactron-user-data"
    if token.startswith("/"):
        return "workspace-root"
    if token.startswith("review/"):
        return "review-branch"
    if token and token in args:
        return "workspace-name"
    return None


def capture_system_snapshot() -> dict:
    vm_stat = run_text_command(["vm_stat"])
    swapusage = run_text_command(["sysctl", "vm.swapusage"])
    memory_pressure = run_text_command(["memory_pressure", "-Q"])
    return {
        "summary": {
            "swapUsedMb": parse_swap_usage_mb(swapusage),
            "memoryFreePercent": parse_memory_free_percent(memory_pressure),
        },
        "artifacts": {
            "vm_stat": vm_stat,
            "swapusage": swapusage,
            "memory_pressure": memory_pressure,
        },
    }


def parse_swap_usage_mb(raw: str) -> float | None:
    match = re.search(r"used = ([0-9.]+)M", raw)
    if not match:
        return None
    return round(float(match.group(1)), 1)


def parse_memory_free_percent(raw: str) -> float | None:
    match = re.search(r"System-wide memory free percentage:\s*([0-9.]+)%", raw)
    if not match:
        return None
    return round(float(match.group(1)), 1)


def classify_incident(args: argparse.Namespace, sample: dict) -> list[str]:
    reasons: list[str] = []
    if sample["maxMatchedRssMb"] >= args.process_rss_threshold_mb:
        reasons.append("process-rss-threshold")
    if sample["totalMatchedRssMb"] >= args.group_rss_threshold_mb:
        reasons.append("group-rss-threshold")
    swap_used_mb = sample["system"]["swapUsedMb"]
    if (
        swap_used_mb is not None
        and swap_used_mb >= args.swap_threshold_mb
        and sample["totalMatchedRssMb"] >= 256
    ):
        reasons.append("swap-pressure")
    return reasons


def write_sample(sample: dict) -> None:
    persisted = dict(sample)
    matched_processes = persisted.pop("_matchedProcesses")
    system_artifacts = persisted.pop("_systemArtifacts")
    STATE_ROOT.mkdir(parents=True, exist_ok=True)
    CURRENT_PATH.write_text(
        json.dumps(persisted, indent=2) + "\n",
        encoding="utf-8",
    )
    with SAMPLES_PATH.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(persisted) + "\n")
    latest_system_dir = STATE_ROOT / "latest-system"
    latest_system_dir.mkdir(parents=True, exist_ok=True)
    write_text(latest_system_dir / "vm_stat.txt", system_artifacts["vm_stat"])
    write_text(latest_system_dir / "swapusage.txt", system_artifacts["swapusage"])
    write_text(
        latest_system_dir / "memory_pressure.txt",
        system_artifacts["memory_pressure"],
    )
    persist_sample_sqlite(persisted, matched_processes, system_artifacts)


def write_incident_bundle(
    target: WorkspaceTarget,
    sample: dict,
    reasons: list[str],
) -> pathlib.Path:
    timestamp = time.strftime(DATE_FORMAT, time.gmtime())
    incident_name = f"{timestamp}-{'-'.join(reasons)}"
    incident_dir = INCIDENTS_ROOT / incident_name
    incident_dir.mkdir(parents=True, exist_ok=False)

    persisted = dict(sample)
    persisted.pop("_matchedProcesses")
    system_artifacts = persisted.pop("_systemArtifacts")
    persisted["incidentReasons"] = reasons

    write_text(incident_dir / "summary.json", json.dumps(persisted, indent=2) + "\n")
    write_text(
        incident_dir / "top-processes.txt",
        render_top_processes(sample["topProcesses"]),
    )
    write_text(incident_dir / "vm_stat.txt", system_artifacts["vm_stat"])
    write_text(incident_dir / "swapusage.txt", system_artifacts["swapusage"])
    write_text(
        incident_dir / "memory_pressure.txt",
        system_artifacts["memory_pressure"],
    )

    forge_status = run_optional_json_command(["forge", "tip", "status", "designer", "--json"])
    if forge_status is not None:
        write_text(
            incident_dir / "forge-tip-status.json",
            json.dumps(forge_status, indent=2) + "\n",
        )

    runtime_snapshot = run_optional_text_command(
        [
            "f",
            "run",
            "--config",
            str(target.workspace_root / "flow.toml"),
            "runtime",
            "--",
            "--json",
        ]
    )
    if runtime_snapshot is not None:
        write_text(incident_dir / "designer-runtime.json", runtime_snapshot)

    persist_incident_sqlite(target, incident_dir, persisted, reasons)
    return incident_dir


def get_db_connection() -> sqlite3.Connection:
    global DB_CONNECTION
    if DB_CONNECTION is None:
        STATE_ROOT.mkdir(parents=True, exist_ok=True)
        DB_CONNECTION = sqlite3.connect(DB_PATH)
        DB_CONNECTION.execute("PRAGMA journal_mode=WAL")
        DB_CONNECTION.execute("PRAGMA synchronous=NORMAL")
        DB_CONNECTION.execute("PRAGMA foreign_keys=ON")
        DB_CONNECTION.executescript(
            """
            CREATE TABLE IF NOT EXISTS samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                captured_at TEXT NOT NULL,
                workspace_root TEXT NOT NULL,
                app_path TEXT NOT NULL,
                workspace_name TEXT NOT NULL,
                branch TEXT,
                matched_process_count INTEGER NOT NULL,
                total_matched_rss_mb REAL NOT NULL,
                max_matched_rss_mb REAL NOT NULL,
                max_matched_pid INTEGER,
                swap_used_mb REAL,
                memory_free_percent REAL,
                sample_json TEXT NOT NULL,
                system_artifacts_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_samples_captured_at
                ON samples(captured_at);
            CREATE INDEX IF NOT EXISTS idx_samples_workspace_root
                ON samples(workspace_root);
            CREATE TABLE IF NOT EXISTS sample_processes (
                sample_id INTEGER NOT NULL,
                rank_index INTEGER NOT NULL,
                pid INTEGER NOT NULL,
                ppid INTEGER NOT NULL,
                rss_mb REAL NOT NULL,
                vsz_mb REAL NOT NULL,
                cpu_percent REAL NOT NULL,
                elapsed TEXT NOT NULL,
                command TEXT NOT NULL,
                args TEXT NOT NULL,
                reasons_json TEXT NOT NULL,
                PRIMARY KEY (sample_id, rank_index),
                FOREIGN KEY(sample_id) REFERENCES samples(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_sample_processes_pid
                ON sample_processes(pid);
            CREATE TABLE IF NOT EXISTS incidents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                incident_path TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL,
                workspace_root TEXT NOT NULL,
                branch TEXT,
                reasons_json TEXT NOT NULL,
                summary_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_incidents_created_at
                ON incidents(created_at);
            CREATE TABLE IF NOT EXISTS daemon_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                occurred_at TEXT NOT NULL,
                level TEXT NOT NULL,
                event_type TEXT NOT NULL,
                message TEXT NOT NULL,
                data_json TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_daemon_events_occurred_at
                ON daemon_events(occurred_at);
            CREATE INDEX IF NOT EXISTS idx_daemon_events_type
                ON daemon_events(event_type);
            """
        )
    return DB_CONNECTION


def persist_sample_sqlite(
    sample: dict,
    matched_processes: list[dict],
    system_artifacts: dict,
) -> None:
    conn = get_db_connection()
    cursor = conn.execute(
        """
        INSERT INTO samples (
            captured_at,
            workspace_root,
            app_path,
            workspace_name,
            branch,
            matched_process_count,
            total_matched_rss_mb,
            max_matched_rss_mb,
            max_matched_pid,
            swap_used_mb,
            memory_free_percent,
            sample_json,
            system_artifacts_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            sample["timestamp"],
            sample["workspaceRoot"],
            sample["appPath"],
            sample["workspaceName"],
            sample["branch"],
            sample["matchedProcessCount"],
            sample["totalMatchedRssMb"],
            sample["maxMatchedRssMb"],
            sample["maxMatchedPid"],
            sample["system"]["swapUsedMb"],
            sample["system"]["memoryFreePercent"],
            json.dumps(sample, sort_keys=True),
            json.dumps(system_artifacts, sort_keys=True),
        ),
    )
    sample_id = cursor.lastrowid
    conn.executemany(
        """
        INSERT INTO sample_processes (
            sample_id,
            rank_index,
            pid,
            ppid,
            rss_mb,
            vsz_mb,
            cpu_percent,
            elapsed,
            command,
            args,
            reasons_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        [
            (
                sample_id,
                index,
                proc["pid"],
                proc["ppid"],
                proc["rssMb"],
                proc["vszMb"],
                proc["cpuPercent"],
                proc["elapsed"],
                proc["command"],
                proc["args"],
                json.dumps(proc["reasons"]),
            )
            for index, proc in enumerate(matched_processes)
        ],
    )
    conn.commit()


def persist_incident_sqlite(
    target: WorkspaceTarget,
    incident_dir: pathlib.Path,
    summary: dict,
    reasons: list[str],
) -> None:
    conn = get_db_connection()
    conn.execute(
        """
        INSERT OR REPLACE INTO incidents (
            incident_path,
            created_at,
            workspace_root,
            branch,
            reasons_json,
            summary_json
        ) VALUES (?, ?, ?, ?, ?, ?)
        """,
        (
            str(incident_dir),
            summary["timestamp"],
            str(target.workspace_root),
            target.branch,
            json.dumps(reasons),
            json.dumps(summary, sort_keys=True),
        ),
    )
    conn.commit()


def persist_event_sqlite(
    level: str,
    event_type: str,
    message: str,
    data: dict | None = None,
) -> None:
    conn = get_db_connection()
    conn.execute(
        """
        INSERT INTO daemon_events (
            occurred_at,
            level,
            event_type,
            message,
            data_json
        ) VALUES (?, ?, ?, ?, ?)
        """,
        (
            time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            level,
            event_type,
            message,
            json.dumps(data, sort_keys=True) if data is not None else None,
        ),
    )
    conn.commit()


def load_db_counts() -> dict:
    if not DB_PATH.exists():
        return {
            "samples": 0,
            "sampleProcesses": 0,
            "incidents": 0,
            "events": 0,
        }
    conn = get_db_connection()
    return {
        "samples": conn.execute("SELECT COUNT(*) FROM samples").fetchone()[0],
        "sampleProcesses": conn.execute("SELECT COUNT(*) FROM sample_processes").fetchone()[0],
        "incidents": conn.execute("SELECT COUNT(*) FROM incidents").fetchone()[0],
        "events": conn.execute("SELECT COUNT(*) FROM daemon_events").fetchone()[0],
    }


def emit_event(
    level: str,
    event_type: str,
    message: str,
    data: dict | None = None,
) -> None:
    normalized_message = str(message)
    print(normalized_message, flush=True)
    persist_event_sqlite(level, event_type, normalized_message, data)


def load_pid() -> int | None:
    if not PID_PATH.exists():
        return None
    raw = PID_PATH.read_text(encoding="utf-8").strip()
    if not raw:
        return None
    return int(raw)


def process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def find_latest_incident() -> pathlib.Path | None:
    if not INCIDENTS_ROOT.exists():
        return None
    incidents = sorted([path for path in INCIDENTS_ROOT.iterdir() if path.is_dir()])
    if not incidents:
        return None
    return incidents[-1]


def render_top_processes(processes: list[dict]) -> str:
    lines = []
    for proc in processes:
        lines.append(
            f"pid={proc['pid']} rss_mb={proc['rssMb']} cpu={proc['cpuPercent']} "
            f"etime={proc['elapsed']} reasons={','.join(proc['reasons'])}"
        )
        lines.append(f"command={proc['command']}")
        lines.append(f"args={proc['args']}")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def serialize_process(proc: ProcessInfo) -> dict:
    payload = asdict(proc)
    payload["rssMb"] = round(proc.rss_kb / 1024, 1)
    payload["vszMb"] = round(proc.vsz_kb / 1024, 1)
    payload["cpuPercent"] = proc.cpu_percent
    del payload["rss_kb"]
    del payload["vsz_kb"]
    del payload["cpu_percent"]
    del payload["elapsed"]
    payload["elapsed"] = proc.elapsed
    return payload


def run_text_command(command: list[str]) -> str:
    completed = subprocess.run(
        command,
        text=True,
        capture_output=True,
        check=True,
    )
    return completed.stdout


def run_json_command(command: list[str]) -> dict:
    output = run_text_command(command)
    return json.loads(output)


def run_optional_text_command(command: list[str]) -> str | None:
    try:
        return run_text_command(command)
    except subprocess.CalledProcessError as error:
        return (
            error.stdout
            + ("\n" if error.stdout and error.stderr else "")
            + error.stderr
        ).strip() or None
    except FileNotFoundError:
        return None


def run_optional_json_command(command: list[str]) -> dict | None:
    output = run_optional_text_command(command)
    if output is None:
        return None
    try:
        return json.loads(output)
    except json.JSONDecodeError:
        return None


def write_text(path: pathlib.Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
