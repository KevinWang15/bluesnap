//! Process run modes, capability matrix, and window-lifecycle policy.
//!
//! `bluesnap` runs in one of two distinct modes depending on how it was invoked:
//!
//! - [`RunMode::Daemon`]: the resident background screenshot utility (default
//!   when launched with no arguments, flags, image files, or multiple files).
//!   Registers the global capture shortcut, creates the tray / menu bar icon,
//!   refreshes the login item and `.bs` file association, and stays alive in
//!   the background until explicitly quit from the tray menu.
//! - [`RunMode::EditBs`]: a standalone Composition Editor session (when launched
//!   with a single `.bs` argument, e.g. double-clicking a `.bs` file in Windows
//!   Explorer or macOS Finder). Skips the tray icon, global capture shortcut,
//!   login-item update, and startup corner toast so it coexists cleanly with an
//!   already-running `bluesnap` daemon, while keeping full in-editor screenshot,
//!   scrolling-capture, OCR, and AI auto-redaction support — and exits
//!   automatically once all user windows and pending clipboard/save tasks finish.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bluesnap_platform::AppRole;
use gpui::App;

use crate::{AppState, demo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// Resident background tray / menu bar screenshot utility.
    Daemon { initial_files: Vec<PathBuf> },
    /// Standalone Composition Editor opened for a single `.bs` file.
    EditBs { file: PathBuf },
}

impl RunMode {
    /// Determines the run mode from `std::env::args_os()` (excluding `argv[0]`).
    pub fn from_env() -> Self {
        Self::from_args(std::env::args_os().skip(1))
    }

    /// Parses post-`argv[0]` command-line arguments into a [`RunMode`].
    pub fn from_args(args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        // Ignore macOS LaunchServices `-psn_0_…` process serial number flags.
        let filtered: Vec<OsString> = args
            .into_iter()
            .map(Into::into)
            .filter(|a| !a.to_string_lossy().starts_with("-psn_"))
            .collect();

        if let [only] = filtered.as_slice() {
            let path = PathBuf::from(only);
            let looks_like_flag = only.to_string_lossy().starts_with('-') && !path.exists();
            if !looks_like_flag && has_bs_extension(&path) {
                return Self::EditBs { file: path };
            }
        }

        let initial_files = filtered
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.exists() || !p.as_os_str().to_string_lossy().starts_with('-'))
            .collect();
        Self::Daemon { initial_files }
    }

    /// OS application role passed to [`bluesnap_platform::Platform::init`].
    pub fn app_role(&self) -> AppRole {
        match self {
            Self::Daemon { .. } => AppRole::BackgroundUtility,
            Self::EditBs { .. } => AppRole::StandaloneEditor,
        }
    }

    /// Global capture hotkey to register at startup, or `None` when skipped.
    ///
    /// Standalone `.bs` editor sessions skip the global capture hotkey so they
    /// never collide with a resident `bluesnap` daemon; session-scoped hotkeys
    /// (such as `Escape` during an in-editor scrolling capture) remain available.
    pub fn capture_hotkey<'a>(&self, configured: &'a str) -> Option<&'a str> {
        match self {
            Self::Daemon { .. } => Some(configured),
            Self::EditBs { .. } => None,
        }
    }

    /// Whether to create the system tray / menu bar icon.
    pub fn uses_tray(&self) -> bool {
        matches!(self, Self::Daemon { .. })
    }

    /// Whether to refresh the start-at-login entry to point at this executable.
    pub fn refreshes_login_item(&self) -> bool {
        matches!(self, Self::Daemon { .. })
    }

    /// Whether `.bs` auto-registration should update an already-registered
    /// executable path (`true` in [`Self::Daemon`], `false` in [`Self::EditBs`]
    /// so opening a file with a temporary binary only registers `.bs` if unset).
    pub fn overwrites_file_association(&self) -> bool {
        matches!(self, Self::Daemon { .. })
    }

    /// Whether to show the 3-second launch corner notification.
    pub fn shows_startup_toast(&self) -> bool {
        match self {
            Self::Daemon { initial_files } => initial_files.is_empty() && !demo::enabled(),
            Self::EditBs { .. } => false,
        }
    }

    /// Whether `BLUESNAP_DEMO_CAPTURE_AFTER_MS` applies in this mode.
    pub fn runs_demo_capture(&self) -> bool {
        matches!(self, Self::Daemon { .. })
    }

    /// Whether the process should quit automatically once all user windows close
    /// and all pending clipboard / save tasks complete.
    pub fn exits_when_idle(&self) -> bool {
        matches!(self, Self::EditBs { .. })
    }

    /// Files to open into the Composition Editor right after startup.
    pub fn initial_files(&self) -> &[PathBuf] {
        match self {
            Self::Daemon { initial_files } => initial_files,
            Self::EditBs { file } => std::slice::from_ref(file),
        }
    }
}

fn has_bs_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("bs"))
}

/// Hooks window-close events so modes with [`RunMode::exits_when_idle`] terminate
/// cleanly after their last visible window and background I/O task finish.
pub fn install_lifecycle(cx: &mut App) {
    cx.on_window_closed(|cx, _| {
        cx.defer(exit_if_idle);
    })
    .detach();
}

/// Marks a background delivery or modal dialog task as active so standalone
/// sessions never exit mid-clipboard-copy or mid-file-save.
pub fn begin_job(cx: &mut App) {
    if let Some(state) = cx.try_global::<AppState>() {
        let next = state.pending_jobs + 1;
        cx.global_mut::<AppState>().pending_jobs = next;
    }
}

/// Completes a background job started with [`begin_job`] and re-checks idle exit.
pub fn end_job(cx: &mut App) {
    if cx.try_global::<AppState>().is_some() {
        let state = cx.global_mut::<AppState>();
        state.pending_jobs = state.pending_jobs.saturating_sub(1);
        exit_if_idle(cx);
    }
}

/// Quits the application if the active [`RunMode`] exits on idle and no user
/// windows, hidden editor sessions, capture overlays, or background jobs remain.
pub fn exit_if_idle(cx: &mut App) {
    let Some(state) = cx.try_global::<AppState>() else { return };
    if !state.mode.exits_when_idle() || state.pending_jobs > 0 {
        return;
    }
    if state.editor.is_some()
        || state.editor_hidden
        || state.overlay.is_some()
        || state.scrolling.is_some()
        || state.ai_window.is_some()
        || state.settings_window.is_some()
    {
        return;
    }
    let keepalive = state.keepalive;
    let has_user_window = cx.windows().into_iter().any(|w| Some(w) != keepalive);
    if !has_user_window {
        log::info!("standalone editor session idle; quitting");
        cx.quit();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bluesnap_platform::AppRole;

    use super::RunMode;

    #[test]
    fn single_bs_argument_selects_standalone_editor_mode() {
        let mode = RunMode::from_args([r"C:\Users\ke\Desktop\diagram.bs"]);
        assert_eq!(mode, RunMode::EditBs { file: PathBuf::from(r"C:\Users\ke\Desktop\diagram.bs") });
        assert_eq!(mode.app_role(), AppRole::StandaloneEditor);
        assert_eq!(mode.capture_hotkey("Ctrl+Alt+A"), None);
        assert!(!mode.uses_tray());
        assert!(!mode.refreshes_login_item());
        assert!(!mode.overwrites_file_association());
        assert!(!mode.shows_startup_toast());
        assert!(!mode.runs_demo_capture());
        assert!(mode.exits_when_idle());
        assert_eq!(mode.initial_files(), &[PathBuf::from(r"C:\Users\ke\Desktop\diagram.bs")]);
    }

    #[test]
    fn uppercase_bs_and_macos_psn_still_select_standalone_editor_mode() {
        assert_eq!(
            RunMode::from_args(["REPORT.BS"]),
            RunMode::EditBs { file: PathBuf::from("REPORT.BS") }
        );
        assert_eq!(
            RunMode::from_args(["-psn_0_102425", "/Users/ke/Documents/shot.Bs"]),
            RunMode::EditBs { file: PathBuf::from("/Users/ke/Documents/shot.Bs") }
        );
    }

    #[test]
    fn zero_non_bs_multiple_or_flag_args_select_daemon_mode() {
        let empty = RunMode::from_args(std::iter::empty::<&str>());
        assert_eq!(empty, RunMode::Daemon { initial_files: Vec::new() });
        assert_eq!(empty.app_role(), AppRole::BackgroundUtility);
        assert_eq!(empty.capture_hotkey("Ctrl+Alt+A"), Some("Ctrl+Alt+A"));
        assert!(empty.uses_tray());
        assert!(empty.refreshes_login_item());
        assert!(empty.overwrites_file_association());
        assert!(empty.runs_demo_capture());
        assert!(!empty.exits_when_idle());

        assert_eq!(
            RunMode::from_args(["photo.png"]),
            RunMode::Daemon { initial_files: vec![PathBuf::from("photo.png")] }
        );
        assert_eq!(
            RunMode::from_args(["first.bs", "second.bs"]),
            RunMode::Daemon {
                initial_files: vec![PathBuf::from("first.bs"), PathBuf::from("second.bs")],
            }
        );
        assert_eq!(
            RunMode::from_args(["--unknown-flag.bs"]),
            RunMode::Daemon { initial_files: Vec::new() }
        );
    }
}
