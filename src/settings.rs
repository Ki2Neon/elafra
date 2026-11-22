use std::{
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Serialize, Serializer, de::DeserializeOwned, ser::SerializeSeq};
use serde_json::Value;
use tempfile::NamedTempFile;

const VERSION: u64 = 1;
const RECENT_LIMIT: usize = 10;
pub const MAX_COUNTDOWN_SECONDS: u32 = 99 * 3600 + 59 * 60 + 59;
pub const DEFAULT_AUDIENCE_CACHE_MIB: u64 = 128;
pub const MAX_AUDIENCE_CACHE_MIB: u64 = (usize::MAX / (1024 * 1024)) as u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    Light,
    Dark,
}

impl ThemePreference {
    pub fn gpui_mode(self) -> gpui_kit::component::theme::ThemeMode {
        use gpui_kit::component::theme::ThemeMode;

        match self {
            Self::Light => ThemeMode::Light,
            Self::Dark => ThemeMode::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerMode {
    #[default]
    CountUp,
    CountDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default)]
pub struct TimerSettings {
    pub mode: TimerMode,
    pub countdown_seconds: u32,
}

impl Default for TimerSettings {
    fn default() -> Self {
        Self {
            mode: TimerMode::CountUp,
            countdown_seconds: 10 * 60,
        }
    }
}

impl TimerSettings {
    pub(crate) fn normalized(mut self) -> Self {
        self.countdown_seconds = self.countdown_seconds.clamp(1, MAX_COUNTDOWN_SECONDS);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThumbnailSize {
    Small,
    #[default]
    Medium,
    Large,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlideListMode {
    #[default]
    Thumbnails,
    PageNumbers,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlideListLayout {
    #[default]
    Vertical,
    Horizontal,
    Grid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default)]
pub struct SlideListSettings {
    pub size: ThumbnailSize,
    pub mode: SlideListMode,
    pub layout: SlideListLayout,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlideFit {
    #[default]
    WholePage,
    FitWidth,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default)]
pub struct CurrentSlideSettings {
    pub fit: SlideFit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dock: Option<Value>,
    pub timer: TimerSettings,
    pub slides: SlideListSettings,
    pub current_slide: CurrentSlideSettings,
    pub theme: ThemePreference,
    pub audience_cache_mib: u64,
    #[serde(serialize_with = "serialize_recent")]
    pub recent: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dock: None,
            timer: TimerSettings::default(),
            slides: SlideListSettings::default(),
            current_slide: CurrentSlideSettings::default(),
            theme: ThemePreference::default(),
            audience_cache_mib: DEFAULT_AUDIENCE_CACHE_MIB,
            recent: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self, SettingsError> {
        Self::load_from(&settings_path()?)
    }

    pub fn save(&self) -> Result<(), SettingsError> {
        self.save_to(&settings_path()?)
    }

    pub fn record_recent(&mut self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent.retain(|entry| entry != &path);
        self.recent.insert(0, path);
        self.recent.truncate(RECENT_LIMIT);
    }

    fn load_from(path: &Path) -> Result<Self, SettingsError> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(SettingsError::Read(error)),
        };
        let value: Value = serde_json::from_reader(BufReader::new(file))?;
        let version = value["version"]
            .as_u64()
            .ok_or(SettingsError::InvalidFormat)?;
        if version != VERSION {
            return Err(SettingsError::UnsupportedVersion(version));
        }

        Ok(Self {
            dock: value.get("dock").filter(|dock| !dock.is_null()).cloned(),
            timer: load_section::<TimerSettings>(&value, "timer").normalized(),
            slides: load_section(&value, "slides"),
            current_slide: load_section(&value, "current_slide"),
            theme: load_section(&value, "theme"),
            audience_cache_mib: value["audience_cache_mib"]
                .as_u64()
                .filter(|capacity| *capacity <= MAX_AUDIENCE_CACHE_MIB)
                .unwrap_or(DEFAULT_AUDIENCE_CACHE_MIB),
            recent: load_recent(&value["recent"]),
        })
    }

    fn save_to(&self, path: &Path) -> Result<(), SettingsError> {
        #[derive(Serialize)]
        struct SettingsFile<'a> {
            version: u64,
            #[serde(flatten)]
            settings: &'a Settings,
        }

        let bytes = serde_json::to_vec_pretty(&SettingsFile {
            version: VERSION,
            settings: self,
        })?;
        let directory = path.parent().ok_or(SettingsError::NoDirectory)?;
        fs::create_dir_all(directory).map_err(SettingsError::Write)?;
        let mut temporary = NamedTempFile::new_in(directory).map_err(SettingsError::Write)?;
        temporary.write_all(&bytes).map_err(SettingsError::Write)?;
        temporary.write_all(b"\n").map_err(SettingsError::Write)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(SettingsError::Write)?;
        temporary
            .persist(path)
            .map_err(|error| SettingsError::Write(error.error))?;
        Ok(())
    }
}

fn settings_path() -> Result<PathBuf, SettingsError> {
    #[cfg(debug_assertions)]
    if let Some(directory) = std::env::var_os("ELAFRA_CONFIG_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(directory).join("settings.json"));
    }
    ProjectDirs::from("", "", "Elafra")
        .map(|project| project.config_local_dir().join("settings.json"))
        .ok_or(SettingsError::NoDirectory)
}

fn load_section<T: Default + DeserializeOwned>(value: &Value, field: &str) -> T {
    value
        .get(field)
        .and_then(|section| T::deserialize(section).ok())
        .unwrap_or_default()
}

fn load_recent(value: &Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(entries) = value.as_array() {
        for entry in entries {
            let path = if let Some(path) = entry.as_str() {
                PathBuf::from(path)
            } else if let Ok(path) = serde_json::from_value::<OsString>(entry.clone()) {
                PathBuf::from(path)
            } else {
                continue;
            };
            if path.is_absolute() && !paths.contains(&path) {
                paths.push(path);
                if paths.len() == RECENT_LIMIT {
                    break;
                }
            }
        }
    }
    paths
}

fn serialize_recent<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
    let mut sequence = serializer.serialize_seq(Some(paths.len()))?;
    for path in paths {
        if let Some(path) = path.to_str() {
            sequence.serialize_element(path)?;
        } else {
            sequence.serialize_element(path.as_os_str())?;
        }
    }
    sequence.end()
}

#[derive(Debug)]
pub enum SettingsError {
    NoDirectory,
    Read(io::Error),
    Write(io::Error),
    Json(serde_json::Error),
    InvalidFormat,
    UnsupportedVersion(u64),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDirectory => write!(formatter, "ユーザー設定フォルダーを取得できない"),
            Self::Read(error) => write!(formatter, "設定を読み込めない（{error}）"),
            Self::Write(error) => write!(formatter, "設定を保存できない（{error}）"),
            Self::Json(error) => write!(formatter, "設定のJSON形式を処理できない（{error}）"),
            Self::InvalidFormat => write!(formatter, "設定形式のバージョンが見つからない"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "設定形式のバージョン {version} に対応していない")
            }
        }
    }
}

impl Error for SettingsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for SettingsError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restores_settings_and_replaces_previous_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config").join("settings.json");
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());

        let mut settings = Settings {
            dock: Some(json!({"opaque_layout": [1, 2, 3]})),
            timer: TimerSettings {
                mode: TimerMode::CountDown,
                countdown_seconds: 90,
            },
            slides: SlideListSettings {
                size: ThumbnailSize::Large,
                mode: SlideListMode::PageNumbers,
                layout: SlideListLayout::Horizontal,
            },
            current_slide: CurrentSlideSettings {
                fit: SlideFit::FitWidth,
            },
            theme: ThemePreference::Dark,
            audience_cache_mib: 0,
            ..Settings::default()
        };
        settings.record_recent(&directory.path().join("資料.pdf"));
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);

        settings.dock = None;
        settings.slides.layout = SlideListLayout::Grid;
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn legacy_slide_settings_default_to_vertical_without_resetting_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let legacy = json!({
            "version": VERSION,
            "slides": {"size": "small", "mode": "page_numbers"},
        });
        fs::write(&path, legacy.to_string()).unwrap();

        assert_eq!(
            Settings::load_from(&path).unwrap().slides,
            SlideListSettings {
                size: ThumbnailSize::Small,
                mode: SlideListMode::PageNumbers,
                layout: SlideListLayout::Vertical,
            }
        );
    }

    #[test]
    fn invalid_section_preserves_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let settings_path = directory.path().join("settings.json");
        let recent_path = directory.path().join("資料.pdf");
        let value = json!({
            "version": VERSION,
            "dock": "invalid dock",
            "timer": {"mode": "invalid mode"},
            "slides": {"size": "large"},
            "recent": [false, recent_path, recent_path],
        });
        fs::write(&settings_path, value.to_string()).unwrap();
        let settings = Settings::load_from(&settings_path).unwrap();

        assert_eq!(settings.dock, Some(json!("invalid dock")));
        assert_eq!(settings.timer, TimerSettings::default());
        assert_eq!(settings.slides.size, ThumbnailSize::Large);
        assert_eq!(settings.recent, [recent_path]);
        assert_eq!(settings.theme, ThemePreference::Light);
        assert_eq!(settings.audience_cache_mib, 128);
    }

    #[test]
    fn invalid_preferences_fall_back_independently() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");

        fs::write(
            &path,
            json!({"version": VERSION, "theme": "dark", "audience_cache_mib": u64::MAX})
                .to_string(),
        )
        .unwrap();
        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.theme, ThemePreference::Dark);
        assert_eq!(settings.audience_cache_mib, DEFAULT_AUDIENCE_CACHE_MIB);

        fs::write(
            &path,
            json!({"version": VERSION, "theme": "invalid", "audience_cache_mib": 256}).to_string(),
        )
        .unwrap();
        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.theme, ThemePreference::Light);
        assert_eq!(settings.audience_cache_mib, 256);
    }

    #[test]
    fn recents_move_to_front_and_retain_ten() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        for index in 0..=RECENT_LIMIT {
            settings.record_recent(&directory.path().join(format!("{index}.pdf")));
        }
        let reopened = directory.path().join("2.pdf");
        settings.record_recent(&reopened);

        assert_eq!(settings.recent.len(), RECENT_LIMIT);
        assert_eq!(settings.recent[0], reopened);
        assert_eq!(
            settings
                .recent
                .iter()
                .filter(|path| *path == &reopened)
                .count(),
            1
        );
        assert!(!settings.recent.contains(&directory.path().join("0.pdf")));
    }
}
