use std::path::PathBuf;
use std::process::Command;

use log::{info, warn};

use crate::config::AppConfig;
use crate::error::SyncError;

const LABEL: &str = "me.83noit.remtodo.agent";
const PLIST_NAME: &str = "me.83noit.remtodo.agent.plist";

fn plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_NAME)
}

fn log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("Logs")
        .join("remtodo.log")
}

/// Escape special XML characters in a string value.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Locate the reminders-helper binary using the same search order as SwiftCli,
/// but returning an absolute canonicalized path suitable for embedding in a plist.
fn find_reminders_helper() -> Option<PathBuf> {
    // 1. REMINDERS_HELPER env var
    if let Ok(path) = std::env::var("REMINDERS_HELPER") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return p.canonicalize().ok().or(Some(p));
        }
    }

    // 2. Sibling of current executable
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("reminders-helper");
            if sibling.exists() {
                return Some(sibling);
            }
        }
    }

    // 3. Swift build output relative to cwd
    for profile in ["release", "debug"] {
        let path = PathBuf::from(format!("swift/.build/{profile}/reminders-helper"));
        if path.exists() {
            return path.canonicalize().ok().or(Some(path));
        }
    }

    // 4. PATH lookup
    if let Ok(output) = Command::new("which").arg("reminders-helper").output() {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path_str.is_empty() {
                return Some(PathBuf::from(path_str));
            }
        }
    }

    None
}

/// Generate the launchd plist XML content.
pub fn generate_plist(app_config: &AppConfig, config_path: Option<&str>) -> String {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("remtodo"));

    let log = log_path();

    if !app_config.output.starts_with('/') && !app_config.output.starts_with('~') {
        warn!(
            "Config 'output' path '{}' is relative; it will not resolve correctly under launchd. Use an absolute or ~/-prefixed path.",
            app_config.output
        );
    }

    let mut prog_args = vec![
        format!(
            "        <string>{}</string>",
            xml_escape(&exe.to_string_lossy())
        ),
        "        <string>sync</string>".to_string(),
    ];
    if let Some(path) = config_path {
        prog_args.push("        <string>--config</string>".to_string());
        prog_args.push(format!("        <string>{}</string>", xml_escape(path)));
    }

    let helper_env_block = match find_reminders_helper() {
        Some(helper) => {
            info!("Found reminders-helper at {}", helper.display());
            format!(
                "\n    <key>EnvironmentVariables</key>\n    <dict>\n        <key>REMINDERS_HELPER</key>\n        <string>{}</string>\n    </dict>",
                xml_escape(&helper.to_string_lossy())
            )
        }
        None => {
            warn!(
                "reminders-helper not found; launchd agent may fail to sync. \
                 Build with: cd swift && swift build -c release"
            );
            String::new()
        }
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{prog_args}
    </array>
    <key>StartInterval</key>
    <integer>{interval}</integer>
    <key>RunAtLoad</key>
    <true/>{helper_env_block}
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        prog_args = prog_args.join("\n"),
        interval = app_config.poll_interval_secs,
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// Install (or reinstall) the launchd agent.
pub fn install(app_config: &AppConfig, config_path: Option<&str>) -> Result<(), SyncError> {
    let plist = plist_path();

    // Unload existing agent before overwriting plist (idempotent reinstall).
    if plist.exists() {
        info!("Existing plist found; unloading before reinstall...");
        let _ = Command::new("launchctl")
            .args(["unload", &plist.to_string_lossy()])
            .status();
    }

    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = generate_plist(app_config, config_path);
    std::fs::write(&plist, &content)?;
    info!("Wrote plist to {}", plist.display());

    let status = Command::new("launchctl")
        .args(["load", &plist.to_string_lossy()])
        .status()?;

    if !status.success() {
        return Err(SyncError::Config(format!(
            "launchctl load failed (exit code {:?})",
            status.code()
        )));
    }

    info!("Agent {} loaded", LABEL);
    println!("remtodo agent installed and loaded.\nRun 'remtodo status' to verify.");
    Ok(())
}

/// Unload and remove the launchd agent. No-op if the plist does not exist.
pub fn uninstall() -> Result<(), SyncError> {
    let plist = plist_path();

    if !plist.exists() {
        println!(
            "No plist found at {}; nothing to uninstall.",
            plist.display()
        );
        return Ok(());
    }

    let _ = Command::new("launchctl")
        .args(["unload", &plist.to_string_lossy()])
        .status();

    std::fs::remove_file(&plist)?;
    info!("Agent {} unloaded and plist removed", LABEL);
    println!("remtodo agent uninstalled.");
    Ok(())
}

/// Print the current status of the launchd agent.
pub fn status() {
    let plist = plist_path();
    let log = log_path();

    if !plist.exists() {
        println!(
            "Plist: {} (not found — run 'remtodo install' first)",
            plist.display()
        );
        return;
    }

    println!("Plist: {} (exists)", plist.display());

    match Command::new("launchctl").args(["list", LABEL]).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let pid = parse_launchctl_value(&stdout, "PID");
            let exit_status = parse_launchctl_value(&stdout, "LastExitStatus");

            match pid {
                Some(ref p) => println!("Status: running (PID {p})"),
                None => println!("Status: loaded (not currently running)"),
            }
            if let Some(ref s) = exit_status {
                if s == "0" {
                    println!("LastExitStatus: {s} (ok)");
                } else {
                    println!("LastExitStatus: {s} (non-zero — check logs)");
                }
            }
        }
        Ok(_) => println!("Status: not loaded"),
        Err(e) => println!("Failed to run launchctl: {e}"),
    }

    if log.exists() {
        println!("\nLast 5 lines of {}:", log.display());
        match std::fs::read_to_string(&log) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(5);
                for line in &lines[start..] {
                    println!("  {line}");
                }
            }
            Err(e) => println!("  (could not read log: {e})"),
        }
    } else {
        println!("\nLog file not yet created: {}", log.display());
    }
}

/// Parse a value from `launchctl list <label>` output.
///
/// The output format uses lines like: `"PID" = 12345;`
fn parse_launchctl_value(output: &str, key: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("\"{key}\"")) {
            if let Some(eq_pos) = trimmed.find('=') {
                let value = trimmed[eq_pos + 1..]
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .trim_matches('"');
                return Some(value.to_string());
            }
        }
    }
    None
}
