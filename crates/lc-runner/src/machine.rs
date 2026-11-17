//! Things a Runner reports about, and does to, the computer it runs on:
//! identity (for the operator and for Wake-on-LAN), its recent log, and
//! restarting or shutting down the machine.

use std::collections::VecDeque;
use std::io::Write;
use std::net::IpAddr;
use std::sync::Mutex;

use lc_proto::MachineInfo;

/// Exit code by which a Runner asks its supervisor to start it again.
pub const RESTART_EXIT_CODE: i32 = 75;
/// Exit code asking the supervisor to end too (`RunnerAction::Exit`).
pub const EXIT_FOR_GOOD_CODE: i32 = 77;
/// Environment variable through which the supervisor tells the Runner the
/// UDP port it waits for start requests on.
pub const LAUNCHER_PORT_ENV: &str = "LCPLAY_LAUNCHER_PORT";

/// Lines of log kept for the operator.
const LOG_LINES: usize = 400;

static LOG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// The log file, once [`persist_log`] has opened it.
static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Largest log file kept before it is rotated (to `runner.log.1`).
const LOG_FILE_MAX: u64 = 5 << 20;

/// Remove terminal colour codes (`ESC [ … m`): they belong on a console,
/// not in a file or in the Producer's log window.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, up to the final letter.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Also keep the log in `path`, so that it survives the Runner — the lines
/// before a crash are the interesting ones, and a restarted Runner would
/// otherwise start with an empty log. With `load_previous`, the end of the
/// existing file is put back into the in-memory log first.
pub fn persist_log(path: &std::path::Path, load_previous: bool) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_FILE_MAX) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    if load_previous {
        if let Ok(previous) = std::fs::read_to_string(path) {
            let lines: Vec<&str> = previous.lines().collect();
            let tail = &lines[lines.len().saturating_sub(LOG_LINES / 2)..];
            if !tail.is_empty() {
                let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
                log.extend(tail.iter().map(|l| l.to_string()));
                log.push_back("──────── runner started ────────".to_string());
                while log.len() > LOG_LINES {
                    log.pop_front();
                }
            }
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    match file {
        Ok(f) => *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(f),
        Err(e) => tracing::warn!("cannot write the log file {}: {e}", path.display()),
    }
}

/// Send panics (with their location) to the log, not only to stderr: a
/// crash must be readable from the Producer after the supervisor restarts
/// the Runner.
pub fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("PANIC: {info}");
        default(info);
    }));
}

/// A log destination that writes to stderr as usual and also keeps the last
/// lines in memory, so the operator can read a Runner's log from the
/// Producer without walking to the machine.
#[derive(Clone, Copy)]
pub struct LogWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogLine;
    fn make_writer(&'a self) -> LogLine {
        LogLine(Vec::new())
    }
}

/// One formatted log event, collected and then written out on drop.
pub struct LogLine(Vec<u8>);

impl Write for LogLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogLine {
    fn drop(&mut self) {
        let _ = std::io::stderr().write_all(&self.0);
        let text = strip_ansi(&String::from_utf8_lossy(&self.0));
        if let Some(file) = LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = file.write_all(text.as_bytes());
        }
        let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if log.len() == LOG_LINES {
                log.pop_front();
            }
            log.push_back(line.to_string());
        }
    }
}

/// The last log lines, oldest first.
pub fn recent_log() -> Vec<String> {
    LOG.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Describe this machine. `local_ip` is the address this Runner talks to
/// the Director from: its network card is the one whose hardware address
/// matters for Wake-on-LAN.
pub fn describe(local_ip: Option<IpAddr>, allow_power: bool, supervised: bool) -> MachineInfo {
    let mac = local_ip.and_then(|ip| {
        netdev::get_interfaces()
            .into_iter()
            .find(|i| {
                i.ipv4.iter().any(|n| IpAddr::V4(n.addr()) == ip)
                    || i.ipv6.iter().any(|n| IpAddr::V6(n.addr()) == ip)
            })
            .and_then(|i| i.mac_addr)
            .map(|m| m.to_string())
            .filter(|m| m != "00:00:00:00:00:00")
    });
    MachineInfo {
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        os: std::env::consts::OS.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ip: local_ip.map(|ip| ip.to_string()).unwrap_or_default(),
        mac,
        allow_power,
        supervised,
        launcher_port: std::env::var(LAUNCHER_PORT_ENV)
            .ok()
            .and_then(|p| p.parse().ok()),
    }
}

/// Restart (`reboot = true`) or shut down the computer, now.
pub fn power(reboot: bool) -> anyhow::Result<()> {
    let mut command;
    #[cfg(windows)]
    {
        command = std::process::Command::new("shutdown");
        command.args([if reboot { "/r" } else { "/s" }, "/t", "0"]);
    }
    #[cfg(target_os = "macos")]
    {
        command = std::process::Command::new("osascript");
        command.args([
            "-e",
            if reboot {
                "tell app \"System Events\" to restart"
            } else {
                "tell app \"System Events\" to shut down"
            },
        ]);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        command = std::process::Command::new("systemctl");
        command.arg(if reboot { "reboot" } else { "poweroff" });
    }
    let status = command.status()?;
    if !status.success() {
        anyhow::bail!(
            "the system refused ({status}); the user running the Runner may lack the permission"
        );
    }
    Ok(())
}

/// Start this Runner when the user logs in, with the options it was given
/// now (minus `--autostart`), or stop doing so.
///
/// Windows: the `LC Play Runner` value under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Linux: an XDG
/// autostart entry. Per user, no administrator rights needed.
pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("locating this program")?;
    let args = autostart_args(
        std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
    );
    #[cfg(windows)]
    let quote = windows_quote;
    #[cfg(not(windows))]
    let quote = desktop_quote;
    let command_line = std::iter::once(exe.display().to_string())
        .chain(args)
        .map(|a| quote(&a))
        .collect::<Vec<_>>()
        .join(" ");

    #[cfg(windows)]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut cmd = std::process::Command::new("reg");
        if on {
            cmd.args([
                "add",
                key,
                "/v",
                "LC Play Runner",
                "/t",
                "REG_SZ",
                "/d",
                &command_line,
                "/f",
            ]);
        } else {
            cmd.args(["delete", key, "/v", "LC Play Runner", "/f"]);
        }
        let status = cmd.status().context("running reg.exe")?;
        anyhow::ensure!(status.success() || !on, "reg.exe failed ({status})");
    }
    #[cfg(not(windows))]
    {
        let dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")))
            .context("no home folder")?
            .join("autostart");
        let file = dir.join("lc-play-runner.desktop");
        if on {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                &file,
                format!(
                    "[Desktop Entry]\nType=Application\nName=LC Play Runner\nExec={command_line}\n"
                ),
            )?;
        } else {
            let _ = std::fs::remove_file(&file);
        }
    }
    println!(
        "LC Play Runner {} start at login{}",
        if on { "will" } else { "will no longer" },
        if on {
            format!(": {command_line}")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// The options to start with at login: these, minus `--autostart` and its
/// value, with relative paths (the show file, `--cache-dir`) made absolute —
/// at login the current folder is a different one.
fn autostart_args(args: impl Iterator<Item = String>) -> Vec<String> {
    const TAKES_VALUE: [&str; 4] = ["--director", "--name", "--interface", "--cache-dir"];
    let absolute = |p: &str| {
        std::path::absolute(p).map_or_else(|_| p.to_string(), |a| a.display().to_string())
    };
    let mut out = Vec::new();
    let mut it = args;
    while let Some(a) = it.next() {
        if a == "--autostart" {
            it.next();
        } else if a.starts_with("--autostart=") {
            // dropped
        } else if a == "--cache-dir" {
            out.push(a);
            if let Some(v) = it.next() {
                out.push(absolute(&v));
            }
        } else if let Some(v) = a.strip_prefix("--cache-dir=") {
            out.push(format!("--cache-dir={}", absolute(v)));
        } else if TAKES_VALUE.contains(&a.as_str()) {
            out.push(a);
            if let Some(v) = it.next() {
                out.push(v);
            }
        } else if !a.starts_with('-') {
            out.push(absolute(&a)); // the show file
        } else {
            out.push(a);
        }
    }
    out
}

/// Quote one argument for a Windows command line, by the rules programs
/// use to split it (`CommandLineToArgvW`): quotes around anything empty or
/// with spaces, tabs or quotes; backslashes doubled before a quote.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Quote one argument for the `Exec=` line of a desktop entry.
#[cfg_attr(windows, allow(dead_code))]
fn desktop_quote(arg: &str) -> String {
    let mut out = String::from('"');
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_quoting_handles_awkward_arguments() {
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote(""), "\"\"");
        // A trailing backslash must not escape the closing quote.
        assert_eq!(windows_quote("D:\\LC Play\\"), "\"D:\\LC Play\\\\\"");
        assert_eq!(windows_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn autostart_keeps_options_and_makes_paths_absolute() {
        let args = [
            "--autostart",
            "on",
            "--name",
            "left",
            "--cache-dir",
            "rel",
            "show.json",
        ]
        .map(String::from);
        let out = autostart_args(args.into_iter());
        assert_eq!(out[0..2], ["--name", "left"]);
        assert_eq!(out[2], "--cache-dir");
        assert!(std::path::Path::new(&out[3]).is_absolute());
        assert!(std::path::Path::new(&out[4]).is_absolute());
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn colour_codes_are_stripped() {
        let coloured = "\u{1b}[2m2026-11-15T21:53:24Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m runner: ok";
        assert_eq!(
            strip_ansi(coloured),
            "2026-11-15T21:53:24Z  INFO runner: ok"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
