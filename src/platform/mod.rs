//! Platform-specific process and filesystem operations.
//!
//! Centralizes OS-dependent behavior behind a clean boundary so core
//! modules don't scatter `#[cfg]` branches through product logic.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: u32,
    pub processes: Vec<ForegroundProcess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

pub(crate) fn detached_custom_command_process(command: &str) -> std::process::Command {
    let mut process = detached_custom_command_process_platform(command);
    configure_background_command(&mut process);
    process
}

pub(crate) fn pane_custom_command_pty_builder(command: &str) -> portable_pty::CommandBuilder {
    pane_custom_command_pty_builder_platform(command)
}

pub(crate) fn configure_background_command(command: &mut std::process::Command) {
    configure_background_command_platform(command);
}

#[cfg(not(windows))]
fn configure_background_command_platform(_command: &mut std::process::Command) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformCapabilities {
    pub(crate) live_handoff: bool,
    pub(crate) remote_attach: bool,
    pub(crate) direct_terminal_attach: bool,
    pub(crate) preserve_legacy_doubled_escape_input: bool,
}

pub(crate) const fn capabilities() -> PlatformCapabilities {
    PlatformCapabilities {
        live_handoff: cfg!(unix),
        remote_attach: cfg!(unix),
        direct_terminal_attach: cfg!(unix),
        preserve_legacy_doubled_escape_input: cfg!(target_os = "macos"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn current_process_is_detached_server_daemon() -> bool {
    unsafe { libc::getsid(0) == libc::getpid() }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
// Windows does not wire clipboard-image bridging into semantic input yet.
#[cfg_attr(windows, allow(dead_code))]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

#[cfg(unix)]
pub(crate) fn read_limited_reader(
    mut reader: impl std::io::Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_len = remaining.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if bytes_read == 0 {
            return if bytes.is_empty() {
                Ok(LimitedRead::Empty)
            } else {
                Ok(LimitedRead::Complete(bytes))
            };
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let mut sentinel = [0_u8; 1];
    loop {
        return match reader.read(&mut sentinel) {
            Ok(0) if bytes.is_empty() => Ok(LimitedRead::Empty),
            Ok(0) => Ok(LimitedRead::Complete(bytes)),
            Ok(_) => Ok(LimitedRead::Oversized),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

// ---------------------------------------------------------------------------
// System monitor metrics (CPU / RAM / GPU)
//
// Presentation-only sampling for the optional top-of-space monitor strip. The
// neutral types and the pure parsers live here (unit-tested without touching
// the filesystem); the real `/proc` and GPU reads live in the per-OS modules.
// Non-Linux targets fall back to "unavailable" so the strip simply stays empty.
// ---------------------------------------------------------------------------

/// A CPU busy/idle snapshot from `/proc/stat`, differenced across ticks to get a
/// non-blocking busy percentage (no in-sample `sleep`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuSnapshot {
    pub total: u64,
    pub idle: u64,
}

/// A GPU utilization sample. `vram_pct` is `None` when memory info is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuSample {
    pub util_pct: u8,
    pub vram_pct: Option<u8>,
}

/// One system-monitor sample. Each field is `None` when that metric could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemSample {
    pub cpu_pct: Option<u8>,
    pub ram_pct: Option<u8>,
    pub gpu: Option<GpuSample>,
}

/// Parse the aggregate `cpu` line of `/proc/stat` into a snapshot. `idle`
/// folds in `iowait`; `total` sums every reported class.
pub fn parse_proc_stat_cpu(contents: &str) -> Option<CpuSnapshot> {
    let line = contents.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields.filter_map(|field| field.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal guest guest_nice
    if values.len() < 4 {
        return None;
    }
    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    let total: u64 = values.iter().copied().fold(0u64, u64::saturating_add);
    Some(CpuSnapshot { total, idle })
}

/// Compute a CPU busy percentage from two `/proc/stat` snapshots. Returns
/// `None` when the counters did not advance (or went backwards after a reset).
pub fn cpu_pct_from_delta(prev: CpuSnapshot, cur: CpuSnapshot) -> Option<u8> {
    let total = cur.total.checked_sub(prev.total)?;
    let idle = cur.idle.checked_sub(prev.idle)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    Some((busy.saturating_mul(100) / total).min(100) as u8)
}

/// Parse `/proc/meminfo` into a used-memory percentage from `MemTotal` and
/// `MemAvailable` (the kernel's own "available" estimate, matching `free`).
pub fn parse_proc_meminfo_pct(contents: &str) -> Option<u8> {
    let mut total_kb = None;
    let mut avail_kb = None;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok());
        }
        if total_kb.is_some() && avail_kb.is_some() {
            break;
        }
    }
    let total = total_kb?;
    if total == 0 {
        return None;
    }
    let avail = avail_kb?.min(total);
    let used = total - avail;
    Some((used.saturating_mul(100) / total) as u8)
}

/// Parse one CSV row from `nvidia-smi --query-gpu=utilization.gpu,memory.used,
/// memory.total --format=csv,noheader,nounits`.
pub fn parse_nvidia_smi_line(line: &str) -> Option<GpuSample> {
    let mut fields = line.split(',').map(str::trim);
    let util: u8 = fields.next()?.parse().ok()?;
    let used: f64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let total: f64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let vram_pct = (total > 0.0).then(|| ((used / total) * 100.0).round().clamp(0.0, 100.0) as u8);
    Some(GpuSample {
        util_pct: util.min(100),
        vram_pct,
    })
}

/// Parse an AMD `gpu_busy_percent` sysfs value (a bare integer percentage).
pub fn parse_amd_gpu_busy(contents: &str) -> Option<GpuSample> {
    let util: u8 = contents.trim().parse().ok()?;
    Some(GpuSample {
        util_pct: util.min(100),
        vram_pct: None,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read_cpu_snapshot() -> Option<CpuSnapshot> {
    None
}
#[cfg(not(target_os = "linux"))]
pub fn read_ram_pct() -> Option<u8> {
    None
}
#[cfg(not(target_os = "linux"))]
pub fn read_gpu_sample() -> Option<GpuSample> {
    None
}
#[cfg(not(target_os = "linux"))]
pub fn gpu_monitor_supported() -> bool {
    false
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn available_pane_shell_from_job(child_pid: u32, job: ForegroundJob) -> Option<String> {
    if job.process_group_id != child_pid
        || job.processes.iter().any(|process| process.pid != child_pid)
    {
        return None;
    }
    job.processes
        .into_iter()
        .find(|process| process.pid == child_pid)
        .map(|process| process.name)
        .filter(|name| is_pane_shell_process_name(name))
}

fn normalized_process_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn is_powershell_process_name(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "pwsh" | "powershell"
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn interactive_unix_shell_command(
    argv: &[String],
    shell_name: &str,
    quote_posix_arg: fn(&str) -> String,
) -> Option<String> {
    let quote = if is_powershell_process_name(shell_name) {
        quote_powershell_arg
    } else {
        quote_posix_arg
    };
    let mut parts = argv.iter();
    let mut command = quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&quote(part));
    }
    Some(command)
}

pub(crate) fn quote_powershell_arg(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'=')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = normalized_process_name(name);
    matches!(
        normalized.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "elvish"
            | "xonsh"
            | "nu"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_agent_hint(_pid: u32) -> Option<crate::detect::Agent> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn parse_agent_env_hint(environ: &[u8]) -> Option<crate::detect::Agent> {
    for record in environ.split(|&byte| byte == 0) {
        let Some(value) = record.strip_prefix(b"HERDR_AGENT=") else {
            continue;
        };
        return crate::detect::parse_agent_label(std::str::from_utf8(value).ok()?);
    }
    None
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub(crate) struct InputSourceRestore;

#[cfg(not(target_os = "macos"))]
pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    None
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn pump_input_source_runloop() {}

/// Switches the host keyboard input source while prefix mode is active.
///
/// `App` drives this through a trait so the prefix-mode transitions can be
/// tested with a fake, without touching the real macOS APIs or leaking a
/// platform-specific restore type into `App`.
pub(crate) trait PrefixInputSource {
    /// Switch to an ASCII-capable input source for prefix commands. No-op if
    /// the current source is already ASCII-capable, the platform is
    /// unsupported, or the switch fails. Calling it again before `restore`
    /// keeps the source saved by the first call.
    fn switch_to_ascii(&mut self);

    /// Restore whatever `switch_to_ascii` saved. No-op if nothing was switched.
    fn restore(&mut self);
}

/// Production [`PrefixInputSource`] backed by the per-platform API.
#[derive(Default)]
pub(crate) struct RealPrefixInputSource {
    restore: Option<InputSourceRestore>,
}

impl PrefixInputSource for RealPrefixInputSource {
    fn switch_to_ascii(&mut self) {
        if self.restore.is_none() {
            // Drain pending input-source-change notifications so the read below is fresh (see
            // `pump_input_source_runloop`); a no-op on non-macOS.
            pump_input_source_runloop();
            self.restore = switch_to_ascii_input_source();
        }
    }

    fn restore(&mut self) {
        let _ = self.restore.take();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn pane_shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish", "pwsh", "powershell.exe"] {
            assert!(is_pane_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_pane_shell_process_name(program), "{program}");
        }
    }

    #[test]
    fn detached_custom_command_preserves_unix_login_shell_flag() {
        let cmd = detached_custom_command_process("echo hello");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-lc"),
                std::ffi::OsStr::new("echo hello")
            ]
        );
    }

    #[test]
    fn pane_custom_command_builder_preserves_unix_shell_flag() {
        let expected: Vec<std::ffi::OsString> =
            vec!["/bin/sh".into(), "-c".into(), "echo hello".into()];
        assert_eq!(
            pane_custom_command_pty_builder("echo hello").get_argv(),
            &expected
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_accepts_known_agents() {
        assert_eq!(
            parse_agent_env_hint(b"PATH=/bin\0HERDR_AGENT=claude\0TERM=xterm\0"),
            Some(crate::detect::Agent::Claude)
        );
        assert_eq!(
            parse_agent_env_hint(b"HERDR_AGENT=codex"),
            Some(crate::detect::Agent::Codex)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_ignores_missing_or_unknown_agents() {
        assert_eq!(parse_agent_env_hint(b"PATH=/bin\0TERM=xterm\0"), None);
        assert_eq!(parse_agent_env_hint(b"HERDR_AGENT=not-an-agent\0"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn interactive_shell_command_quotes_for_posix_and_powershell() {
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "a'b".into(),
            "$HOME".into(),
            "semi;colon".into(),
            "@options".into(),
        ];
        assert_eq!(
            interactive_shell_command(&argv, "bash").as_deref(),
            Some("pi '' 'two words' 'a'\\''b' '$HOME' 'semi;colon' @options")
        );
        assert_eq!(
            interactive_shell_command(&argv, "pwsh").as_deref(),
            Some("pi '' 'two words' 'a''b' '$HOME' 'semi;colon' '@options'")
        );
    }

    #[test]
    fn read_limited_reader_returns_complete_data_under_limit() {
        let input = std::io::Cursor::new(b"image".to_vec());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_returns_empty_for_empty_input() {
        let input = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Empty
        );
    }

    #[test]
    fn read_limited_reader_accepts_data_exactly_at_limit() {
        let input = std::io::Cursor::new(b"four".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Complete(b"four".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_rejects_data_over_limit() {
        let input = std::io::Cursor::new(b"oversized".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Oversized
        );
    }

    #[test]
    fn parse_proc_stat_cpu_reads_aggregate_line() {
        let snap = parse_proc_stat_cpu("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4")
            .expect("aggregate cpu line");
        // total = 100+50+800+50 = 1000; idle = 800+50 = 850
        assert_eq!(snap.total, 1000);
        assert_eq!(snap.idle, 850);
    }

    #[test]
    fn parse_proc_stat_cpu_rejects_non_cpu_first_line() {
        assert!(parse_proc_stat_cpu("intr 1 2 3\ncpu 1 2 3 4").is_none());
    }

    #[test]
    fn cpu_pct_from_delta_computes_busy_fraction() {
        let prev = CpuSnapshot {
            total: 1000,
            idle: 900,
        };
        let cur = CpuSnapshot {
            total: 1200,
            idle: 1050,
        };
        // delta total 200, delta idle 150 -> busy 50 -> 25%
        assert_eq!(cpu_pct_from_delta(prev, cur), Some(25));
    }

    #[test]
    fn cpu_pct_from_delta_handles_no_progress_and_reset() {
        let snap = CpuSnapshot {
            total: 1000,
            idle: 900,
        };
        assert_eq!(cpu_pct_from_delta(snap, snap), None);
        let later = CpuSnapshot {
            total: 500,
            idle: 400,
        };
        assert_eq!(cpu_pct_from_delta(snap, later), None);
    }

    #[test]
    fn parse_proc_meminfo_pct_uses_total_and_available() {
        let contents = "MemTotal:       1000 kB\nMemFree: 100 kB\nMemAvailable:    250 kB\n";
        // used = 1000 - 250 = 750 -> 75%
        assert_eq!(parse_proc_meminfo_pct(contents), Some(75));
    }

    #[test]
    fn parse_proc_meminfo_pct_requires_both_fields() {
        assert!(parse_proc_meminfo_pct("MemTotal: 1000 kB\n").is_none());
    }

    #[test]
    fn parse_nvidia_smi_line_reads_util_and_vram() {
        let sample = parse_nvidia_smi_line("7, 4096, 24564").expect("nvidia line");
        assert_eq!(sample.util_pct, 7);
        assert_eq!(sample.vram_pct, Some(17));
    }

    #[test]
    fn parse_nvidia_smi_line_without_memory_hides_vram() {
        let sample = parse_nvidia_smi_line("42").expect("nvidia line");
        assert_eq!(sample.util_pct, 42);
        assert_eq!(sample.vram_pct, None);
    }

    #[test]
    fn parse_amd_gpu_busy_reads_percentage() {
        let sample = parse_amd_gpu_busy("63\n").expect("amd busy");
        assert_eq!(sample.util_pct, 63);
        assert_eq!(sample.vram_pct, None);
    }

    #[test]
    fn read_limited_reader_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: std::io::Cursor<Vec<u8>>,
        }

        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.inner.read(buffer)
            }
        }

        let input = InterruptedOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"image".to_vec()),
        };
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }
}
