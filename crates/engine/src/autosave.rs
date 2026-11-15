//! Auto-save ring + crash-recovery journal, and the preferences that drive them.
//!
//! **Auto Save** (Premiere behaviour, Preferences ▸ Auto Save): every *interval* minutes, if the
//! project changed since the last auto-save, a copy is written to the `Auto-Save` folder next to the
//! project as `<name>-YYYY-MM-DD_HH-MM-SS.fcproj`, keeping at most *Maximum Project Versions* files.
//! Optionally the project file itself is saved too.
//!
//! **Recovery journal** (FilmCraft addition): while the project has unsaved changes, a snapshot is
//! written to `<data dir>/Recovery/<session>/snapshot.fcproj` shortly after each change (debounced,
//! and at least every *N* seconds while edits keep coming). Each running app holds an OS lock on
//! `<session>/lock`; when the process dies (crash, `kill -9`, power cut) the lock is released, so on
//! the next launch any session directory whose lock can be taken and that holds a snapshot is a
//! recovery candidate ("Recover unsaved changes from <time>?").
//!
//! **Threading.** The UI thread only hands the worker an `Arc<Project>` (the immutable snapshot the
//! engine already keeps for undo) — a pointer copy and a channel send. Serialization and all file
//! I/O happen on the worker thread, so neither ever stalls the UI.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use filmcraft_format::autosave as names;
use filmcraft_project::Project;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use crate::settings;

// ------------------------------------------------------------------ preferences

/// Preferences ▸ Auto Save.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AutoSavePrefs {
    /// "Automatically save projects".
    pub enabled: bool,
    /// "Automatically Save Every: N minute(s)".
    pub interval_minutes: u32,
    /// "Maximum Project Versions".
    pub max_versions: u32,
    /// "Auto Save also saves the current project(s)".
    pub save_current_project: bool,
    /// Keep a crash-recovery journal of unsaved changes.
    pub recovery_journal: bool,
    /// Longest time (s) unsaved changes may stay out of the journal while edits keep coming.
    pub recovery_interval_seconds: u32,
    /// Folders chosen in Project Settings ▸ Scratch Disks on this computer. A project file may
    /// name these, or folders inside its own folder; see
    /// [`crate::project_tools::trusted_scratch`].
    pub scratch_folders: Vec<String>,
}

impl Default for AutoSavePrefs {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 5,
            max_versions: 20,
            save_current_project: false,
            recovery_journal: true,
            recovery_interval_seconds: 5,
            scratch_folders: Vec::new(),
        }
    }
}

impl AutoSavePrefs {
    fn clamp(&mut self) {
        self.interval_minutes = self.interval_minutes.clamp(1, 1440);
        self.max_versions = self.max_versions.clamp(1, 1000);
        self.recovery_interval_seconds = self.recovery_interval_seconds.clamp(1, 600);
    }
}

/// Settings ▸ Playback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaybackPrefs {
    /// "Preroll: N seconds" (Play Around, trim-mode loop playback).
    pub preroll_seconds: f64,
    /// "Postroll: N seconds".
    pub postroll_seconds: f64,
    /// "Step forward/back many: N frames" (Shift+Right / Shift+Left).
    pub step_many_frames: u32,
    pub pause_encoder_queue: bool,
    pub enable_transmit: bool,
    pub disable_video_in_background: bool,
    /// Draft decoding while playing at 1/2 resolution or lower: H.264 non-reference pictures
    /// skip deblocking (faster; those frames are approximate). Never used for exports, renders or
    /// a paused frame. Off by default.
    pub draft_decode: bool,
    /// "Hardware decoding": `auto` (default: the system's hardware decoder for streams it
    /// supports, e.g. VideoToolbox H.264 / HEVC on macOS, falling back to FilmCraft's own decoder)
    /// or `off` (always FilmCraft's own decoders). Media already open keeps its decoder until it
    /// is reopened.
    pub hardware_decoding: String,
}

impl Default for PlaybackPrefs {
    fn default() -> Self {
        Self {
            preroll_seconds: 3.0,
            postroll_seconds: 2.0,
            step_many_frames: 5,
            pause_encoder_queue: true,
            enable_transmit: false,
            disable_video_in_background: true,
            draft_decode: false,
            hardware_decoding: "auto".into(),
        }
    }
}

/// Settings ▸ Trim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrimPrefs {
    /// "Large Trim Offset: N frames" (Trim Forward/Backward Many, the Trim Monitor's ±N buttons).
    pub large_trim_offset: u32,
    /// The audio Large Trim Offset (audio time units).
    pub large_trim_offset_audio: u32,
    /// "Allow Selection tool to choose Roll and Ripple trims without modifier key".
    pub selection_tool_roll_ripple: bool,
    pub tool_changes_trim_type: bool,
    pub shift_overlapping_clips: bool,
    pub ripple_adds_edits: bool,
    /// "Playhead position determines trim monitor loop playback" (else the edit point does).
    pub playhead_determines_loop: bool,
    pub dynamic_ripple_updates: bool,
}

impl Default for TrimPrefs {
    fn default() -> Self {
        Self {
            large_trim_offset: 5,
            large_trim_offset_audio: 100,
            selection_tool_roll_ripple: false,
            tool_changes_trim_type: true,
            shift_overlapping_clips: false,
            ripple_adds_edits: false,
            playhead_determines_loop: false,
            dynamic_ripple_updates: true,
        }
    }
}

/// User preferences (persisted as JSON in the per-user data directory). Keys are dotted camelCase
/// paths: `autoSave.enabled`, `autoSave.intervalMinutes`, `playback.prerollSeconds`, …
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Preferences {
    /// File layout version ([`crate::settings::PREFS_VERSION`]); older files are migrated on load.
    pub version: u32,
    pub general: settings::GeneralPrefs,
    pub appearance: settings::AppearancePrefs,
    pub audio: AudioPrefs,
    pub audio_hardware: settings::AudioHardwarePrefs,
    pub auto_save: AutoSavePrefs,
    pub color: settings::ColorPrefs,
    pub graphics: settings::GraphicsPrefs,
    pub labels: settings::LabelPrefs,
    pub media: MediaPrefs,
    pub media_analysis: settings::MediaAnalysisPrefs,
    pub media_cache: settings::MediaCachePrefs,
    pub memory: settings::MemoryPrefs,
    pub playback: PlaybackPrefs,
    pub plugins: settings::PluginPrefs,
    pub timeline: settings::TimelinePrefs,
    pub trim: TrimPrefs,
    pub essential_sound: EssentialSoundPrefs,
    pub guides: GuidePrefs,
    /// Voice-Over Record Settings (timeline track header ▸ right-click the microphone).
    pub voice_over: crate::voiceover::VoiceOverPrefs,
    /// Project panel: view, List view columns and sort, view presets, Freeform options.
    pub project_panel: crate::project_panel::ProjectPanelPrefs,
    /// Media Browser: Favorites, recent directories, file types, view, columns.
    pub media_browser: crate::media_browser::MediaBrowserPrefs,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: settings::PREFS_VERSION,
            general: Default::default(),
            appearance: Default::default(),
            audio: Default::default(),
            audio_hardware: Default::default(),
            auto_save: Default::default(),
            color: Default::default(),
            graphics: Default::default(),
            labels: Default::default(),
            media: Default::default(),
            media_analysis: Default::default(),
            media_cache: Default::default(),
            memory: Default::default(),
            playback: Default::default(),
            plugins: Default::default(),
            timeline: Default::default(),
            trim: Default::default(),
            essential_sound: Default::default(),
            guides: Default::default(),
            voice_over: Default::default(),
            project_panel: Default::default(),
            media_browser: Default::default(),
        }
    }
}

/// View ▸ Guide Templates: guide sets saved from the Program Monitor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GuidePrefs {
    pub templates: Vec<GuideTemplate>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GuideTemplate {
    pub name: String,
    pub guides: Vec<Guide>,
}

/// A monitor guide: a vertical line at x = `position` or a horizontal line at y = `position`
/// (frame pixels).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Guide {
    pub vertical: bool,
    pub position: f64,
}

/// Settings ▸ Media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaPrefs {
    /// "Indeterminate Media Timebase": the frame rate of stills (`29.97df`, `25`, …).
    pub indeterminate_timebase: String,
    /// "Timecode": `useMediaSource` | `generate`.
    pub timecode: String,
    /// "Frame Count": `startAt0` | `startAt1` | `timecodeConversion`.
    pub frame_count: String,
    /// "Default Media Scaling": `none` | `scaleToFrameSize` | `setToFrameSize`.
    pub default_media_scaling: String,
    pub write_xmp_id: bool,
    pub validate_content_credentials: bool,
    pub write_clip_markers_to_xmp: bool,
    pub enable_xmp_linking: bool,
    pub include_captions_on_import: bool,
    /// "Enable proxies": playback and monitors read attached proxies (the monitors' Toggle
    /// Proxies button). Export always uses full-resolution media.
    pub enable_proxies: bool,
    pub allow_duplicate_media: bool,
    pub create_folder_for_imported_projects: bool,
    pub auto_hide_dependent_clips: bool,
    pub import_image_sequences: bool,
    pub refresh_growing_files: bool,
    pub resume_growing_playback: bool,
    pub growing_refresh_seconds: u32,
    pub hardware_decoding: bool,
    pub prores_hardware_encoding: bool,
}

impl Default for MediaPrefs {
    fn default() -> Self {
        Self {
            indeterminate_timebase: "29.97df".into(),
            timecode: "useMediaSource".into(),
            frame_count: "startAt0".into(),
            default_media_scaling: "none".into(),
            write_xmp_id: false,
            validate_content_credentials: true,
            write_clip_markers_to_xmp: false,
            enable_xmp_linking: false,
            include_captions_on_import: true,
            enable_proxies: false,
            allow_duplicate_media: false,
            create_folder_for_imported_projects: false,
            auto_hide_dependent_clips: false,
            import_image_sequences: false,
            refresh_growing_files: true,
            resume_growing_playback: false,
            growing_refresh_seconds: 60,
            hardware_decoding: true,
            prores_hardware_encoding: true,
        }
    }
}

/// Essential Sound: user presets saved from the panel (built-in presets live in
/// `filmcraft_project::essential`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EssentialSoundPrefs {
    pub user_presets: Vec<UserSoundPreset>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserSoundPreset {
    pub name: String,
    pub settings: filmcraft_project::EssentialSound,
}

/// Preferences ▸ Audio (the mixer-automation part) and the Track Mixer panel-menu toggle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioPrefs {
    /// "Automatch Time" (s): how long Touch takes to return to the existing automation.
    pub automatch_time: f64,
    /// "Large Volume Adjustment" (dB) for the Increase/Decrease Clip Volume Many commands.
    pub large_volume_adjustment: f64,
    /// Automation Keyframe Optimization: "Linear keyframe thinning".
    pub linear_keyframe_thinning: bool,
    /// "Minimum time interval thinning".
    pub minimum_time_interval_thinning: bool,
    /// "Minimum time" (ms).
    pub minimum_time_ms: u32,
    /// Track Mixer ▸ "Switch to Touch after Write".
    pub switch_to_touch_after_write: bool,
    /// "5.1 Mixdown Type": `front` | `frontRear` | `frontLfe` | `frontRearLfe`.
    pub mixdown_type: String,
    pub sum_to_mono_in_source: bool,
    pub scrub_audio: bool,
    pub maintain_pitch_shuttling: bool,
    pub mute_input_during_recording: bool,
    pub generate_waveforms_on_import: bool,
    pub multithreaded_waveforms: bool,
    pub render_audio_with_video: bool,
    pub auto_tag_audio_types: bool,
    pub always_override_audio_tags: bool,
    /// "Render Edit in Audition files to": `scratch` | `nextToMedia`.
    pub edit_in_audition_location: String,
    /// Essential Sound Loudness Auto-Match targets (integrated LUFS) per audio type.
    pub dialogue_target_lufs: f64,
    pub music_target_lufs: f64,
    pub sfx_target_lufs: f64,
    pub ambience_target_lufs: f64,
}

impl Default for AudioPrefs {
    fn default() -> Self {
        Self {
            automatch_time: 1.0,
            large_volume_adjustment: 6.0,
            linear_keyframe_thinning: true,
            minimum_time_interval_thinning: false,
            minimum_time_ms: 20,
            switch_to_touch_after_write: true,
            mixdown_type: "frontRear".into(),
            sum_to_mono_in_source: true,
            scrub_audio: true,
            maintain_pitch_shuttling: true,
            mute_input_during_recording: false,
            generate_waveforms_on_import: true,
            multithreaded_waveforms: false,
            render_audio_with_video: false,
            auto_tag_audio_types: true,
            always_override_audio_tags: false,
            edit_in_audition_location: "nextToMedia".into(),
            dialogue_target_lufs: -23.0,
            music_target_lufs: -25.0,
            sfx_target_lufs: -21.0,
            ambience_target_lufs: -30.0,
        }
    }
}

impl AudioPrefs {
    /// Auto-Match target for an audio type.
    pub fn loudness_target(&self, t: filmcraft_project::AudioType) -> f64 {
        match t {
            filmcraft_project::AudioType::Dialogue => self.dialogue_target_lufs,
            filmcraft_project::AudioType::Music => self.music_target_lufs,
            filmcraft_project::AudioType::Sfx => self.sfx_target_lufs,
            filmcraft_project::AudioType::Ambience => self.ambience_target_lufs,
        }
    }
    fn clamp(&mut self) {
        self.automatch_time = if self.automatch_time.is_finite() { self.automatch_time.clamp(0.0, 30.0) } else { 1.0 };
        self.large_volume_adjustment = if self.large_volume_adjustment.is_finite() { self.large_volume_adjustment.clamp(0.0, 96.0) } else { 6.0 };
        self.minimum_time_ms = self.minimum_time_ms.clamp(1, 10_000);
        for (v, d) in [
            (&mut self.dialogue_target_lufs, -23.0),
            (&mut self.music_target_lufs, -25.0),
            (&mut self.sfx_target_lufs, -21.0),
            (&mut self.ambience_target_lufs, -30.0),
        ] {
            *v = if v.is_finite() { v.clamp(-60.0, 0.0) } else { d };
        }
    }
}

impl Preferences {
    fn clamp(&mut self) {
        self.auto_save.clamp();
        self.audio.clamp();
        let secs = |v: f64| if v.is_finite() { v.clamp(0.0, 60.0) } else { 0.0 };
        self.playback.preroll_seconds = secs(self.playback.preroll_seconds);
        self.playback.postroll_seconds = secs(self.playback.postroll_seconds);
        self.trim.large_trim_offset = self.trim.large_trim_offset.clamp(1, 1000);
        self.labels.sanitize_labels();
        self.appearance.sync_color_theme();
        self.version = settings::PREFS_VERSION;
    }

    /// Parse a preferences document: migrate older layouts, repair values against the schema.
    pub fn from_value(mut v: Value) -> Self {
        settings::migrate(&mut v);
        settings::sanitize(&mut v, &Self::default().to_value());
        let mut p: Preferences = serde_json::from_value(v).unwrap_or_default();
        p.clamp();
        p
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    /// All leaf keys (`autoSave.enabled`, …).
    pub fn keys(&self) -> Vec<String> {
        fn walk(prefix: &str, v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for (k, c) in m {
                        walk(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, c, out);
                    }
                }
                _ => out.push(prefix.to_string()),
            }
        }
        let mut out = Vec::new();
        walk("", &self.to_value(), &mut out);
        out
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        let v = self.to_value();
        key.split('.').try_fold(&v, |v, k| v.get(k)).cloned()
    }

    /// Set one key (type-checked against the schema, values clamped to their ranges).
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), String> {
        let mut v = self.to_value();
        let mut slot = &mut v;
        for k in key.split('.') {
            slot = slot.get_mut(k).ok_or_else(|| format!("unknown preference `{key}`"))?;
        }
        if slot.is_object() {
            return Err(format!("`{key}` is a group; set one of its keys"));
        }
        settings::validate(key, &value)?;
        let value = match (&*slot, value) {
            // Numeric choices (buffer size, sample rate) arrive as strings from the dialog.
            (Value::Number(_), Value::String(t)) if t.parse::<u64>().is_ok() => Value::from(t.parse::<u64>().unwrap_or_default()),
            // Accept 5.0 for integer prefs (agents often send floats).
            (Value::Number(n), Value::Number(m)) if n.is_u64() && !m.is_u64() => {
                Value::from(m.as_f64().filter(|f| f.is_finite() && *f >= 0.0).ok_or_else(|| format!("`{key}` must be a non-negative number"))?.round() as u64)
            }
            (_, v) => v,
        };
        *slot = value;
        let _: Preferences = serde_json::from_value(v.clone()).map_err(|e| format!("`{key}`: {e}"))?;
        settings::sanitize(&mut v, &Self::default().to_value());
        let mut p: Preferences = serde_json::from_value(v).map_err(|e| format!("`{key}`: {e}"))?;
        // Old clients that set the single Color Theme still pick the theme they name (before
        // `clamp`, which brings `colorTheme` back in line with the mode and theme choices).
        if key == "appearance.colorTheme" {
            let theme = p.appearance.color_theme.clone();
            p.appearance.select_theme(&theme);
        }
        p.clamp();
        *self = p;
        Ok(())
    }

    pub fn load(path: &Path) -> Self {
        fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).map(Self::from_value).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        filmcraft_format::atomic_write(path, &serde_json::to_vec_pretty(self).unwrap_or_default())
    }
}

/// A file with this name next to the executable makes the install portable: its data directory is
/// `<exe dir>/data` instead of the per-user one (the Windows portable zip ships it).
pub const PORTABLE_MARKER: &str = "portable.txt";

/// `<exe_dir>/data` when `exe_dir` holds [`PORTABLE_MARKER`], else `None`.
pub fn portable_data_dir(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir.join(PORTABLE_MARKER).is_file().then(|| exe_dir.join("data"))
}

/// The data directory. `FILMCRAFT_DATA_DIR` overrides; a portable install ([`PORTABLE_MARKER`] next
/// to the executable) uses `<exe dir>/data`; otherwise the per-user directory:
/// macOS `~/Library/Application Support/FilmCraft`, Windows `%APPDATA%\FilmCraft`,
/// elsewhere `$XDG_DATA_HOME/filmcraft` or `~/.local/share/filmcraft`.
pub fn default_data_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(d) = env("FILMCRAFT_DATA_DIR") {
        return Some(d);
    }
    if let Some(d) = std::env::current_exe().ok().and_then(|exe| exe.parent().and_then(portable_data_dir)) {
        return Some(d);
    }
    if cfg!(target_os = "macos") {
        return env("HOME").map(|h| h.join("Library/Application Support/FilmCraft"));
    }
    if cfg!(windows) {
        return env("APPDATA").map(|h| h.join("FilmCraft"));
    }
    env("XDG_DATA_HOME").map(|d| d.join("filmcraft")).or_else(|| env("HOME").map(|h| h.join(".local/share/filmcraft")))
}

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ------------------------------------------------------------------ recovery candidates

/// Snapshot meta written next to `snapshot.fcproj`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JournalMeta {
    pub pid: u32,
    pub started_unix: i64,
    pub saved_unix: i64,
    pub revision: u64,
    pub project_name: String,
    pub project_path: Option<String>,
    pub generator: String,
    pub bytes: usize,
    /// Stable fingerprint of the paired snapshot. Older journals omit this and remain readable.
    #[serde(default)]
    pub snapshot_fingerprint: Option<u64>,
    /// True when the app was closed normally while changes were unsaved (no crash).
    pub clean_exit: bool,
    /// Whether project content was unsaved when this snapshot was written.
    /// Older journals without the field keep being offered (the default is true).
    #[serde(default = "journal_dirty_by_default")]
    pub dirty: bool,
    /// Whether the project's saved Timeline view differed from the journaled view. Defaults to
    /// false for journals created before clean-project view changes were recoverable.
    #[serde(default)]
    pub view_dirty: bool,
    /// Epoch of the compact view overlay beside `snapshot.fcproj`, when one was committed.
    #[serde(default)]
    pub view_snapshot_epoch: Option<u64>,
    /// Stable fingerprint for the compact view overlay.
    #[serde(default)]
    pub view_fingerprint: Option<u64>,
}

fn journal_dirty_by_default() -> bool {
    true
}

/// Unsaved changes left behind by a session that is no longer running.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCandidate {
    /// Session directory name (pass to `file.recover {id}`).
    pub id: String,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(flatten)]
    pub meta: JournalMeta,
}

const SNAPSHOT: &str = "snapshot.fcproj";
const META: &str = "session.json";
const PENDING_META: &str = "session.pending.json";
const LOCK: &str = "lock";

pub fn recovery_root(data_dir: &Path) -> PathBuf {
    data_dir.join("Recovery")
}

/// Is the session directory's lock free (owner gone)?
fn stale_session_lock(dir: &Path) -> (Option<File>, bool) {
    let Ok(f) = File::options().read(true).write(true).open(dir.join(LOCK)) else { return (None, false) };
    match f.try_lock() {
        // Keep the acquired handle alive while inspecting and possibly removing a clean journal,
        // so a new session cannot start between the stale check and deletion.
        Ok(()) => (Some(f), false),
        Err(fs::TryLockError::WouldBlock) => (None, true),
        // Unknown locking failures must not authorize deletion. A dirty snapshot can still be
        // offered for recovery, but clean journals are retained without a confirmed stale lock.
        Err(fs::TryLockError::Error(_)) => (None, false),
    }
}

/// Find snapshots of sessions that are no longer running without deleting session data.
/// `skip` is the caller's own session directory; stale empty and clean sessions are cleaned at
/// persistence startup instead.
pub fn scan_candidates(data_dir: &Path, skip: Option<&Path>) -> Vec<RecoveryCandidate> {
    let Ok(rd) = fs::read_dir(recovery_root(data_dir)) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let dir = e.path();
        if !dir.is_dir() || Some(dir.as_path()) == skip {
            continue;
        }
        let (_, active) = stale_session_lock(&dir);
        if active {
            continue;
        }
        let snap = dir.join(SNAPSHOT);
        if !snap.is_file() {
            // A listing is read-only for a live or not-yet-locked session.
            continue;
        }
        // Keep the snapshot even when session.json is missing or unreadable: the project
        // is still recoverable. Meta fields fall back to defaults.
        let meta = match read_matching_meta(&dir) {
            Ok(Some(meta)) => meta,
            Ok(None) => JournalMeta {
                // a missing meta must never hide a real recovery
                dirty: true,
                ..JournalMeta::default()
            },
            Err(()) => continue,
        };
        if !meta.dirty && !meta.view_dirty {
            // A snapshot with neither unsaved content nor an unsaved view is not a recovery
            // candidate. Stale clean journals are removed at startup, not while listing.
            continue;
        }
        out.push(RecoveryCandidate { id: e.file_name().to_string_lossy().into_owned(), dir, meta });
    }
    out.sort_by(|a, b| b.meta.saved_unix.cmp(&a.meta.saved_unix).then(b.id.cmp(&a.id)));
    out
}

/// Remove stale empty and clean journals during persistence startup, before starting this
/// session's worker. The acquired lock stays alive until removal completes; active, unlocked, or
/// unreadable sessions are preserved.
fn cleanup_stale_sessions(data_dir: &Path) {
    cleanup_stale_sessions_at(data_dir, std::time::SystemTime::now());
}

fn cleanup_stale_sessions_at(data_dir: &Path, now: std::time::SystemTime) {
    let Ok(rd) = fs::read_dir(recovery_root(data_dir)) else { return };
    for entry in rd.filter_map(Result::ok) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let (stale_lock, active) = stale_session_lock(&dir);
        let Some(_stale_lock) = stale_lock else { continue };
        if active {
            continue;
        }
        let snapshot = dir.join(SNAPSHOT);
        let should_remove = if !snapshot.is_file() {
            // A new session creates its directory before opening and locking `lock`. Keep that
            // brief startup window out of cleanup; abandoned empty directories age out later.
            fs::metadata(&dir)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= Duration::from_secs(300))
        } else {
            read_matching_meta(&dir).ok().flatten().is_some_and(|meta| !meta.dirty && !meta.view_dirty)
        };
        if should_remove {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// Load a candidate's project snapshot and, when present, its separately journaled view.
pub fn load_candidate(c: &RecoveryCandidate) -> Result<filmcraft_format::Loaded, String> {
    let b = filmcraft_media::reader::read_regular_file_bounded(&c.dir.join(SNAPSHOT), filmcraft_format::MAX_PROJECT_BYTES)
        .map_err(|e| format!("{}: {e}", c.dir.display()))?;
    if c.meta.snapshot_fingerprint.is_some_and(|fingerprint| snapshot_fingerprint(&b) != fingerprint) {
        return Err(format!("{}: recovery snapshot changed after it was listed", c.dir.display()));
    }
    let mut loaded = filmcraft_format::decode(&b).map_err(|e| e.to_string())?;
    match (c.meta.view_snapshot_epoch, c.meta.view_fingerprint) {
        (Some(epoch), Some(fingerprint)) => {
            let path = c.dir.join(format!("view-{epoch}.json"));
            let overlay = filmcraft_media::reader::read_regular_file_bounded(&path, 8 << 20).map_err(|e| e.to_string()).and_then(|bytes| {
                if snapshot_fingerprint(&bytes) != fingerprint {
                    return Err("recovery view fingerprint does not match".to_string());
                }
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())
            });
            match overlay {
                Ok(view) => loaded.view = Some(view),
                Err(error) => log::warn!("{}: recovery view ignored, using the snapshot view: {error}", path.display()),
            }
        }
        (None, None) => {}
        _ => log::warn!("{}: incomplete recovery view metadata; using the snapshot view", c.dir.display()),
    }
    Ok(loaded)
}

/// A clean recovery snapshot is clean only while the project file still contains its content.
pub fn candidate_content_matches_project(c: &RecoveryCandidate, recovered: &filmcraft_project::Project) -> bool {
    let Some(path) = c.meta.project_path.as_deref().filter(|path| !path.is_empty()) else { return false };
    let Ok(bytes) = filmcraft_media::reader::read_regular_file_bounded(std::path::Path::new(path), filmcraft_format::MAX_PROJECT_BYTES) else {
        return false;
    };
    filmcraft_format::decode(&bytes).is_ok_and(|loaded| loaded.project == *recovered)
}

pub fn discard_candidate(c: &RecoveryCandidate) -> io::Result<()> {
    fs::remove_dir_all(&c.dir)
}

// ------------------------------------------------------------------ worker

/// Host configuration for [`Persistence::start`].
#[derive(Clone)]
pub struct AutosaveConfig {
    pub data_dir: PathBuf,
    /// Local UTC offset (seconds) at a unix time — used for auto-save file names and display.
    pub local_offset: fn(i64) -> i32,
    /// Quiet period after a change before the journal is written.
    pub journal_debounce: Duration,
}

impl AutosaveConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), local_offset: |_| 0, journal_debounce: Duration::from_millis(1000) }
    }
}

/// What the session last told the worker.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sent {
    pub revision: u64,
    pub saved_revision: u64,
    pub path: Option<String>,
    /// Bumped when only the stored Timeline view (zoom, scroll, sliders) changed. Tracked
    /// separately from `revision` so per-frame view changes never invalidate revision-keyed
    /// caches, while still reaching the autosave journal.
    pub view_epoch: u64,
    /// The view counter as of the last save: it must travel in the comparison too, or a save
    /// that catches the view up sends nothing and leaves the journal behind.
    pub saved_view_epoch: u64,
}

pub(crate) struct Snapshot {
    pub(crate) project: Arc<Project>,
    pub(crate) view: filmcraft_project::ProjectView,
    pub(crate) revision: u64,
    /// The session's view counter at this snapshot, so recovery carries its view.
    pub(crate) view_epoch: u64,
    /// Whether the project content was unsaved when this snapshot was taken. A clean project's
    /// unsaved view is journaled separately through the compact view overlay.
    pub(crate) content_dirty: bool,
    /// Whether the stored view had not reached the project file when this snapshot was taken.
    pub(crate) view_dirty: bool,
    pub(crate) path: Option<String>,
}

enum Msg {
    /// The project has unsaved changes at this revision. Boxed: the other messages are a few
    /// words and the enum is sent after every command.
    Dirty(Box<Snapshot>),
    /// Nothing unsaved (just saved / opened): drop the journal.
    Clean,
    Prefs(AutoSavePrefs),
    AutoSaveNow(Sender<Result<PathBuf, String>>),
    Flush(Sender<Result<Option<u64>, String>>),
    Stop,
}

/// Collapse a run of queued snapshots to the newest state before doing worker I/O. Other message
/// types are barriers: their order relative to snapshots (in particular Clean and Flush) matters.
fn coalesce_pending_dirty(rx: &Receiver<Msg>, mut latest: Box<Snapshot>, deferred: &mut Option<Msg>) -> Box<Snapshot> {
    loop {
        match rx.try_recv() {
            Ok(Msg::Dirty(snapshot)) => latest = snapshot,
            Ok(other) => {
                *deferred = Some(other);
                break;
            }
            Err(_) => break,
        }
    }
    latest
}

/// Worker → session notifications.
#[derive(Clone, Debug)]
pub enum WorkerEvent {
    Journaled {
        revision: u64,
        unix: i64,
        serialize_ms: f64,
        write_ms: f64,
        bytes: usize,
    },
    AutoSaved {
        path: PathBuf,
        revision: u64,
        unix: i64,
        serialize_ms: f64,
        write_ms: f64,
        bytes: usize,
    },
    /// "Auto Save also saves the current project" wrote the project file at this revision.
    SavedProject {
        path: String,
        revision: u64,
        view_epoch: u64,
    },
    Error(String),
}

const WORKER_STOPPED: &str =
    "Auto-save worker stopped; automatic saving and recovery journaling are unavailable. Use File > Save or Save As to save your changes.";

/// Shared by the thread boundary and callers: a stopped worker is one failure, even when
/// several queued requests or subsequent edits discover the disconnected channel.
struct WorkerFailure {
    reported: AtomicBool,
    events: Sender<WorkerEvent>,
}

impl WorkerFailure {
    fn report(&self) {
        if !self.reported.swap(true, Ordering::AcqRel) {
            log::error!("{WORKER_STOPPED}");
            let _ = self.events.send(WorkerEvent::Error(WORKER_STOPPED.into()));
        }
    }
}

/// Last activity, for `file.autoSaveStatus` and the UI.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub last_auto_save: Option<String>,
    pub last_auto_save_unix: Option<i64>,
    pub auto_saves_written: u64,
    pub last_journal_unix: Option<i64>,
    pub journaled_revision: u64,
    pub journals_written: u64,
    /// Worker-side cost of the last write (serialize = JSON encode of the snapshot).
    pub last_serialize_ms: f64,
    pub last_write_ms: f64,
    pub last_bytes: usize,
    pub last_error: Option<String>,
}

/// The running auto-save/recovery machinery of one session.
pub struct Persistence {
    pub data_dir: PathBuf,
    pub session_dir: PathBuf,
    pub local_offset: fn(i64) -> i32,
    pub candidates: Vec<RecoveryCandidate>,
    pub status: Status,
    pub(crate) last_sent: Option<Sent>,
    lock: Option<File>,
    tx: Sender<Msg>,
    events: Receiver<WorkerEvent>,
    thread: Option<JoinHandle<()>>,
    failure: Arc<WorkerFailure>,
}

impl Persistence {
    /// Create this session's journal directory (holding its lock), find recovery candidates left by
    /// dead sessions, and start the worker thread.
    pub fn start(cfg: &AutosaveConfig, prefs: AutoSavePrefs) -> io::Result<Self> {
        let root = recovery_root(&cfg.data_dir);
        fs::create_dir_all(&root)?;
        cleanup_stale_sessions(&cfg.data_dir);
        let started = unix_now();
        let pid = std::process::id();
        // Unique even for several sessions in one process within a second (tests, future multi-window).
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session_dir = root.join(if seq == 0 { format!("{started}-{pid}") } else { format!("{started}-{pid}-{seq}") });
        fs::create_dir_all(&session_dir)?;
        let lock = File::options().create(true).truncate(false).read(true).write(true).open(session_dir.join(LOCK))?;
        // Do not start a worker without owning its lock: cleanup may otherwise mistake this
        // session for a stale empty directory. Unsupported locking remains a usable fallback.
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(io::Error::new(io::ErrorKind::WouldBlock, "recovery session lock is already held")),
            Err(fs::TryLockError::Error(_)) => {}
        }
        let candidates = scan_candidates(&cfg.data_dir, Some(&session_dir));
        let (tx, rx) = channel();
        let (etx, events) = channel();
        let failure = Arc::new(WorkerFailure { reported: AtomicBool::new(false), events: etx.clone() });
        let mut w = Worker {
            data_dir: cfg.data_dir.clone(),
            session_dir: session_dir.clone(),
            local_offset: cfg.local_offset,
            debounce: cfg.journal_debounce,
            prefs,
            events: etx,
            latest: None,
            journaled: 0,
            journaled_view: 0,
            journaled_content_dirty: false,
            journaled_view_dirty: false,
            autosaved: 0,
            first_pending: None,
            last_change: Instant::now(),
            next_autosave: Instant::now(),
            meta: JournalMeta { pid, started_unix: started, generator: filmcraft_format::generator(), ..Default::default() },
        };
        w.next_autosave = Instant::now() + w.autosave_interval();
        let worker_failure = failure.clone();
        let thread = std::thread::Builder::new().name("filmcraft-autosave".into()).spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| w.run(rx))).is_err() {
                worker_failure.report();
            }
        })?;
        Ok(Self {
            data_dir: cfg.data_dir.clone(),
            session_dir,
            local_offset: cfg.local_offset,
            candidates,
            status: Status::default(),
            last_sent: None,
            lock: Some(lock),
            tx,
            events,
            thread: Some(thread),
            failure,
        })
    }

    pub(crate) fn send_dirty(&self, snapshot: Snapshot) {
        self.send(Msg::Dirty(Box::new(snapshot)));
    }

    pub(crate) fn send_clean(&self) {
        self.send(Msg::Clean);
    }

    pub fn set_prefs(&self, p: AutoSavePrefs) {
        self.send(Msg::Prefs(p));
    }

    fn send(&self, msg: Msg) {
        if self.tx.send(msg).is_err() {
            self.failure.report();
        }
    }

    fn stopped_error(&self) -> String {
        self.failure.report();
        WORKER_STOPPED.into()
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.failure.reported.load(Ordering::Acquire) && self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Write any pending journal now and return its confirmed revision (`None` when clean).
    /// Disabled journaling, failed writes and a stopped/unresponsive worker are errors.
    pub fn flush(&self) -> Result<Option<u64>, String> {
        let (tx, rx) = channel();
        self.tx.send(Msg::Flush(tx)).map_err(|_| self.stopped_error())?;
        rx.recv_timeout(Duration::from_secs(30)).map_err(|e| match e {
            RecvTimeoutError::Timeout => "recovery journal flush timed out".to_string(),
            RecvTimeoutError::Disconnected => {
                self.failure.report();
                "auto-save worker stopped before confirming the recovery journal".to_string()
            }
        })?
    }

    /// Write an auto-save now (if there are unsaved changes) and wait for the result.
    pub fn auto_save_now(&self) -> Result<Option<PathBuf>, String> {
        let (tx, rx) = channel();
        self.tx.send(Msg::AutoSaveNow(tx)).map_err(|_| self.stopped_error())?;
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(p)) => Ok(Some(p)),
            Ok(Err(e)) if e.is_empty() => Ok(None),
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Err("auto-save timed out".into()),
            Err(RecvTimeoutError::Disconnected) => Err(self.stopped_error()),
        }
    }

    pub fn drain_events(&mut self) -> Vec<WorkerEvent> {
        let ev: Vec<_> = self.events.try_iter().collect();
        for e in &ev {
            match e {
                WorkerEvent::Journaled { revision, unix, serialize_ms, write_ms, bytes } => {
                    self.status.last_journal_unix = Some(*unix);
                    self.status.journaled_revision = *revision;
                    self.status.journals_written += 1;
                    (self.status.last_serialize_ms, self.status.last_write_ms, self.status.last_bytes) = (*serialize_ms, *write_ms, *bytes);
                }
                WorkerEvent::AutoSaved { path, unix, serialize_ms, write_ms, bytes, .. } => {
                    self.status.last_auto_save = Some(path.to_string_lossy().into_owned());
                    self.status.last_auto_save_unix = Some(*unix);
                    self.status.auto_saves_written += 1;
                    (self.status.last_serialize_ms, self.status.last_write_ms, self.status.last_bytes) = (*serialize_ms, *write_ms, *bytes);
                }
                WorkerEvent::SavedProject { .. } => {}
                WorkerEvent::Error(m) => self.status.last_error = Some(m.clone()),
            }
        }
        ev
    }

    /// Stop the worker. A session that still has a journal (closed with unsaved changes) keeps its
    /// directory — marked as a clean exit — so the changes are offered next time; otherwise the
    /// directory is removed.
    pub fn close(mut self) {
        if let Err(e) = self.flush() {
            log::warn!("auto-save at shutdown: {e}");
        }
        self.stop_worker();
        let snap = self.session_dir.join(SNAPSHOT);
        self.lock.take();
        if snap.is_file() {
            if let Some(mut m) = fs::read(self.session_dir.join(META)).ok().and_then(|b| serde_json::from_slice::<JournalMeta>(&b).ok()) {
                m.clean_exit = true;
                let _ = filmcraft_format::atomic_write(&self.session_dir.join(META), &serde_json::to_vec_pretty(&m).unwrap_or_default());
            }
        } else {
            let _ = fs::remove_dir_all(&self.session_dir);
        }
    }

    /// Stop the worker and release the lock *without* cleaning up — what a crash leaves behind.
    #[doc(hidden)]
    pub fn simulate_crash(mut self) {
        let _ = self.flush();
        self.stop_worker();
        self.lock.take();
    }

    fn stop_worker(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            self.failure.report();
        }
    }

    /// Display form of a unix time in local time.
    pub fn local_time(&self, unix: i64) -> String {
        names::display_time(unix, (self.local_offset)(unix))
    }

    pub fn candidates_json(&self) -> Value {
        Value::Array(
            self.candidates
                .iter()
                .map(|c| {
                    let mut v = serde_json::to_value(c).unwrap_or_default();
                    v["savedAt"] = json!(self.local_time(c.meta.saved_unix));
                    v
                })
                .collect(),
        )
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

struct Worker {
    data_dir: PathBuf,
    session_dir: PathBuf,
    local_offset: fn(i64) -> i32,
    debounce: Duration,
    prefs: AutoSavePrefs,
    events: Sender<WorkerEvent>,
    /// Newest unsaved state (None = clean).
    latest: Option<Snapshot>,
    /// Revision currently in the journal (0 = none) / last auto-saved revision. The view
    /// counters sit beside them: a view-only change must journal again at the same revision.
    journaled: u64,
    journaled_view: u64,
    journaled_content_dirty: bool,
    journaled_view_dirty: bool,
    autosaved: u64,
    first_pending: Option<Instant>,
    last_change: Instant,
    next_autosave: Instant,
    meta: JournalMeta,
}

impl Worker {
    fn autosave_interval(&self) -> Duration {
        Duration::from_secs(self.prefs.interval_minutes as u64 * 60)
    }

    fn journal_due(&self) -> Option<Instant> {
        let s = self.latest.as_ref()?;
        if !self.prefs.recovery_journal
            || journal_state_matches(s, self.journaled, self.journaled_view, self.journaled_content_dirty, self.journaled_view_dirty)
        {
            return None;
        }
        let max = self.first_pending.unwrap_or(self.last_change) + Duration::from_secs(self.prefs.recovery_interval_seconds as u64);
        Some((self.last_change + self.debounce).min(max))
    }

    fn run(mut self, rx: Receiver<Msg>) {
        let mut deferred = None;
        loop {
            let due = [self.journal_due(), self.prefs.enabled.then_some(self.next_autosave)].into_iter().flatten().min();
            let msg = if deferred.is_some() {
                deferred.take()
            } else {
                match due {
                    Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                        Ok(m) => Some(m),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    },
                    None => match rx.recv() {
                        Ok(m) => Some(m),
                        Err(_) => return,
                    },
                }
            };
            let msg = match msg {
                Some(Msg::Dirty(snapshot)) => Some(Msg::Dirty(coalesce_pending_dirty(&rx, snapshot, &mut deferred))),
                other => other,
            };
            match msg {
                Some(Msg::Dirty(s)) => {
                    let now = Instant::now();
                    self.first_pending.get_or_insert(now);
                    self.last_change = now;
                    self.latest = Some(*s);
                }
                Some(Msg::Clean) => {
                    let had_pending = self.latest.take().is_some();
                    self.first_pending = None;
                    if had_pending || has_recovery_files(&self.session_dir) {
                        self.clear_journal();
                    }
                }
                Some(Msg::Prefs(p)) => {
                    let was = self.autosave_interval();
                    let journal_was = self.prefs.recovery_journal;
                    self.prefs = p;
                    if self.autosave_interval() != was {
                        self.next_autosave = Instant::now() + self.autosave_interval();
                    }
                    if journal_was && !self.prefs.recovery_journal {
                        self.clear_journal();
                    }
                }
                Some(Msg::AutoSaveNow(reply)) => {
                    let r = if self.latest.as_ref().is_some_and(|snapshot| snapshot.content_dirty) { self.auto_save() } else { Err(String::new()) };
                    let _ = reply.send(r);
                }
                Some(Msg::Flush(ack)) => {
                    let result = match self.latest.as_ref() {
                        None => Ok(None),
                        Some(_) if !self.prefs.recovery_journal => Err("recovery journal is disabled".to_string()),
                        Some(s) if journal_state_matches(s, self.journaled, self.journaled_view, self.journaled_content_dirty, self.journaled_view_dirty) => {
                            Ok(Some(self.journaled))
                        }
                        Some(_) => self.write_journal().map(Some),
                    };
                    let _ = ack.send(result);
                }
                Some(Msg::Stop) => return,
                None => {}
            }
            let now = Instant::now();
            if self.journal_due().is_some_and(|d| d <= now) {
                // The worker reports write failures through its event channel.
                let _ = self.write_journal();
            }
            if self.prefs.enabled && self.next_autosave <= now {
                // Defensive for any future view-only snapshot producer: it is not unsaved content
                // and must not fill the auto-save ring with copies of the saved project.
                if self.latest.as_ref().is_some_and(|s| s.content_dirty && s.revision != self.autosaved) {
                    let _ = self.auto_save();
                }
                self.next_autosave = now + self.autosave_interval();
            }
        }
    }

    fn error(&self, e: String) {
        log::warn!("auto-save: {e}");
        let _ = self.events.send(WorkerEvent::Error(e));
    }

    fn write_journal(&mut self) -> Result<u64, String> {
        let Some(s) = &self.latest else { return Err("no unsaved recovery state to write".into()) };
        let t0 = Instant::now();
        let stored_meta = fs::read(self.session_dir.join(META)).ok().and_then(|bytes| serde_json::from_slice::<JournalMeta>(&bytes).ok());
        let view_only = s.revision == self.journaled
            && self.session_dir.join(SNAPSHOT).is_file()
            && stored_meta.as_ref().is_some_and(|meta| meta.revision == s.revision && meta.snapshot_fingerprint.is_some());
        let unix = unix_now();
        let bytes = if view_only {
            serde_json::to_vec(&s.view).map_err(|e| format!("serialize recovery view: {e}"))?
        } else {
            filmcraft_format::encode_with_view(&s.project, Some(&s.view), false)
        };
        let serialize_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let bytes_written = bytes.len();
        let t1 = Instant::now();
        let mut meta = self.meta.clone();
        meta.saved_unix = unix;
        meta.revision = s.revision;
        meta.project_name = s.project.name.clone();
        meta.project_path = s.path.clone();
        meta.dirty = s.content_dirty;
        meta.view_dirty = s.view_dirty;
        let r = if view_only {
            write_view_overlay(&self.session_dir, &bytes, s, &meta)
        } else {
            meta.bytes = bytes_written;
            meta.view_snapshot_epoch = None;
            meta.view_fingerprint = None;
            write_recovery_pair(&self.session_dir, &bytes, &meta, filmcraft_format::atomic_write).map(|()| meta)
        };
        let write_ms = t1.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok(meta) => {
                self.meta = meta;
                self.journaled = s.revision;
                self.journaled_view = s.view_epoch;
                self.journaled_content_dirty = s.content_dirty;
                self.journaled_view_dirty = s.view_dirty;
                self.first_pending = None;
                let _ = self.events.send(WorkerEvent::Journaled { revision: s.revision, unix, serialize_ms, write_ms, bytes: bytes_written });
                Ok(s.revision)
            }
            Err(e) => {
                // Try again at the next interval rather than spinning.
                self.first_pending = Some(Instant::now());
                self.last_change = Instant::now();
                let message = format!("recovery journal: {e}");
                self.error(message.clone());
                Err(message)
            }
        }
    }

    pub(crate) fn clear_journal(&mut self) {
        Self::clear_journal_files(&self.session_dir);
        self.journaled = 0;
        self.journaled_view = 0;
        self.journaled_content_dirty = false;
        self.journaled_view_dirty = false;
    }

    fn clear_journal_files(session_dir: &Path) {
        if let Some(epoch) =
            fs::read(session_dir.join(META)).ok().and_then(|bytes| serde_json::from_slice::<JournalMeta>(&bytes).ok()).and_then(|meta| meta.view_snapshot_epoch)
        {
            let _ = fs::remove_file(session_dir.join(format!("view-{epoch}.json")));
        }
        let _ = fs::remove_file(session_dir.join(META));
        let _ = fs::remove_file(session_dir.join(PENDING_META));
        let _ = fs::remove_file(session_dir.join(SNAPSHOT));
    }

    fn auto_save(&mut self) -> Result<PathBuf, String> {
        let Some(s) = &self.latest else { return Err("nothing to save".into()) };
        let scratch = crate::project_tools::trusted_scratch(s.project.settings.scratch.auto_save.as_deref(), s.path.as_deref(), &self.prefs.scratch_folders);
        let (dir, name) = match (&s.path, scratch) {
            // Project Settings ▸ Scratch Disks ▸ Project Auto Save
            (Some(p), Some(scratch)) => {
                let p = Path::new(p);
                (scratch, p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| s.project.name.clone()))
            }
            (Some(p), None) => {
                let p = Path::new(p);
                (names::auto_save_dir(p), p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| s.project.name.clone()))
            }
            (None, _) => (self.data_dir.join(names::AUTO_SAVE_DIR), s.project.name.clone()),
        };
        let t0 = Instant::now();
        let bytes = filmcraft_format::encode_with_view(&s.project, Some(&s.view), false);
        let serialize_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let unix = unix_now();
        let r = names::write_auto_save(&dir, &name, unix, (self.local_offset)(unix), &bytes, self.prefs.max_versions as usize);
        let write_ms = t1.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok(path) => {
                self.autosaved = s.revision;
                let _ = self.events.send(WorkerEvent::AutoSaved { path: path.clone(), revision: s.revision, unix, serialize_ms, write_ms, bytes: bytes.len() });
                if self.prefs.save_current_project
                    && let Some(p) = &s.path
                {
                    match filmcraft_format::atomic_write(Path::new(p), &bytes) {
                        Ok(()) => {
                            let _ = self.events.send(WorkerEvent::SavedProject { path: p.clone(), revision: s.revision, view_epoch: s.view_epoch });
                        }
                        Err(e) => self.error(format!("saving {p}: {e}")),
                    }
                }
                Ok(path)
            }
            Err(e) => {
                let m = format!("auto-save to {}: {e}", dir.display());
                self.error(m.clone());
                Err(m)
            }
        }
    }
}

fn journal_state_matches(snapshot: &Snapshot, revision: u64, view_epoch: u64, content_dirty: bool, view_dirty: bool) -> bool {
    snapshot.revision == revision && snapshot.view_epoch == view_epoch && snapshot.content_dirty == content_dirty && snapshot.view_dirty == view_dirty
}

/// Write only the changed view beside the already journaled project snapshot. The immutable,
/// epoch-named sidecar is written first; atomically switching the metadata pointer commits it
/// while leaving the previous view usable if the process stops mid-write.
fn write_view_overlay(dir: &Path, bytes: &[u8], snapshot: &Snapshot, desired: &JournalMeta) -> io::Result<JournalMeta> {
    let meta_path = dir.join(META);
    let previous = fs::read(&meta_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JournalMeta>(&bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "journal metadata is missing or invalid"))?;
    let Some(base_fingerprint) = previous.snapshot_fingerprint else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "base recovery snapshot has no fingerprint"));
    };
    if previous.revision != snapshot.revision {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "base recovery snapshot revision changed"));
    }
    let path = dir.join(format!("view-{}.json", snapshot.view_epoch));
    filmcraft_format::atomic_write(&path, bytes)?;

    let mut meta = desired.clone();
    meta.bytes = previous.bytes;
    meta.snapshot_fingerprint = Some(base_fingerprint);
    meta.view_snapshot_epoch = Some(snapshot.view_epoch);
    meta.view_fingerprint = Some(snapshot_fingerprint(bytes));
    let encoded = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
    filmcraft_format::atomic_write(&meta_path, &encoded)?;

    if let Some(old_epoch) = previous.view_snapshot_epoch.filter(|epoch| *epoch != snapshot.view_epoch) {
        let _ = fs::remove_file(dir.join(format!("view-{old_epoch}.json")));
    }
    Ok(meta)
}

/// Keep dirty metadata ahead of a dirty snapshot and clean metadata behind a clean snapshot.
/// Otherwise an interruption can leave a dirty snapshot labelled clean and the scan deletes it.
fn write_recovery_pair(dir: &Path, snapshot: &[u8], meta: &JournalMeta, mut write: impl FnMut(&Path, &[u8]) -> io::Result<()>) -> io::Result<()> {
    // The clean-metadata ordering is defensive for legacy data and future producers.
    let mut meta = meta.clone();
    let previous_meta =
        read_matching_meta(dir).ok().flatten().or_else(|| fs::read(dir.join(META)).ok().and_then(|bytes| serde_json::from_slice::<JournalMeta>(&bytes).ok()));
    let previous_recoverable = previous_meta.as_ref().map_or_else(|| dir.join(SNAPSHOT).is_file(), |previous| previous.dirty || previous.view_dirty);
    let current_recoverable = meta.dirty || meta.view_dirty;
    let obsolete_view_epoch = previous_meta.as_ref().and_then(|previous| previous.view_snapshot_epoch).filter(|epoch| Some(*epoch) != meta.view_snapshot_epoch);
    meta.snapshot_fingerprint = Some(snapshot_fingerprint(snapshot));
    let meta_path = dir.join(META);
    let pending_meta_path = dir.join(PENDING_META);
    let snapshot_path = dir.join(SNAPSHOT);
    if current_recoverable && previous_recoverable {
        // Keep old metadata beside the old snapshot. The separate pending record describes the
        // new snapshot and becomes authoritative only after its fingerprint matches on disk.
        // An interrupted earlier replacement may have left its matching metadata only in the
        // pending file. Promote it before the next replacement overwrites that file.
        if let Some(previous) = previous_meta.as_ref() {
            let previous_bytes = serde_json::to_vec_pretty(previous).map_err(io::Error::other)?;
            write(&meta_path, &previous_bytes)?;
        }
        let mut pending = meta.clone();
        pending.view_snapshot_epoch = None;
        pending.view_fingerprint = None;
        let pending_bytes = serde_json::to_vec_pretty(&pending).map_err(io::Error::other)?;
        write(&pending_meta_path, &pending_bytes)?;
        write(&snapshot_path, snapshot)?;
        let metadata = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
        write(&meta_path, &metadata)?;
        match fs::remove_file(&pending_meta_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if let Some(epoch) = obsolete_view_epoch {
            let _ = fs::remove_file(dir.join(format!("view-{epoch}.json")));
        }
        Ok(())
    } else if current_recoverable {
        // Clean-to-dirty keeps its established order so a failed snapshot write cannot offer
        // an old clean snapshot as unsaved work.
        let metadata = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
        write(&meta_path, &metadata)?;
        write(&snapshot_path, snapshot)?;
        remove_obsolete_view_sidecar(dir, obsolete_view_epoch);
        let _ = fs::remove_file(&pending_meta_path);
        Ok(())
    } else {
        let metadata = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
        write(&snapshot_path, snapshot)?;
        write(&meta_path, &metadata)?;
        remove_obsolete_view_sidecar(dir, obsolete_view_epoch);
        let _ = fs::remove_file(&pending_meta_path);
        Ok(())
    }
}

fn remove_obsolete_view_sidecar(dir: &Path, epoch: Option<u64>) {
    if let Some(epoch) = epoch {
        let _ = fs::remove_file(dir.join(format!("view-{epoch}.json")));
    }
}

fn snapshot_fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3))
}

fn has_recovery_files(dir: &Path) -> bool {
    dir.join(SNAPSHOT).is_file() || dir.join(META).is_file() || dir.join(PENDING_META).is_file()
}

/// Select metadata that names the snapshot generation currently on disk. During a full
/// replacement, `session.pending.json` describes the new generation while `session.json`
/// continues to describe the old one. Either file is usable only when its fingerprint matches.
fn read_matching_meta(dir: &Path) -> Result<Option<JournalMeta>, ()> {
    let actual = read_snapshot_fingerprint(&dir.join(SNAPSHOT)).map_err(|_| ())?;
    let mut mismatch = false;
    for name in [PENDING_META, META] {
        let Some(meta) = fs::read(dir.join(name)).ok().and_then(|bytes| serde_json::from_slice::<JournalMeta>(&bytes).ok()) else { continue };
        if let Some(expected) = meta.snapshot_fingerprint {
            if expected == actual {
                return Ok(Some(meta));
            }
            mismatch = true;
        } else if name == META {
            // Legacy metadata has no fingerprint. It remains readable when no generation marker
            // matches it, preserving recovery from older project versions after an interrupted
            // replacement. A matching pending generation would already have returned above.
            return Ok(Some(meta));
        }
    }
    if mismatch { Err(()) } else { Ok(None) }
}

fn read_snapshot_fingerprint(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let limit = filmcraft_format::MAX_PROJECT_BYTES;
    if file.metadata()?.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "recovery snapshot exceeds the project size limit"));
    }
    let mut buffer = [0_u8; 8192];
    let mut hash = 0xcbf29ce484222325;
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(hash);
        }
        total = total.checked_add(count as u64).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "recovery snapshot size overflow"))?;
        if total > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "recovery snapshot grew beyond the project size limit"));
        }
        for byte in &buffer[..count] {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
}
#[cfg(test)]
mod durability_tests {
    use super::*;

    #[test]
    fn oversized_snapshot_fingerprints_are_rejected_before_reading() {
        let dir = TestDir::new("oversized-fingerprint");
        let path = dir.join("snapshot");
        let file = File::create(&path).unwrap();
        file.set_len(filmcraft_format::MAX_PROJECT_BYTES + 1).unwrap();
        assert_eq!(read_snapshot_fingerprint(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn recovery_rejects_a_snapshot_changed_after_candidate_listing() {
        let dir = TestDir::new("changed-candidate");
        let bytes = filmcraft_format::encode(&Project::new("Changed"), false);
        fs::write(dir.join(SNAPSHOT), &bytes).unwrap();
        let candidate = RecoveryCandidate {
            id: "changed".into(),
            dir: dir.to_path_buf(),
            meta: JournalMeta { snapshot_fingerprint: Some(snapshot_fingerprint(b"listed snapshot")), ..JournalMeta::default() },
        };
        assert!(load_candidate(&candidate).is_err());
    }

    #[test]
    fn mismatched_pending_generation_does_not_hide_legacy_metadata() {
        let dir = TestDir::new("legacy-pending-generation");
        let snapshot = filmcraft_format::encode(&Project::new("Legacy"), false);
        fs::write(dir.join(SNAPSHOT), &snapshot).unwrap();
        let legacy = JournalMeta { dirty: true, project_path: Some("legacy.fcproj".into()), ..JournalMeta::default() };
        fs::write(dir.join(META), serde_json::to_vec(&legacy).unwrap()).unwrap();
        let pending = JournalMeta { dirty: true, snapshot_fingerprint: Some(snapshot_fingerprint(b"new generation")), ..JournalMeta::default() };
        fs::write(dir.join(PENDING_META), serde_json::to_vec(&pending).unwrap()).unwrap();
        assert_eq!(read_matching_meta(&dir).unwrap(), Some(legacy));
    }

    #[test]
    fn journal_freshness_includes_saved_baseline_dirty_flags() {
        let snapshot = Snapshot {
            project: Arc::new(Project::default()),
            view: filmcraft_project::ProjectView::default(),
            revision: 5,
            view_epoch: 9,
            content_dirty: false,
            view_dirty: true,
            path: None,
        };
        assert!(journal_state_matches(&snapshot, 5, 9, false, true));
        assert!(!journal_state_matches(&snapshot, 5, 9, true, true), "a content save at the same revision still needs journaling");
    }

    #[test]
    fn worker_coalesces_queued_snapshots_without_reordering_control_messages() {
        let (tx, rx) = channel();
        let snapshot = |revision| {
            Box::new(Snapshot {
                project: Arc::new(Project::default()),
                view: filmcraft_project::ProjectView::default(),
                revision,
                view_epoch: revision,
                content_dirty: true,
                view_dirty: false,
                path: None,
            })
        };
        tx.send(Msg::Dirty(snapshot(1))).unwrap();
        tx.send(Msg::Dirty(snapshot(2))).unwrap();
        tx.send(Msg::Dirty(snapshot(3))).unwrap();
        tx.send(Msg::Clean).unwrap();

        let Some(Msg::Dirty(first)) = rx.recv().ok() else { panic!("first message should be a snapshot") };
        let mut deferred = None;
        let latest = coalesce_pending_dirty(&rx, first, &mut deferred);
        assert_eq!((latest.revision, latest.view_epoch), (3, 3));
        assert!(matches!(deferred, Some(Msg::Clean)), "Clean remains ordered after the latest snapshot");
    }

    #[test]
    fn repeated_clean_updates_do_not_clear_an_empty_journal() {
        let dir = TestDir::new("dedupe-clean");
        let session = dir.join("session");
        fs::create_dir_all(&session).unwrap();
        assert!(!has_recovery_files(&session));

        fs::write(session.join(SNAPSHOT), b"snapshot").unwrap();
        assert!(has_recovery_files(&session), "a dirty-to-clean transition still needs cleanup");
        fs::remove_file(session.join(SNAPSHOT)).unwrap();
        fs::write(session.join(META), b"metadata").unwrap();
        assert!(has_recovery_files(&session), "metadata left by a failed cleanup is retried");
        fs::remove_file(session.join(META)).unwrap();
        assert!(!has_recovery_files(&session), "later Clean messages do no file removal work");
    }

    #[test]
    fn clean_then_identical_view_snapshot_cannot_reuse_stale_pending_metadata() {
        let dir = TestDir::new("clean-pending-metadata");
        let cfg = AutosaveConfig::new(dir.to_path_buf());
        let persistence = Persistence::start(&cfg, AutoSavePrefs::default()).unwrap();
        let bytes = filmcraft_format::encode(&Project::new("Project"), false);
        let stale = JournalMeta {
            dirty: true,
            revision: 7,
            project_path: Some("old.fcproj".into()),
            snapshot_fingerprint: Some(snapshot_fingerprint(&bytes)),
            ..JournalMeta::default()
        };
        fs::write(persistence.session_dir.join(SNAPSHOT), &bytes).unwrap();
        fs::write(persistence.session_dir.join(META), serde_json::to_vec(&stale).unwrap()).unwrap();
        fs::write(persistence.session_dir.join(PENDING_META), serde_json::to_vec(&stale).unwrap()).unwrap();

        persistence.tx.send(Msg::Clean).unwrap();
        for _ in 0..100 {
            if !persistence.session_dir.join(PENDING_META).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!persistence.session_dir.join(PENDING_META).exists());
        let next = JournalMeta { dirty: false, view_dirty: true, revision: 8, project_path: Some("new.fcproj".into()), ..JournalMeta::default() };
        write_recovery_pair(&persistence.session_dir, &bytes, &next, filmcraft_format::atomic_write).unwrap();
        let matched = read_matching_meta(&persistence.session_dir).unwrap().unwrap();
        assert_eq!(matched.project_path.as_deref(), Some("new.fcproj"));
        assert!(!matched.dirty && matched.view_dirty);
    }

    static NEXT_TEST_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            let serial = NEXT_TEST_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("fc-{tag}-{}-{stamp}-{serial}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl std::ops::Deref for TestDir {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for TestDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scan_leaves_an_empty_session_without_a_stale_lock_alone() {
        let dir = TestDir::new("empty-session");
        let session = recovery_root(&dir).join("1-empty");
        fs::create_dir_all(&session).unwrap();

        assert!(scan_candidates(&dir, None).is_empty());
        assert!(session.is_dir(), "a scan must not remove a directory without proving its worker stopped");
    }

    #[test]
    fn scan_skips_clean_journals_without_deleting_their_data() {
        let dir = TestDir::new("clean-session");
        let session = recovery_root(&dir).join("1-clean");
        fs::create_dir_all(&session).unwrap();
        let snapshot = filmcraft_format::encode(&Project::default(), false);
        fs::write(session.join(SNAPSHOT), &snapshot).unwrap();
        fs::write(session.join(META), serde_json::to_vec(&JournalMeta { dirty: false, ..JournalMeta::default() }).unwrap()).unwrap();

        assert!(scan_candidates(&dir, None).is_empty());
        assert!(session.join(SNAPSHOT).is_file(), "a scan without a stale lock leaves the snapshot untouched");
        assert!(session.join(META).is_file(), "a scan without a stale lock leaves its metadata untouched");
    }

    #[test]
    fn startup_cleanup_removes_only_empty_or_clean_sessions_with_confirmed_stale_locks() {
        let dir = TestDir::new("startup-cleanup");
        let root = recovery_root(&dir);
        let clean = root.join("1-clean");
        let empty = root.join("2-empty");
        let dirty = root.join("3-dirty");
        let active = root.join("4-active");
        for session in [&clean, &empty, &dirty, &active] {
            fs::create_dir_all(session).unwrap();
            File::create(session.join(LOCK)).unwrap();
        }
        fs::write(clean.join(SNAPSHOT), b"clean").unwrap();
        fs::write(clean.join(META), serde_json::to_vec(&JournalMeta { dirty: false, ..JournalMeta::default() }).unwrap()).unwrap();
        fs::write(dirty.join(SNAPSHOT), b"dirty").unwrap();
        fs::write(dirty.join(META), serde_json::to_vec(&JournalMeta { dirty: true, ..JournalMeta::default() }).unwrap()).unwrap();
        let active_lock = File::options().read(true).write(true).open(active.join(LOCK)).unwrap();
        active_lock.try_lock().unwrap();

        cleanup_stale_sessions_at(&dir, std::time::SystemTime::now() + Duration::from_secs(301));

        assert!(!clean.exists(), "confirmed stale clean journals are cleaned at startup");
        assert!(!empty.exists(), "confirmed stale empty sessions are cleaned at startup");
        assert!(dirty.exists(), "dirty recovery data remains available");
        assert!(active.exists(), "a live worker's directory is never cleaned");
        drop(active_lock);
        cleanup_stale_sessions_at(&dir, std::time::SystemTime::now() + Duration::from_secs(302));
        assert!(!active.exists(), "after its lock is released, the empty stale directory can be cleaned");
    }

    #[test]
    fn interrupted_replacement_clears_ambiguous_destination() {
        let dir = TestDir::new("pending-destination");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let old = filmcraft_format::encode(&Project::new("A"), false);
        let first = JournalMeta { dirty: true, project_path: Some("a.fcproj".into()), ..JournalMeta::default() };
        write_recovery_pair(&session, &old, &first, filmcraft_format::atomic_write).unwrap();
        let next = filmcraft_format::encode(&Project::new("B"), false);
        let second = JournalMeta { dirty: true, project_path: Some("b.fcproj".into()), ..JournalMeta::default() };
        let mut meta_writes = 0;
        let result = write_recovery_pair(&session, &next, &second, |path, bytes| {
            if path.ends_with(META) {
                meta_writes += 1;
                if meta_writes == 2 {
                    return Err(io::Error::other("interrupted before final metadata"));
                }
            }
            filmcraft_format::atomic_write(path, bytes)
        });
        assert!(result.is_err());
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(load_candidate(&candidates[0]).unwrap().project.name, "B");
        assert_eq!(candidates[0].meta.project_path.as_deref(), Some("b.fcproj"));
    }

    #[test]
    fn interrupted_replacement_promotes_legacy_metadata_to_the_new_generation() {
        let dir = TestDir::new("legacy-pending-destination");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let old = filmcraft_format::encode(&Project::new("A"), false);
        fs::write(session.join(SNAPSHOT), &old).unwrap();
        fs::write(
            session.join(META),
            serde_json::to_vec(&JournalMeta { dirty: true, revision: 1, project_path: Some("a.fcproj".into()), ..JournalMeta::default() }).unwrap(),
        )
        .unwrap();
        assert!(read_matching_meta(&session).unwrap().unwrap().snapshot_fingerprint.is_none(), "fixture starts with fingerprint-free legacy metadata");

        let next = filmcraft_format::encode(&Project::new("B"), false);
        let second = JournalMeta { dirty: true, revision: 2, project_path: Some("b.fcproj".into()), ..JournalMeta::default() };
        let mut meta_writes = 0;
        let result = write_recovery_pair(&session, &next, &second, |path, bytes| {
            if path.ends_with(META) {
                meta_writes += 1;
                if meta_writes == 2 {
                    return Err(io::Error::other("interrupted before final metadata"));
                }
            }
            filmcraft_format::atomic_write(path, bytes)
        });

        assert!(result.is_err());
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(load_candidate(&candidates[0]).unwrap().project.name, "B");
        assert_eq!(candidates[0].meta.revision, 2);
        assert_eq!(candidates[0].meta.project_path.as_deref(), Some("b.fcproj"));
    }

    #[test]
    fn consecutive_interrupted_replacements_keep_the_last_durable_snapshot() {
        let dir = TestDir::new("two-interrupted-replacements");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let a = filmcraft_format::encode(&Project::new("A"), false);
        write_recovery_pair(&session, &a, &JournalMeta { dirty: true, ..JournalMeta::default() }, filmcraft_format::atomic_write).unwrap();
        let b = filmcraft_format::encode(&Project::new("B"), false);
        let mut snapshot_written = false;
        assert!(
            write_recovery_pair(&session, &b, &JournalMeta { dirty: true, revision: 2, ..JournalMeta::default() }, |path, bytes| {
                if path.ends_with(META) && snapshot_written {
                    Err(io::Error::other("final metadata write failed"))
                } else {
                    filmcraft_format::atomic_write(path, bytes)?;
                    snapshot_written |= path.ends_with(SNAPSHOT);
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(read_matching_meta(&session).unwrap().unwrap().revision, 2, "B's pending record should match its snapshot");
        let c = filmcraft_format::encode(&Project::new("C"), false);
        assert!(
            write_recovery_pair(&session, &c, &JournalMeta { dirty: true, revision: 3, ..JournalMeta::default() }, |path, bytes| {
                if path.ends_with(SNAPSHOT) {
                    Err(io::Error::other("stop before next snapshot replacement"))
                } else {
                    filmcraft_format::atomic_write(path, bytes)
                }
            })
            .is_err()
        );
        assert_eq!(read_matching_meta(&session).unwrap().unwrap().revision, 2, "C's interruption must leave B's record authoritative");
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(load_candidate(&candidates[0]).unwrap().project.name, "B");
    }

    #[test]
    fn clean_recovery_is_verified_against_the_current_project_file() {
        let dir = TestDir::new("recovery-current-content");
        let path = dir.join("project.fcproj");
        let current = Project::new("B");
        fs::write(&path, filmcraft_format::encode(&current, false)).unwrap();
        let candidate = RecoveryCandidate {
            id: "candidate".into(),
            dir: dir.to_path_buf(),
            meta: JournalMeta { project_path: Some(path.to_string_lossy().into_owned()), ..JournalMeta::default() },
        };
        assert!(!candidate_content_matches_project(&candidate, &Project::new("A")));
        assert!(candidate_content_matches_project(&candidate, &current));
    }

    #[test]
    fn interrupted_view_only_to_content_replacement_keeps_content_dirty_and_drops_old_overlay() {
        let dir = TestDir::new("view-to-content");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let old = filmcraft_format::encode_with_view(&Project::new("Old"), Some(&filmcraft_project::ProjectView::default()), false);
        let old_meta = JournalMeta {
            dirty: false,
            view_dirty: true,
            project_path: Some("project.fcproj".into()),
            view_snapshot_epoch: Some(7),
            view_fingerprint: Some(snapshot_fingerprint(b"old overlay")),
            ..JournalMeta::default()
        };
        write_recovery_pair(&session, &old, &old_meta, filmcraft_format::atomic_write).unwrap();
        fs::write(session.join("view-7.json"), b"old overlay").unwrap();

        let next_project = Project::new("New unsaved content");
        let next = filmcraft_format::encode_with_view(&next_project, Some(&filmcraft_project::ProjectView::default()), false);
        let result = write_recovery_pair(
            &session,
            &next,
            &JournalMeta { dirty: true, view_dirty: false, revision: 2, project_path: Some("project.fcproj".into()), ..JournalMeta::default() },
            |path, bytes| {
                filmcraft_format::atomic_write(path, bytes)?;
                if path.ends_with(SNAPSHOT) { Err(io::Error::other("interrupted after snapshot replacement")) } else { Ok(()) }
            },
        );
        assert!(result.is_err());
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].meta.dirty, "the new content remains recoverable as unsaved");
        assert!(!candidates[0].meta.view_dirty, "the matching pending record describes the new snapshot generation");
        assert!(candidates[0].meta.view_snapshot_epoch.is_none());
        assert!(candidates[0].meta.view_fingerprint.is_none());
        assert_eq!(candidates[0].meta.project_path.as_deref(), Some("project.fcproj"));
        assert_eq!(load_candidate(&candidates[0]).unwrap().project, next_project);
    }

    #[test]
    fn committed_full_snapshot_removes_its_obsolete_view_sidecar() {
        let dir = TestDir::new("obsolete-view-sidecar");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let old_snapshot = filmcraft_format::encode_with_view(&Project::new("Old"), Some(&filmcraft_project::ProjectView::default()), false);
        let overlay = serde_json::to_vec(&filmcraft_project::ProjectView::default()).unwrap();
        let old_meta = JournalMeta {
            dirty: false,
            view_dirty: true,
            view_snapshot_epoch: Some(7),
            view_fingerprint: Some(snapshot_fingerprint(&overlay)),
            ..JournalMeta::default()
        };
        write_recovery_pair(&session, &old_snapshot, &old_meta, filmcraft_format::atomic_write).unwrap();
        fs::write(session.join("view-7.json"), overlay).unwrap();

        let new_snapshot = filmcraft_format::encode_with_view(&Project::new("New"), Some(&filmcraft_project::ProjectView::default()), false);
        write_recovery_pair(&session, &new_snapshot, &JournalMeta { dirty: true, revision: 2, ..JournalMeta::default() }, filmcraft_format::atomic_write)
            .unwrap();

        assert!(!session.join("view-7.json").exists(), "the old overlay is removed after the new snapshot commits");
    }

    #[test]
    fn committed_clean_to_dirty_snapshot_removes_its_obsolete_view_sidecar() {
        let dir = TestDir::new("obsolete-clean-view-sidecar");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let old_snapshot = filmcraft_format::encode_with_view(&Project::new("Old"), Some(&filmcraft_project::ProjectView::default()), false);
        let overlay = serde_json::to_vec(&filmcraft_project::ProjectView::default()).unwrap();
        fs::write(session.join(SNAPSHOT), &old_snapshot).unwrap();
        fs::write(session.join("view-7.json"), &overlay).unwrap();
        let old_meta = JournalMeta {
            revision: 1,
            snapshot_fingerprint: Some(snapshot_fingerprint(&old_snapshot)),
            view_snapshot_epoch: Some(7),
            view_fingerprint: Some(snapshot_fingerprint(&overlay)),
            ..JournalMeta::default()
        };
        fs::write(session.join(META), serde_json::to_vec(&old_meta).unwrap()).unwrap();

        let new_snapshot = filmcraft_format::encode_with_view(&Project::new("New"), Some(&filmcraft_project::ProjectView::default()), false);
        write_recovery_pair(&session, &new_snapshot, &JournalMeta { dirty: true, revision: 2, ..JournalMeta::default() }, filmcraft_format::atomic_write)
            .unwrap();

        assert!(!session.join("view-7.json").exists(), "the obsolete sidecar is removed after the clean-to-dirty snapshot commits");
    }

    #[test]
    fn interrupted_dirty_snapshot_is_not_discarded_as_clean() {
        let dir = TestDir::new("journal-order");
        let session = recovery_root(&dir).join("1-1");
        fs::create_dir_all(&session).unwrap();
        let old_snapshot = filmcraft_format::encode(&Project::default(), false);
        write_recovery_pair(&session, &old_snapshot, &JournalMeta { dirty: false, ..JournalMeta::default() }, filmcraft_format::atomic_write).unwrap();
        let unsaved = Project::new("Unsaved snapshot after interruption");
        let snapshot = filmcraft_format::encode(&unsaved, false);
        let result = write_recovery_pair(&session, &snapshot, &JournalMeta { dirty: true, ..JournalMeta::default() }, |path, bytes| {
            filmcraft_format::atomic_write(path, bytes)?;
            if path.ends_with(SNAPSHOT) { Err(io::Error::other("interrupted")) } else { Ok(()) }
        });
        assert!(result.is_err());
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].meta.dirty, "the surviving journal remains dirty");
        assert_eq!(load_candidate(&candidates[0]).unwrap().project, unsaved, "the candidate is the interrupted snapshot");
    }

    #[test]
    fn clean_snapshot_survives_interruption_before_its_metadata_write() {
        let dir = TestDir::new("journal-clean-metadata");
        let session = recovery_root(&dir).join("1-clean");
        fs::create_dir_all(&session).unwrap();
        let project = Project::new("Clean recovery snapshot");
        let snapshot = filmcraft_format::encode(&project, false);
        let result = write_recovery_pair(&session, &snapshot, &JournalMeta::default(), |path, bytes| {
            if path.ends_with(META) {
                return Err(io::Error::other("interrupted before metadata write"));
            }
            filmcraft_format::atomic_write(path, bytes)
        });

        assert!(result.is_err());
        assert_eq!(fs::read(session.join(SNAPSHOT)).unwrap(), snapshot, "the completed snapshot remains intact");
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1, "missing metadata does not hide a durable snapshot");
        assert_eq!(load_candidate(&candidates[0]).unwrap().project, project);
    }

    #[test]
    fn failed_clean_to_dirty_pair_does_not_offer_a_stale_snapshot() {
        let dir = TestDir::new("journal-stale-pair");
        let session = recovery_root(&dir).join("1-2");
        fs::create_dir_all(&session).unwrap();
        let old_snapshot = filmcraft_format::encode(&Project::default(), false);
        write_recovery_pair(&session, &old_snapshot, &JournalMeta { dirty: false, ..JournalMeta::default() }, filmcraft_format::atomic_write).unwrap();
        let new_snapshot = filmcraft_format::encode(&Project::new("Unsaved"), false);
        let result = write_recovery_pair(&session, &new_snapshot, &JournalMeta { dirty: true, ..JournalMeta::default() }, |path, bytes| {
            if path.ends_with(SNAPSHOT) {
                return Err(io::Error::other("interrupted before replacing snapshot"));
            }
            filmcraft_format::atomic_write(path, bytes)
        });
        assert!(result.is_err());
        assert_eq!(fs::read(session.join(SNAPSHOT)).unwrap(), old_snapshot, "the previous snapshot remains untouched");
        assert!(scan_candidates(&dir, None).is_empty(), "metadata for a different snapshot must not advertise stale content as recovery");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dirty_to_dirty_write_interrupted_after_metadata_keeps_previous_snapshot_recoverable() {
        let dir = TestDir::new("journal-dirty-metadata");
        let session = recovery_root(&dir).join("1-3");
        fs::create_dir_all(&session).unwrap();
        let previous_project = Project::new("Previous unsaved work");
        let previous_snapshot = filmcraft_format::encode(&previous_project, false);
        let previous_meta = JournalMeta { dirty: true, revision: 1, project_name: "Previous unsaved work".into(), ..JournalMeta::default() };
        write_recovery_pair(&session, &previous_snapshot, &previous_meta, filmcraft_format::atomic_write).unwrap();

        let next_snapshot = filmcraft_format::encode(&Project::new("Next unsaved work"), false);
        let next_meta = JournalMeta { dirty: true, revision: 2, project_name: "Next unsaved work".into(), ..JournalMeta::default() };
        let result = write_recovery_pair(&session, &next_snapshot, &next_meta, |path, bytes| {
            if path.ends_with(PENDING_META) {
                filmcraft_format::atomic_write(path, bytes)?;
                return Err(io::Error::other("interrupted after pending metadata write"));
            }
            filmcraft_format::atomic_write(path, bytes)
        });

        assert!(result.is_err());
        assert_eq!(fs::read(session.join(SNAPSHOT)).unwrap(), previous_snapshot);
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(load_candidate(&candidates[0]).unwrap().project, previous_project);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interrupted_dirty_write_keeps_previous_snapshot_before_replacement() {
        let dir = TestDir::new("journal-missing-meta");
        let session = recovery_root(&dir).join("1-4");
        fs::create_dir_all(&session).unwrap();
        let previous_project = Project::new("Previous recoverable work");
        let previous_snapshot = filmcraft_format::encode(&previous_project, false);
        fs::write(session.join(SNAPSHOT), &previous_snapshot).unwrap();
        let previous_meta =
            JournalMeta { dirty: true, revision: 1, snapshot_fingerprint: Some(snapshot_fingerprint(&previous_snapshot)), ..JournalMeta::default() };
        fs::write(session.join(META), serde_json::to_vec(&previous_meta).unwrap()).unwrap();

        let next_snapshot = filmcraft_format::encode(&Project::new("Next unsaved work"), false);
        let result = write_recovery_pair(&session, &next_snapshot, &JournalMeta { dirty: true, revision: 2, ..JournalMeta::default() }, |path, bytes| {
            if path.ends_with(SNAPSHOT) {
                return Err(io::Error::other("interrupted before snapshot replacement"));
            }
            filmcraft_format::atomic_write(path, bytes)
        });

        assert!(result.is_err());
        assert_eq!(fs::read(session.join(SNAPSHOT)).unwrap(), previous_snapshot);
        let candidates = scan_candidates(&dir, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(load_candidate(&candidates[0]).unwrap().project, previous_project);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recovery_keeps_original_when_worker_has_stopped() {
        let d = TestDir::new("recover-worker-stopped");
        let old_dir = recovery_root(&d).join("1-1");
        fs::create_dir_all(&old_dir).unwrap();
        let mut view = filmcraft_project::ProjectView::default();
        view.timeline_ui.inverted_controls = true;
        let bytes = filmcraft_format::encode_with_view(&Project::default(), Some(&view), false);
        let original = old_dir.join(SNAPSHOT);
        fs::write(&original, &bytes).unwrap();
        let mut session = crate::Session::default();
        session.start_autosave(AutosaveConfig::new(d.0.clone())).unwrap();
        session.persistence.as_mut().unwrap().stop_worker();
        let result = session.execute("file.recover", json!({"id": "1-1"}));
        assert!(session.state.timeline_ui.inverted_controls, "recovery restores the journaled Timeline settings before reporting the stopped worker");
        assert!(original.is_file(), "a stopped worker must not consume the original snapshot");
        assert_eq!(fs::read(&original).unwrap(), bytes);
        assert_eq!(session.recovery_candidates().len(), 1);
        assert!(result.unwrap_err().to_string().contains("worker stopped"));
        session.shutdown();
    }
}

#[cfg(test)]
mod worker_failure_tests {
    use super::*;
    use crate::{Event, Session};

    fn session(tag: &str, offset: fn(i64) -> i32) -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!("fc-worker-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut prefs = Preferences::default();
        prefs.auto_save.recovery_journal = false;
        prefs.save(&dir.join("preferences.json")).unwrap();
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let mut cfg = AutosaveConfig::new(&dir);
        cfg.local_offset = offset;
        s.start_autosave(cfg).unwrap();
        s.execute("sequence.addEdit", json!({"seconds": 0.5})).unwrap();
        (s, dir)
    }

    fn failure_toasts(s: &mut Session) -> usize {
        s.drain_events().into_iter().filter(|e| matches!(e, Event::Toast { error: true, .. })).count()
    }

    fn verify_report_and_foreground_save(s: &mut Session, dir: &Path) {
        s.poll_persistence();
        assert_eq!(s.execute("file.autoSaveStatus", json!({})).unwrap()["running"], false);
        let message = s.persistence.as_ref().unwrap().status.last_error.as_deref().unwrap();
        assert!(message.contains("stopped") && message.contains("Save"), "{message}");
        assert_eq!(failure_toasts(s), 1, "worker failure must be visible exactly once");
        let log: Vec<_> = s.log.entries.iter().filter(|e| e.source == "autosave").collect();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].count, 1);
        let before = s.revision;
        s.execute("sequence.addEdit", json!({"seconds": 1.0})).unwrap();
        assert!(s.revision > before && s.is_dirty(), "the edit was applied despite autosave failure");
        s.execute("prefs.set", json!({"key": "autoSave.maxVersions", "value": 7})).unwrap();
        s.poll_persistence();
        s.poll_persistence();
        assert_eq!(failure_toasts(s), 0, "later handoffs must not repeat the failure");
        let path = dir.join("manual.fcproj");
        s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
        assert!(!s.is_dirty());
        let loaded = filmcraft_format::decode(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded.project, *s.project);
        s.poll_persistence();
        assert_eq!(failure_toasts(s), 0);
    }

    #[test]
    fn worker_panic_is_contained_and_reported() {
        fn broken_clock(_: i64) -> i32 {
            panic!("synthetic worker clock failure")
        }
        let (mut s, dir) = session("panic", broken_clock);
        let error = s.execute("file.autoSaveNow", json!({})).unwrap_err().to_string();
        let start = Instant::now();
        while !s.persistence.as_ref().unwrap().thread.as_ref().unwrap().is_finished() {
            assert!(start.elapsed() < Duration::from_secs(10), "worker did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        let joined = s.persistence.as_mut().unwrap().thread.take().unwrap().join();
        assert!(joined.is_ok(), "panic escaped the autosave worker boundary");
        assert!(error.contains("stopped") && !error.contains("timed out"), "{error}");
        verify_report_and_foreground_save(&mut s, &dir);
        s.shutdown();
        fs::remove_dir_all(dir).unwrap();
    }

    fn disconnected_handoff(tag: &str) {
        let (mut s, dir) = session(tag, |_| 0);
        s.persistence.as_mut().unwrap().stop_worker();
        match tag {
            "dirty" => {
                s.execute("sequence.addEdit", json!({"seconds": 2.0})).unwrap();
            }
            "clean" => {
                s.execute("file.saveAs", json!({"path": dir.join("initial.fcproj").to_string_lossy()})).unwrap();
            }
            "prefs" => {
                s.execute("prefs.set", json!({"key": "autoSave.maxVersions", "value": 3})).unwrap();
            }
            "flush" => {
                assert!(s.persistence.as_ref().unwrap().flush().unwrap_err().contains("worker stopped"));
            }
            "request" => {
                assert!(s.execute("file.autoSaveNow", json!({})).unwrap_err().to_string().contains("stopped"));
            }
            _ => panic!("unknown test handoff"),
        }
        verify_report_and_foreground_save(&mut s, &dir);
        s.shutdown();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worker_disconnect_dirty_reports_once() {
        disconnected_handoff("dirty");
    }
    #[test]
    fn worker_disconnect_clean_reports_once() {
        disconnected_handoff("clean");
    }
    #[test]
    fn worker_disconnect_preferences_reports_once() {
        disconnected_handoff("prefs");
    }
    #[test]
    fn worker_disconnect_flush_reports_once() {
        disconnected_handoff("flush");
    }
    #[test]
    fn worker_disconnect_flush_reply_reports_once() {
        let (mut s, dir) = session("flush-reply", |_| 0);
        s.persistence.as_mut().unwrap().stop_worker();
        let (tx, rx) = channel();
        s.persistence.as_mut().unwrap().tx = tx;
        let worker = std::thread::spawn(move || match rx.recv().unwrap() {
            Msg::Flush(reply) => drop(reply),
            _ => panic!("expected a flush request"),
        });
        let error = s.persistence.as_ref().unwrap().flush().unwrap_err();
        worker.join().unwrap();
        assert!(error.contains("worker stopped"), "{error}");
        verify_report_and_foreground_save(&mut s, &dir);
        s.shutdown();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worker_disconnect_request_reports_once() {
        disconnected_handoff("request");
    }

    #[test]
    fn worker_remains_available_after_an_ordinary_write_error() {
        let (mut s, dir) = session("write-error", |_| 0);
        let blocked = dir.join(names::AUTO_SAVE_DIR);
        fs::write(&blocked, b"occupied directory path").unwrap();
        assert!(s.execute("file.autoSaveNow", json!({})).is_err());
        s.poll_persistence();
        assert_eq!(s.execute("file.autoSaveStatus", json!({})).unwrap()["running"], true);
        assert!(s.persistence.as_ref().unwrap().status.last_error.as_deref().unwrap().contains("auto-save to"));
        assert_eq!(failure_toasts(&mut s), 1);
        fs::remove_file(&blocked).unwrap();
        let saved = s.execute("file.autoSaveNow", json!({})).unwrap();
        assert!(Path::new(saved["path"].as_str().unwrap()).is_file());
        assert!(s.is_dirty(), "auto-save should not mark the foreground project saved");
        s.shutdown();
        fs::remove_dir_all(dir).unwrap();
    }
}
