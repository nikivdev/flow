use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cli::{KillOpts, ProcessOpts};
use crate::running;
use crate::tasks;

/// Show running processes for a project (or all projects)
pub fn show_project_processes(opts: ProcessOpts) -> Result<()> {
    if opts.all {
        show_all_processes()
    } else {
        let (config_path, cfg) = tasks::load_project_config(opts.config)?;
        let canonical = config_path.canonicalize()?;
        show_processes_for_project(&canonical, cfg.project_name.as_deref())
    }
}

fn show_processes_for_project(config_path: &Path, project_name: Option<&str>) -> Result<()> {
    let processes = running::get_project_processes(config_path)?;
    let project_root = config_path.parent().unwrap_or(Path::new("."));

    match project_name {
        Some(name) => println!("Project: {} ({})", name, project_root.display()),
        None => println!("Project: {}", project_root.display()),
    }

    if processes.is_empty() {
        println!("No running flow processes.");
        return Ok(());
    }

    println!("Running processes:");
    for proc in &processes {
        let runtime = format_runtime(proc.started_at);
        println!(
            "  {} [pid: {}, pgid: {}] - {}",
            proc.task_name, proc.pid, proc.pgid, runtime
        );
        println!("    {}", proc.command);
        if proc.used_flox {
            println!("    (flox environment)");
        }
    }

    Ok(())
}

fn show_all_processes() -> Result<()> {
    let all = running::load_running_processes()?;

    if all.projects.is_empty() {
        println!("No running flow processes.");
        return Ok(());
    }

    for (config_path, processes) in &all.projects {
        let project_name = processes
            .first()
            .and_then(|p| p.project_name.as_deref())
            .unwrap_or("unknown");
        let project_root = Path::new(config_path)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| config_path.clone());

        println!("\n{} ({}):", project_name, project_root);
        for proc in processes {
            let runtime = format_runtime(proc.started_at);
            println!("  {} [pid: {}] - {}", proc.task_name, proc.pid, runtime);
        }
    }

    Ok(())
}

fn format_runtime(started_at: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let elapsed_secs = ((now.saturating_sub(started_at)) / 1000) as u64;

    if elapsed_secs < 60 {
        format!("{}s", elapsed_secs)
    } else if elapsed_secs < 3600 {
        format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
    } else {
        format!("{}h {}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
    }
}

/// Kill processes based on options
pub fn kill_processes(opts: KillOpts) -> Result<()> {
    let (config_path, _cfg) = tasks::load_project_config(opts.config)?;
    let canonical = config_path.canonicalize()?;

    if let Some(pid) = opts.pid {
        kill_by_pid(pid, opts.force, opts.timeout)
    } else if let Some(task) = &opts.task {
        kill_by_task(&canonical, task, opts.force, opts.timeout)
    } else if opts.all {
        kill_all_for_project(&canonical, opts.force, opts.timeout)
    } else {
        bail!("Specify a task name, --pid <pid>, or --all")
    }
}

fn kill_by_pid(pid: u32, force: bool, timeout: u64) -> Result<()> {
    let processes = running::load_running_processes()?;

    // Find the process entry to get its PGID
    let entry = processes.projects.values().flatten().find(|p| p.pid == pid);

    let pgid = entry.map(|e| e.pgid).unwrap_or(pid);
    let task_name = entry.map(|e| e.task_name.as_str()).unwrap_or("unknown");

    terminate_process_group(pgid, force, timeout)?;
    running::unregister_process(pid)?;

    println!("Killed {} (pid: {}, pgid: {})", task_name, pid, pgid);
    Ok(())
}

fn kill_by_task(config_path: &Path, task: &str, force: bool, timeout: u64) -> Result<()> {
    let processes = running::get_project_processes(config_path)?;
    let matching: Vec<_> = processes.iter().filter(|p| p.task_name == task).collect();

    if matching.is_empty() {
        bail!("No running process found for task '{}'", task);
    }

    for proc in matching {
        terminate_process_group(proc.pgid, force, timeout)?;
        running::unregister_process(proc.pid)?;
        println!("Killed {} (pid: {})", proc.task_name, proc.pid);
    }

    Ok(())
}

fn kill_all_for_project(config_path: &Path, force: bool, timeout: u64) -> Result<()> {
    let processes = running::get_project_processes(config_path)?;

    if processes.is_empty() {
        println!("No running processes to kill.");
        return Ok(());
    }

    for proc in &processes {
        terminate_process_group(proc.pgid, force, timeout)?;
        running::unregister_process(proc.pid)?;
        println!("Killed {} (pid: {})", proc.task_name, proc.pid);
    }

    Ok(())
}

fn terminate_process_group(pgid: u32, force: bool, timeout: u64) -> Result<()> {
    #[cfg(unix)]
    {
        if force {
            // Immediate SIGKILL to process group
            Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", pgid))
                .status()
                .context("failed to send SIGKILL")?;
        } else {
            // Graceful SIGTERM to process group
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(format!("-{}", pgid))
                .status();

            // Wait for process to exit
            for _ in 0..timeout {
                thread::sleep(Duration::from_secs(1));
                if !running::process_alive(pgid) {
                    return Ok(());
                }
            }

            // Force kill if still alive
            if running::process_alive(pgid) {
                Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{}", pgid))
                    .status()
                    .context("failed to send SIGKILL after timeout")?;
            }
        }
    }

    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/PID", &pgid.to_string(), "/T", "/F"])
            .status()
            .context("failed to kill process tree")?;
    }

    Ok(())
}
