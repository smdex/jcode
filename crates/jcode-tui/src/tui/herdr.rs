//! Native agent-state reporting for the herdr terminal multiplexer.
//!
//! When jcode runs inside a herdr pane (`HERDR_ENV=1`), it reports its state
//! (`idle` / `working`), the command that resumes the current session, and
//! releases the pane when the user quits. The interactive TUI never blocks a
//! turn on a user decision, so `blocked` is not reported. Subagent counts,
//! names and tasks use display-only metadata, refreshed before its TTL. See
//! <https://herdr.dev/docs/add-herdr-support/>.
//!
//! Reports go through `"$HERDR_BIN_PATH" pane report-agent ...` on a single
//! background thread. Only the latest pending report is kept, every command
//! has a short timeout, and failures are ignored so herdr can never slow down
//! or break the TUI. Outside herdr every entry point is a no-op.

use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Stable integration id. Must not start with `herdr:`.
const SOURCE: &str = "jcode";
/// Agent name shown in herdr's sidebar and `herdr agent list`.
const AGENT: &str = "jcode";
/// Upper bound on a single herdr CLI call.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const REFRESH_INTERVAL: Duration = Duration::from_secs(45);
const METADATA_TTL_MS: &str = "120000";
const METADATA_SOURCE: &str = "jcode:subagents";
/// herdr's resume argv limits.
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MetadataReport {
    pub summary: Option<String>,
    pub title_suffix: Option<String>,
}

/// A state report plus, when known, the session that `resume_argv` reopens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateReport {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub resume_argv: Option<Vec<String>>,
    pub metadata: MetadataReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HerdrEnv {
    bin: String,
    pane_id: String,
}

impl HerdrEnv {
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let non_empty = |key: &str| get(key).filter(|value| !value.trim().is_empty());
        if non_empty("HERDR_ENV").as_deref() != Some("1") {
            return None;
        }
        // The socket must exist for the CLI to reach herdr; require it as the
        // docs do, even though we talk to herdr through its binary.
        non_empty("HERDR_SOCKET_PATH")?;
        Some(Self {
            bin: non_empty("HERDR_BIN_PATH")?,
            pane_id: non_empty("HERDR_PANE_ID")?,
        })
    }
}

#[derive(Default)]
struct Slot {
    pending: Option<StateReport>,
    released: bool,
}

struct Reporter {
    env: HerdrEnv,
    slot: Arc<(Mutex<Slot>, Condvar)>,
    last_submitted: Mutex<Option<StateReport>>,
}

static REPORTER: OnceLock<Option<Reporter>> = OnceLock::new();
static LAST_SEQ: Mutex<u64> = Mutex::new(0);
/// Set once herdr rejects a report carrying a resume command. herdr older
/// than 0.9.2 fails the whole report with a usage error on the `--` resume
/// separator, so after that we keep reporting state without it.
static RESUME_UNSUPPORTED: AtomicBool = AtomicBool::new(false);
static METADATA_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

fn reporter() -> Option<&'static Reporter> {
    REPORTER
        .get_or_init(|| {
            let env = HerdrEnv::from_lookup(|key| std::env::var(key).ok())?;
            crate::logging::info(&format!(
                "herdr: reporting agent state for pane {}",
                env.pane_id
            ));
            let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
            let worker_slot = Arc::clone(&slot);
            let worker_env = env.clone();
            let spawned = std::thread::Builder::new()
                .name("herdr-reporter".into())
                .spawn(move || worker_loop(worker_env, worker_slot, REFRESH_INTERVAL));
            if let Err(error) = spawned {
                crate::logging::warn(&format!("herdr: failed to start reporter: {error}"));
                return None;
            }
            Some(Reporter {
                env,
                slot,
                last_submitted: Mutex::new(None),
            })
        })
        .as_ref()
}

/// Avoid building display metadata outside a Herdr pane.
pub fn enabled() -> bool {
    reporter().is_some()
}

/// Queue the newest snapshot; unchanged reports are dropped, and the worker
/// retries failures and refreshes metadata without requiring another UI event.
pub fn sync(state: AgentState, session_id: Option<&str>, metadata: MetadataReport) {
    let Some(reporter) = reporter() else {
        return;
    };
    let report = {
        let mut last = lock(&reporter.last_submitted);
        if last.as_ref().is_some_and(|last| {
            last.state == state
                && last.session_id.as_deref() == session_id
                && last.metadata == metadata
        }) {
            return;
        }
        let report = StateReport {
            state,
            session_id: session_id.map(str::to_owned),
            resume_argv: session_id
                .and_then(|id| resume_argv_for_session(id, |key| std::env::var(key).ok())),
            metadata,
        };
        *last = Some(report.clone());
        report
    };
    let (slot, wake) = &*reporter.slot;
    let mut slot = lock(slot);
    if slot.released {
        return;
    }
    slot.pending = Some(report);
    wake.notify_one();
}

/// Release the pane because the user is quitting jcode. Blocks for at most
/// two [`COMMAND_TIMEOUT`] calls (metadata cleanup and lifecycle release).
/// Do not call this when jcode is about to exec itself
/// (reload/update) since the same agent keeps running in the pane.
pub fn release() {
    let Some(reporter) = reporter() else {
        return;
    };
    release_reporter(reporter);
}

fn release_reporter(reporter: &Reporter) {
    let (metadata_seq, state_seq) = {
        let mut slot = lock(&reporter.slot.0);
        if slot.released {
            return;
        }
        slot.released = true;
        slot.pending = None;
        reporter.slot.1.notify_one();
        (next_seq(), next_seq())
    };
    // In-flight reports reserved their sequences under the same slot lock,
    // so neither an old working state nor old metadata can undo this release.
    send_metadata(&reporter.env, &MetadataReport::default(), metadata_seq);
    run_herdr(
        &reporter.env.bin,
        &release_args(&reporter.env.pane_id, state_seq),
    );
}

fn worker_loop(env: HerdrEnv, slot: Arc<(Mutex<Slot>, Condvar)>, refresh_interval: Duration) {
    let (lock_slot, wake) = &*slot;
    let mut latest = None;
    let mut due = Instant::now();
    loop {
        let (report, state_seq, retry_seq, metadata_seq) = {
            let mut guard = lock(lock_slot);
            loop {
                if guard.released {
                    return;
                }
                if let Some(report) = guard.pending.take() {
                    latest = Some(report);
                    break;
                }
                if latest.is_some() && Instant::now() >= due {
                    break;
                }
                guard = if latest.is_some() {
                    wake.wait_timeout(guard, due.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                } else {
                    wake.wait(guard).unwrap_or_else(|e| e.into_inner())
                };
            }
            (latest.clone().unwrap(), next_seq(), next_seq(), next_seq())
        };
        let state_ok = send_report(&env, report.clone(), state_seq, retry_seq);
        let metadata_ok = send_metadata(&env, &report.metadata, metadata_seq);
        due = Instant::now()
            + if state_ok && metadata_ok {
                refresh_interval
            } else {
                RETRY_INTERVAL
            };
    }
}

fn send_report(env: &HerdrEnv, mut report: StateReport, seq: u64, retry_seq: u64) -> bool {
    if RESUME_UNSUPPORTED.load(Ordering::Relaxed) {
        report.resume_argv = None;
    }
    let status = run_herdr(&env.bin, &report_args(&env.pane_id, &report, seq));
    // Only a usage error on a report that carried a resume command means
    // "this herdr predates resume". Transient failures must not disable resume.
    if !(is_usage_error(status) && resume_command(&report).is_some()) {
        return status.is_some_and(|status| status.success());
    }
    report.resume_argv = None;
    let retry = run_herdr(&env.bin, &report_args(&env.pane_id, &report, retry_seq));
    if retry.is_some_and(|status| status.success()) {
        RESUME_UNSUPPORTED.store(true, Ordering::Relaxed);
        crate::logging::info(
            "herdr: this herdr rejects resume commands (needs 0.9.2+), reporting state only",
        );
    }
    retry.is_some_and(|status| status.success())
}

fn send_metadata(env: &HerdrEnv, metadata: &MetadataReport, seq: u64) -> bool {
    if METADATA_UNSUPPORTED.load(Ordering::Relaxed) {
        return true;
    }
    let status = run_herdr(&env.bin, &metadata_args(&env.pane_id, metadata, seq));
    if is_usage_error(status) {
        METADATA_UNSUPPORTED.store(true, Ordering::Relaxed);
        crate::logging::info("herdr: metadata unsupported, continuing lifecycle reports");
        return true;
    }
    status.is_some_and(|status| status.success())
}

fn metadata_args(pane_id: &str, metadata: &MetadataReport, seq: u64) -> Vec<String> {
    let mut args: Vec<String> = [
        "pane",
        "report-metadata",
        pane_id,
        "--source",
        METADATA_SOURCE,
        "--agent",
        AGENT,
        "--applies-to-source",
        SOURCE,
        "--ttl-ms",
        METADATA_TTL_MS,
        "--seq",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(seq.to_string());
    if let Some(summary) = &metadata.summary {
        args.extend(["--token".into(), format!("summary={summary}")]);
        for state in ["idle", "done", "working"] {
            args.extend(["--state-label".into(), format!("{state}={summary}")]);
        }
    } else {
        args.extend([
            "--clear-state-labels".into(),
            "--clear-token".into(),
            "summary".into(),
        ]);
    }
    if let Some(suffix) = &metadata.title_suffix {
        args.extend(["--token".into(), format!("title-suffix={suffix}")]);
    } else {
        args.extend(["--clear-token".into(), "title-suffix".into()]);
    }
    args
}

/// Exit code clap-based CLIs such as herdr use for argument/usage errors.
const USAGE_ERROR_EXIT_CODE: i32 = 2;

fn is_usage_error(status: Option<ExitStatus>) -> bool {
    status.and_then(|status| status.code()) == Some(USAGE_ERROR_EXIT_CODE)
}

/// The resume command [`report_args`] sends after `--`, if any. herdr only
/// accepts one alongside a session id, and only when it passes its validation.
fn resume_command(report: &StateReport) -> Option<&Vec<String>> {
    report.session_id.as_ref()?;
    report
        .resume_argv
        .as_ref()
        .filter(|argv| valid_resume_argv(argv))
}

/// Strictly increasing across reports, sessions, and exec reloads: wall-clock
/// microseconds, bumped past the previous value if the clock stalls.
fn next_seq() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let mut last = lock(&LAST_SEQ);
    *last = now.max(last.saturating_add(1));
    *last
}

fn report_args(pane_id: &str, report: &StateReport, seq: u64) -> Vec<String> {
    let mut args: Vec<String> = [
        "pane",
        "report-agent",
        pane_id,
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--state",
        report.state.as_str(),
        "--seq",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(seq.to_string());
    if let Some(session_id) = report.session_id.as_deref() {
        args.push("--agent-session-id".into());
        args.push(session_id.to_string());
        if let Some(argv) = resume_command(report) {
            args.push("--".into());
            args.extend(argv.iter().cloned());
        }
    }
    args
}

fn release_args(pane_id: &str, seq: u64) -> Vec<String> {
    vec![
        "pane".into(),
        "release-agent".into(),
        pane_id.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--seq".into(),
        seq.to_string(),
    ]
}

/// herdr rejects resume commands whose first word is a path, that contain an
/// apostrophe or control character, or that exceed 64 args / 8 KiB.
pub fn valid_resume_argv(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return false;
    }
    if argv.len() > MAX_RESUME_ARGS {
        return false;
    }
    let total: usize = argv.iter().map(|arg| arg.len()).sum::<usize>() + argv.len();
    if total > MAX_RESUME_BYTES {
        return false;
    }
    argv.iter()
        .all(|arg| !arg.contains('\'') && !arg.chars().any(char::is_control))
}

/// Command that reopens `session_id` from the pane's working directory,
/// keeping SSH attach targets and self-dev mode so the resumed client
/// behaves like this one. The model and provider live with the session.
fn resume_argv_for_session(
    session_id: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    let mut argv = vec!["jcode".to_string()];
    if let Some(host) = get("JCODE_SSH_REMOTE").filter(|h| !h.trim().is_empty()) {
        argv.extend(["--ssh".to_string(), host]);
        for (flag, variable) in [
            ("--ssh-binary", "JCODE_SSH_BINARY"),
            ("--ssh-server-socket", "JCODE_SSH_SERVER_SOCKET"),
            ("--remote-working-dir", "JCODE_SSH_WORKING_DIR"),
        ] {
            if let Some(value) = get(variable) {
                argv.extend([flag.to_string(), value]);
            }
        }
    }
    argv.extend(["--resume".to_string(), session_id.to_string()]);
    if get(jcode_selfdev_types::CLIENT_SELFDEV_ENV).is_some() {
        argv.push("self-dev".to_string());
    }
    valid_resume_argv(&argv).then_some(argv)
}

/// Run one herdr CLI call with a timeout. Returns its exit status, or `None`
/// when it could not be started or timed out.
fn run_herdr(bin: &str, args: &[String]) -> Option<ExitStatus> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("JCODE_HOOKS_DISABLED", "1")
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            crate::logging::warn(&format!("herdr: failed to run {bin}: {error}"));
            return None;
        }
    };
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    crate::logging::info(&format!(
                        "herdr: `{} {}` exited with {status}",
                        bin,
                        args.get(1).map(String::as_str).unwrap_or("")
                    ));
                }
                return Some(status);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                crate::logging::warn("herdr: report timed out");
                return None;
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Option<HerdrEnv> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        HerdrEnv::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn activates_only_inside_herdr_with_all_vars() {
        let full = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "p1"),
            ("HERDR_BIN_PATH", "/usr/bin/herdr"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ];
        assert_eq!(
            env(&full),
            Some(HerdrEnv {
                bin: "/usr/bin/herdr".into(),
                pane_id: "p1".into()
            })
        );
        assert_eq!(env(&[]), None);
        let mut not_one = full;
        not_one[0] = ("HERDR_ENV", "0");
        assert_eq!(env(&not_one), None);
        for skip in 1..full.len() {
            let partial: Vec<_> = full
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, kv)| *kv)
                .collect();
            assert_eq!(env(&partial), None, "missing {}", full[skip].0);
        }
    }

    #[test]
    fn report_args_include_resume_command_after_separator() {
        let report = StateReport {
            state: AgentState::Idle,
            session_id: Some("session_fox_123".into()),
            resume_argv: resume_argv_for_session("session_fox_123", |_| None),
            metadata: MetadataReport::default(),
        };
        assert_eq!(
            report_args("p1", &report, 42),
            [
                "pane",
                "report-agent",
                "p1",
                "--source",
                "jcode",
                "--agent",
                "jcode",
                "--state",
                "idle",
                "--seq",
                "42",
                "--agent-session-id",
                "session_fox_123",
                "--",
                "jcode",
                "--resume",
                "session_fox_123"
            ]
        );
    }

    #[test]
    fn report_args_without_session_omit_resume() {
        let report = StateReport {
            state: AgentState::Working,
            session_id: None,
            resume_argv: None,
            metadata: MetadataReport::default(),
        };
        let args = report_args("p1", &report, 7);
        assert_eq!(&args[7..], ["--state", "working", "--seq", "7"]);
    }

    #[test]
    fn release_args_match_herdr_cli() {
        assert_eq!(
            release_args("p1", 9),
            [
                "pane",
                "release-agent",
                "p1",
                "--source",
                "jcode",
                "--agent",
                "jcode",
                "--seq",
                "9"
            ]
        );
    }

    #[test]
    fn resume_argv_validation_follows_herdr_rules() {
        let ok =
            |v: &[&str]| valid_resume_argv(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&["jcode", "--resume", "session_a"]));
        assert!(!ok(&[]));
        assert!(!ok(&["/usr/bin/jcode"]));
        assert!(!ok(&["jcode", "it's"]));
        assert!(!ok(&["jcode", "a\nb"]));
        assert!(!ok(&vec!["x"; 65]));
        assert!(!ok(&["jcode", &"x".repeat(9000)]));
        assert_eq!(resume_argv_for_session("bad'id", |_| None), None);
    }

    #[test]
    fn resume_argv_keeps_ssh_target_and_selfdev() {
        let vars: std::collections::HashMap<&str, &str> = [
            ("JCODE_SSH_REMOTE", "devbox"),
            ("JCODE_SSH_WORKING_DIR", "/srv/repo"),
            (jcode_selfdev_types::CLIENT_SELFDEV_ENV, "1"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            resume_argv_for_session("session_a", |k| vars.get(k).map(|v| v.to_string())).unwrap(),
            [
                "jcode",
                "--ssh",
                "devbox",
                "--remote-working-dir",
                "/srv/repo",
                "--resume",
                "session_a",
                "self-dev"
            ]
        );
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let a = next_seq();
        let b = next_seq();
        let c = next_seq();
        assert!(a < b && b < c);
    }

    /// Fake herdr for `send_report` tests. Each call is logged; any call
    /// carrying the `--` resume separator exits with `resume_exit_code`.
    /// Holds the test lock and resets [`RESUME_UNSUPPORTED`] on drop, so a
    /// failing assertion cannot leak the global flag into other tests.
    #[cfg(unix)]
    struct FakeHerdr {
        dir: std::path::PathBuf,
        env: HerdrEnv,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl FakeHerdr {
        fn new(resume_exit_code: i32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            static SERIAL: Mutex<()> = Mutex::new(());
            let serial = lock(&SERIAL);
            RESUME_UNSUPPORTED.store(false, Ordering::Relaxed);
            METADATA_UNSUPPORTED.store(false, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "jcode-herdr-fake-{}-{}",
                std::process::id(),
                next_seq()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("herdr");
            // The script logs next to itself via `$0` rather than embedding
            // the temp path, so any `TMPDIR` (spaces, quotes) works.
            std::fs::write(
                &bin,
                format!(
                    "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/calls.log\"\nfor a in \"$@\"; do [ \"$a\" = -- ] && exit {resume_exit_code}; done\nexit 0\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let env = HerdrEnv {
                bin: bin.display().to_string(),
                pane_id: "p1".into(),
            };
            Self {
                dir,
                env,
                _serial: serial,
            }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeHerdr {
        fn drop(&mut self) {
            RESUME_UNSUPPORTED.store(false, Ordering::Relaxed);
            METADATA_UNSUPPORTED.store(false, Ordering::Relaxed);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn idle_report_with_resume() -> StateReport {
        StateReport {
            state: AgentState::Idle,
            session_id: Some("session_fox_1".into()),
            resume_argv: resume_argv_for_session("session_fox_1", |_| None),
            metadata: MetadataReport::default(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn report_falls_back_to_state_only_when_herdr_rejects_resume() {
        // herdr < 0.9.2 treats the `--` resume separator as a usage error.
        let herdr = FakeHerdr::new(USAGE_ERROR_EXIT_CODE);
        send_report(
            &herdr.env,
            idle_report_with_resume(),
            next_seq(),
            next_seq(),
        );
        assert!(RESUME_UNSUPPORTED.load(Ordering::Relaxed));
        send_report(
            &herdr.env,
            idle_report_with_resume(),
            next_seq(),
            next_seq(),
        );

        let calls = herdr.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].contains(" -- jcode --resume session_fox_1"));
        assert!(!calls[1].contains(" -- "));
        assert!(calls[1].contains("--agent-session-id session_fox_1"));
        // Remembered: later reports skip the doomed first attempt.
        assert!(!calls[2].contains(" -- "));
    }

    #[cfg(unix)]
    #[test]
    fn herdr_worker_retries_failed_idle_without_another_state_change() {
        let herdr = FakeHerdr::new(1);
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        lock(&slot.0).pending = Some(idle_report_with_resume());
        let worker_slot = Arc::clone(&slot);
        let env = herdr.env.clone();
        let worker = std::thread::spawn(move || worker_loop(env, worker_slot, REFRESH_INTERVAL));
        let deadline = Instant::now() + Duration::from_secs(3);
        let state_calls = || {
            herdr
                .calls()
                .iter()
                .filter(|call| call.contains(" report-agent "))
                .count()
        };
        while state_calls() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        lock(&slot.0).released = true;
        slot.1.notify_one();
        worker.join().unwrap();
        assert!(state_calls() >= 2, "failed idle must be retried");
    }

    #[cfg(unix)]
    #[test]
    fn herdr_metadata_sets_and_clears_owned_fields_as_literal_arguments() {
        let herdr = FakeHerdr::new(0);
        std::fs::write(
            &herdr.env.bin,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$(dirname \"$0\")/argv\"\nexit 0\n",
        )
        .unwrap();
        let metadata = MetadataReport {
            summary: Some("⏳ 2 subagents (scout, reviewer) · review '$HOME'; 東京".into()),
            title_suffix: Some("⏳2⚠".into()),
        };
        assert!(send_metadata(&herdr.env, &metadata, 42));
        let bytes = std::fs::read(herdr.dir.join("argv")).unwrap();
        let args: Vec<_> = bytes
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| std::str::from_utf8(arg).unwrap())
            .collect();
        assert!(args.windows(2).any(|p| p == ["--source", METADATA_SOURCE]));
        assert!(
            args.windows(2)
                .any(|p| p == ["--applies-to-source", SOURCE])
        );
        assert!(args.windows(2).any(|p| p == ["--ttl-ms", METADATA_TTL_MS]));
        assert!(args.windows(2).any(|p| p == ["--seq", "42"]));
        for state in ["idle", "done", "working"] {
            assert!(
                args.contains(&format!("{state}={}", metadata.summary.as_ref().unwrap()).as_str())
            );
        }
        assert!(args.contains(&"title-suffix=⏳2⚠"));
        assert!(!args.contains(&"--clear-token"));
        std::fs::remove_file(herdr.dir.join("argv")).unwrap();
        assert!(send_metadata(&herdr.env, &MetadataReport::default(), 43));
        let bytes = std::fs::read(herdr.dir.join("argv")).unwrap();
        let args: Vec<_> = bytes
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| std::str::from_utf8(arg).unwrap())
            .collect();
        assert!(args.contains(&"--clear-state-labels"));
        assert!(args.windows(2).any(|p| p == ["--clear-token", "summary"]));
        assert!(
            args.windows(2)
                .any(|p| p == ["--clear-token", "title-suffix"])
        );
        assert!(!args.contains(&"--token"));
    }

    #[cfg(unix)]
    #[test]
    fn herdr_old_metadata_cli_does_not_disable_lifecycle_reporting() {
        let herdr = FakeHerdr::new(0);
        std::fs::write(&herdr.env.bin,
            "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/calls.log\"\n[ \"$2\" = report-metadata ] && exit 2\nexit 0\n"
        ).unwrap();
        assert!(send_metadata(
            &herdr.env,
            &MetadataReport::default(),
            next_seq()
        ));
        assert!(METADATA_UNSUPPORTED.load(Ordering::Relaxed));
        assert!(send_metadata(
            &herdr.env,
            &MetadataReport::default(),
            next_seq()
        ));
        assert!(send_report(
            &herdr.env,
            idle_report_with_resume(),
            next_seq(),
            next_seq()
        ));
        assert_eq!(herdr.calls().len(), 2);
        assert!(herdr.calls()[1].contains("--state idle"));
    }

    #[cfg(unix)]
    #[test]
    fn herdr_worker_refreshes_live_metadata_and_keeps_newest_snapshot() {
        let herdr = FakeHerdr::new(0);
        let mut report = idle_report_with_resume();
        report.state = AgentState::Working;
        report.metadata = MetadataReport {
            summary: Some("one child".into()),
            title_suffix: Some("⏳1".into()),
        };
        let slot = Arc::new((
            Mutex::new(Slot {
                pending: Some(report),
                released: false,
            }),
            Condvar::new(),
        ));
        let env = herdr.env.clone();
        let worker_slot = Arc::clone(&slot);
        let worker =
            std::thread::spawn(move || worker_loop(env, worker_slot, Duration::from_millis(20)));
        let deadline = Instant::now() + Duration::from_secs(3);
        while herdr
            .calls()
            .iter()
            .filter(|call| call.contains("summary=one child"))
            .count()
            < 2
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        // A final idle snapshot supersedes both the old active report and its heartbeat.
        lock(&slot.0).pending = Some(idle_report_with_resume());
        slot.1.notify_one();
        while !herdr
            .calls()
            .iter()
            .any(|call| call.contains("--state idle"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        lock(&slot.0).released = true;
        slot.1.notify_one();
        worker.join().unwrap();
        let calls = herdr.calls();
        assert!(
            calls
                .iter()
                .filter(|call| call.contains("summary=one child"))
                .count()
                >= 2
        );
        let idle = calls
            .iter()
            .position(|call| call.contains("--state idle"))
            .expect("idle sent");
        assert!(
            calls[idle + 1..].iter().all(
                |call| !call.contains("summary=one child") && !call.contains("--state working")
            )
        );
        assert!(calls.last().unwrap().contains("--clear-token summary"));
    }

    #[cfg(unix)]
    #[test]
    fn herdr_release_clears_metadata_and_wakes_worker_without_resurrection() {
        let herdr = FakeHerdr::new(0);
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        let reporter = Reporter {
            env: herdr.env.clone(),
            slot: Arc::clone(&slot),
            last_submitted: Mutex::new(None),
        };
        let env = herdr.env.clone();
        let worker_slot = Arc::clone(&slot);
        let worker = std::thread::spawn(move || worker_loop(env, worker_slot, REFRESH_INTERVAL));
        release_reporter(&reporter);
        worker.join().unwrap();
        release_reporter(&reporter);
        let calls = herdr.calls();
        assert_eq!(calls.len(), 2, "release is idempotent: {calls:?}");
        assert!(calls[0].contains("report-metadata") && calls[0].contains("--clear-state-labels"));
        assert!(
            calls[0].contains("--clear-token summary")
                && calls[0].contains("--clear-token title-suffix")
        );
        assert!(calls[1].contains("release-agent"));
    }

    #[cfg(unix)]
    #[test]
    fn report_keeps_resume_after_non_usage_failure() {
        // A transient failure (not a usage error) must not disable resume.
        let herdr = FakeHerdr::new(1);
        send_report(
            &herdr.env,
            idle_report_with_resume(),
            next_seq(),
            next_seq(),
        );
        assert!(!RESUME_UNSUPPORTED.load(Ordering::Relaxed));

        let calls = herdr.calls();
        assert_eq!(calls.len(), 1, "no state-only retry expected: {calls:?}");
        assert!(calls[0].contains(" -- jcode --resume session_fox_1"));
    }
}
