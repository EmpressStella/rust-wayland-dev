//! Prevent stale idle actions from running immediately after resume.
//!
//! NVIDIA keeps user sessions runnable around sleep so it can preserve GPU
//! state. `swayidle` timers therefore continue to age while a laptop sleeps.
//! This helper records the cumulative suspend time when its idle service
//! starts, then suppresses the stale dim, lock, display-off, and suspend batch
//! that can otherwise run immediately after wake.

#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::Duration,
};

use rustix::{
    fs::{FlockOperation, flock},
    time::{ClockId, Timespec, clock_gettime},
};
use uzers::get_current_uid;

const DIRECTORY_NAME: &str = "genoa";
const STATE_FILE_NAME: &str = "idle-suspend-baseline";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const MAX_STATE_BYTES: usize = 64;
const SUSPEND_DETECTION_TOLERANCE: Duration = Duration::from_secs(1);
const RESUME_GUARD_DURATION: Duration = Duration::from_secs(60);
const STALE_ACTION_EXIT_CODE: u8 = 75;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardState {
    cumulative_suspend: Duration,
    suppress_until: Duration,
}

fn main() -> ExitCode {
    let result = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => suspend_if_timer_is_current().map(|()| ExitCode::SUCCESS),
        [flag] if flag == "--check" => stale_idle_action_is_current().map(|current| {
            if current {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(STALE_ACTION_EXIT_CODE)
            }
        }),
        [flag] if flag == "--record-baseline" => record_baseline().map(|()| ExitCode::SUCCESS),
        [flag] if flag == "--help" || flag == "-h" => {
            println!("Usage: genoa-idle-suspend [--record-baseline | --check]");
            Ok(ExitCode::SUCCESS)
        }
        _ => Err("Usage: genoa-idle-suspend [--record-baseline | --check]".to_string()),
    };

    match result {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("genoa-idle-suspend: {error}");
            ExitCode::FAILURE
        }
    }
}

fn record_baseline() -> Result<(), String> {
    let mut state = open_state_file()?;
    write_state(
        &mut state,
        GuardState {
            cumulative_suspend: cumulative_suspend_time()?,
            suppress_until: Duration::ZERO,
        },
    )
}

fn suspend_if_timer_is_current() -> Result<(), String> {
    if !stale_idle_action_is_current()? {
        return Ok(());
    }
    let status = Command::new(SYSTEMCTL)
        .arg("suspend")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("could not start system suspend: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl suspend exited with {status}"))
    }
}

fn stale_idle_action_is_current() -> Result<bool, String> {
    let cumulative_suspend = cumulative_suspend_time()?;
    let monotonic = monotonic_time()?;
    let mut state_file = open_state_file()?;
    let state = match read_state(&mut state_file) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("genoa-idle-suspend: invalid idle baseline; resetting it: {error}");
            None
        }
    };
    let (next_state, should_run) = decide_idle_action(state, cumulative_suspend, monotonic);
    write_state(&mut state_file, next_state)?;
    if !should_run {
        eprintln!("genoa-idle-suspend: discarded an idle action that aged through suspend");
    }
    Ok(should_run)
}

fn decide_idle_action(
    state: Option<GuardState>,
    cumulative_suspend: Duration,
    monotonic: Duration,
) -> (GuardState, bool) {
    let Some(mut state) = state else {
        return (
            GuardState {
                cumulative_suspend,
                suppress_until: suppress_until(monotonic),
            },
            false,
        );
    };
    if timer_aged_through_suspend(state.cumulative_suspend, cumulative_suspend) {
        state.cumulative_suspend = cumulative_suspend;
        state.suppress_until = suppress_until(monotonic);
        return (state, false);
    }
    if monotonic < state.suppress_until {
        return (state, false);
    }
    state.cumulative_suspend = cumulative_suspend;
    (state, true)
}

const fn suppress_until(monotonic: Duration) -> Duration {
    monotonic.saturating_add(RESUME_GUARD_DURATION)
}

fn timer_aged_through_suspend(baseline: Duration, now: Duration) -> bool {
    now.saturating_sub(baseline) >= SUSPEND_DETECTION_TOLERANCE
}

fn cumulative_suspend_time() -> Result<Duration, String> {
    let monotonic = duration_from_timespec(clock_gettime(ClockId::Monotonic))?;
    let boottime = duration_from_timespec(clock_gettime(ClockId::Boottime))?;
    Ok(boottime.saturating_sub(monotonic))
}

fn monotonic_time() -> Result<Duration, String> {
    duration_from_timespec(clock_gettime(ClockId::Monotonic))
}

fn duration_from_timespec(timespec: Timespec) -> Result<Duration, String> {
    let seconds = u64::try_from(timespec.tv_sec)
        .map_err(|_| "clock returned a negative seconds value".to_string())?;
    let nanoseconds = u32::try_from(timespec.tv_nsec)
        .map_err(|_| "clock returned an invalid nanoseconds value".to_string())?;
    Ok(Duration::new(seconds, nanoseconds))
}

fn open_state_file() -> Result<File, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "XDG_RUNTIME_DIR is unavailable".to_string())?;
    open_state_file_in(&runtime)
}

fn open_state_file_in(runtime: &Path) -> Result<File, String> {
    ensure_private_directory(runtime)?;
    let directory = runtime.join(DIRECTORY_NAME);
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("could not create {}: {error}", directory.display())),
    }
    ensure_private_directory(&directory)?;

    let path = directory.join(STATE_FILE_NAME);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    ensure_private_regular_file(&file, &path)?;
    flock(&file, FlockOperation::NonBlockingLockExclusive)
        .map_err(|error| format!("another idle-suspend helper is running: {error}"))?;
    Ok(file)
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{} is not a real directory", path.display()));
    }
    if metadata.uid() != get_current_uid() {
        return Err(format!(
            "{} is not owned by the active user",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} is accessible by another user", path.display()));
    }
    Ok(())
}

fn ensure_private_regular_file(file: &File, path: &Path) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.uid() != get_current_uid() {
        return Err(format!("{} is not a private regular file", path.display()));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} is accessible by another user", path.display()));
    }
    Ok(())
}

fn read_state(file: &mut File) -> Result<Option<GuardState>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not read idle baseline: {error}"))?;
    let mut bytes = [0_u8; MAX_STATE_BYTES + 1];
    let length = file
        .read(&mut bytes)
        .map_err(|error| format!("could not read idle baseline: {error}"))?;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_STATE_BYTES {
        return Err("idle baseline is too large".to_string());
    }
    let text = std::str::from_utf8(&bytes[..length])
        .map_err(|_| "idle baseline is not valid UTF-8".to_string())?;
    let values = text.split_ascii_whitespace().collect::<Vec<_>>();
    match values.as_slice() {
        [legacy_baseline] => Ok(Some(GuardState {
            cumulative_suspend: parse_duration(legacy_baseline)?,
            suppress_until: Duration::ZERO,
        })),
        ["v1", baseline, suppress_until] => Ok(Some(GuardState {
            cumulative_suspend: parse_duration(baseline)?,
            suppress_until: parse_duration(suppress_until)?,
        })),
        _ => Err("idle baseline has an unknown format".to_string()),
    }
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let nanoseconds = value
        .parse::<u64>()
        .map_err(|_| "idle baseline is not a duration".to_string())?;
    Ok(Duration::from_nanos(nanoseconds))
}

fn write_state(file: &mut File, state: GuardState) -> Result<(), String> {
    let suspend_nanoseconds = u64::try_from(state.cumulative_suspend.as_nanos())
        .map_err(|_| "idle baseline exceeds the supported range".to_string())?;
    let suppress_nanoseconds = u64::try_from(state.suppress_until.as_nanos())
        .map_err(|_| "idle suppression window exceeds the supported range".to_string())?;
    file.set_len(0)
        .map_err(|error| format!("could not reset idle baseline: {error}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not write idle baseline: {error}"))?;
    file.write_all(format!("v1 {suspend_nanoseconds} {suppress_nanoseconds}\n").as_bytes())
        .map_err(|error| format!("could not write idle baseline: {error}"))?;
    file.sync_data()
        .map_err(|error| format!("could not save idle baseline: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    use super::{
        GuardState, RESUME_GUARD_DURATION, decide_idle_action, open_state_file_in, read_state,
        timer_aged_through_suspend, write_state,
    };

    #[test]
    fn detects_idle_timers_that_crossed_a_real_suspend() {
        assert!(timer_aged_through_suspend(
            Duration::from_secs(5),
            Duration::from_secs(7)
        ));
    }

    #[test]
    fn ignores_clock_read_jitter_while_the_system_is_awake() {
        assert!(!timer_aged_through_suspend(
            Duration::from_secs(5),
            Duration::from_millis(5_500)
        ));
    }

    #[test]
    fn state_is_private_and_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
        let mut state = open_state_file_in(temporary.path())?;
        let expected = GuardState {
            cumulative_suspend: Duration::from_secs(42),
            suppress_until: Duration::from_secs(99),
        };
        write_state(&mut state, expected)?;
        assert_eq!(read_state(&mut state)?, Some(expected));
        assert_eq!(state.metadata()?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn suppresses_the_entire_stale_action_batch_after_resume() {
        let state = GuardState {
            cumulative_suspend: Duration::from_secs(10),
            suppress_until: Duration::ZERO,
        };
        let (state, run_first_action) = decide_idle_action(
            Some(state),
            Duration::from_secs(12),
            Duration::from_secs(1_000),
        );
        assert!(!run_first_action);
        assert_eq!(
            state.suppress_until,
            Duration::from_secs(1_000) + RESUME_GUARD_DURATION
        );
        let (_, run_later_stale_action) = decide_idle_action(
            Some(state),
            Duration::from_secs(12),
            Duration::from_secs(1_005),
        );
        assert!(!run_later_stale_action);
    }

    #[test]
    fn allows_a_new_idle_sequence_after_the_guard_expires() {
        let state = GuardState {
            cumulative_suspend: Duration::from_secs(12),
            suppress_until: Duration::from_secs(1_060),
        };
        let (_, should_run) = decide_idle_action(
            Some(state),
            Duration::from_secs(12),
            Duration::from_secs(1_061),
        );
        assert!(should_run);
    }
}
