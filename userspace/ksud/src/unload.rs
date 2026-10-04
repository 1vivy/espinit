use anyhow::Result;
use log::{info, warn};
use std::fs;
use std::process::Command;

use crate::utils;

/// Find PIDs of processes running in the espinit domain (u:r:espinit:s0).
/// Returns a list of PIDs excluding our own.
fn find_espinit_domain_pids() -> Vec<i32> {
    let my_pid = std::process::id() as i32;
    let mut pids = Vec::new();

    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == my_pid {
            continue;
        }

        let attr_path = format!("/proc/{pid}/attr/current");
        if let Ok(context) = fs::read_to_string(&attr_path) {
            let context = context.trim().trim_end_matches('\0');
            if context == "u:r:espinit:s0" {
                pids.push(pid);
            }
        }
    }

    pids
}

/// Find PIDs of processes holding espinit driver or wrapper file descriptors.
/// Returns a list of PIDs excluding our own.
fn find_espinit_fd_holders() -> Vec<i32> {
    let my_pid = std::process::id() as i32;
    let mut pids = Vec::new();

    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == my_pid {
            continue;
        }

        let fd_dir = format!("/proc/{pid}/fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };

        for fd_entry in fds.flatten() {
            let link_path = fd_entry.path();
            if let Ok(target) = fs::read_link(&link_path) {
                let target_str = target.to_string_lossy();
                if target_str.contains("[espinit") {
                    pids.push(pid);
                    break;
                }
            }
        }
    }

    pids
}

fn kill_pids(pids: &[i32], signal: i32) {
    for &pid in pids {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// Close all espinit driver and wrapper fds held by the current process.
fn close_espinit_fds() {
    let Ok(entries) = fs::read_dir("/proc/self/fd") else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Ok(target) = fs::read_link(entry.path()) {
            let target_str = target.to_string_lossy();
            if target_str.contains("[espinit") {
                info!("unload: closing fd {fd} -> {target_str}");
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

pub fn unload() -> Result<()> {
    info!("unload: starting espinit unload sequence");

    // 0. Switch cgroups so we don't get killed along with our parent shell
    utils::switch_cgroups();

    // 1. stop (Android init stop command - stops all services)
    info!("unload: stopping Android services...");
    let _ = Command::new("stop").status();

    // 2. Kill all espinit domain processes and processes holding espinit fds (except ourselves)
    info!("unload: killing espinit domain processes...");
    let domain_pids = find_espinit_domain_pids();
    if !domain_pids.is_empty() {
        info!(
            "unload: found {} espinit domain processes, sending SIGKILL",
            domain_pids.len()
        );
        kill_pids(&domain_pids, libc::SIGKILL);
    }

    info!("unload: killing processes holding espinit fds...");
    let fd_pids = find_espinit_fd_holders();
    if !fd_pids.is_empty() {
        info!(
            "unload: found {} processes holding espinit fds, sending SIGKILL",
            fd_pids.len()
        );
        kill_pids(&fd_pids, libc::SIGKILL);
    }

    // 3. Close all our own espinit driver and wrapper fds
    info!("unload: closing all espinit fds...");
    close_espinit_fds();

    // 4. delete_module("espinit")
    info!("unload: removing espinit module...");
    if let Err(e) = rustix::system::delete_module(c"espinit", 0) {
        warn!("unload: delete_module espinit failed: {e}");
    }

    // 5. start (Android init start command - restarts all services)
    info!("unload: restarting Android services...");
    let _ = Command::new("start").status();

    // 6. Exit
    info!("unload: done, exiting espinitd");
    std::process::exit(0);
}
