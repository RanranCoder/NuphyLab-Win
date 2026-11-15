// Drag Convert persistence (settings + the last 20 batches): plain JSON files
// under AIR_STUDIO_DATA/drag-convert/, written atomically with a backup
// rename so a crash can never truncate the previous state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAXIMUM_HISTORY: usize = 20;
const MAXIMUM_HISTORY_BYTES: u64 = 2_000_000;

fn issue(zh: &str, en: &str) -> String {
    format!("{zh} / {en}")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DragPreferences {
    pub enabled: bool,
    pub trigger: String,
    pub quality: f64,
    #[serde(rename = "maxDimension")]
    pub max_dimension: u32,
    #[serde(rename = "outputDirectory")]
    pub output_directory: Option<String>,
}

impl Default for DragPreferences {
    fn default() -> Self {
        DragPreferences {
            enabled: false,
            trigger: "shift".into(),
            quality: 0.88,
            max_dimension: 1600,
            output_directory: None,
        }
    }
}

impl DragPreferences {
    /// DragConvertPreferences.validated parity: known trigger, quality in
    /// [0.5, 1], the four offered edge sizes and a plain output folder.
    pub fn validate(&self) -> Result<(), String> {
        if !super::trigger::is_valid_trigger(&self.trigger) {
            return Err(issue("设置无效：触发键不受支持。", "Invalid settings: unsupported trigger key."));
        }
        if !self.quality.is_finite() || !(0.5..=1.0).contains(&self.quality) {
            return Err(issue("设置无效：画质超出范围。", "Invalid settings: quality out of range."));
        }
        if !matches!(self.max_dimension, 800 | 1200 | 1600 | 2400) {
            return Err(issue("设置无效：长边尺寸不受支持。", "Invalid settings: unsupported edge size."));
        }
        if let Some(directory) = &self.output_directory {
            if directory.is_empty() || directory.contains('\0') || !Path::new(directory).is_absolute() {
                return Err(issue("设置无效：输出文件夹不可用。", "Invalid settings: unsupported output folder."));
            }
        }
        Ok(())
    }

    /// Applies a UI patch; output locations only ever come from the native
    /// folder picker, so the current value always wins.
    pub fn apply_patch(&mut self, patch: &Value) {
        if let Some(enabled) = patch.get("enabled").and_then(Value::as_bool) {
            self.enabled = enabled;
        }
        if let Some(trigger) = patch.get("trigger").and_then(Value::as_str) {
            self.trigger = trigger.to_string();
        }
        if let Some(quality) = patch.get("quality").and_then(Value::as_f64) {
            self.quality = quality;
        }
        if let Some(max_dimension) = patch.get("maxDimension").and_then(Value::as_u64) {
            if let Ok(max_dimension) = u32::try_from(max_dimension) {
                self.max_dimension = max_dimension;
            }
        }
    }

    pub fn preferences_value(&self) -> Value {
        serde_json::json!({
            "enabled": self.enabled,
            "trigger": self.trigger,
            "quality": self.quality,
            "maxDimension": self.max_dimension,
            "outputDirectory": self.output_directory,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileResult {
    pub input: String,
    pub output: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Batch {
    pub id: String,
    pub format: String,
    pub time: f64,
    pub succeeded: usize,
    pub failed: usize,
    #[serde(rename = "inputCount", skip_serializing_if = "Option::is_none")]
    pub input_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    pub files: Vec<FileResult>,
}

impl Batch {
    pub fn new(format: &str, input_count: usize) -> Batch {
        Batch {
            id: crate::pocket::store::uuid_v4(),
            format: format.to_string(),
            time: unix_now(),
            succeeded: 0,
            failed: 0,
            input_count: Some(input_count),
            cancelled: None,
            files: Vec::new(),
        }
    }

    /// Bounds the history to the newest MAXIMUM_HISTORY batches.
    pub fn push_history(history: &mut Vec<Batch>, batch: Batch) {
        history.insert(0, batch);
        history.truncate(MAXIMUM_HISTORY);
    }
}

pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedHistory {
    version: i32,
    history: Vec<Batch>,
    outputs: BTreeMap<String, Vec<String>>,
}

pub struct DragStore {
    root: PathBuf,
}

impl DragStore {
    pub fn open(data_dir: &Path) -> DragStore {
        let root = data_dir.join("drag-convert");
        let _ = std::fs::create_dir_all(&root);
        DragStore { root }
    }

    pub fn load_settings(&self) -> Result<DragPreferences, String> {
        let path = self.root.join("settings.json");
        if !path.exists() {
            return Ok(DragPreferences::default());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let mut preferences: DragPreferences =
            serde_json::from_str(&text).map_err(|_| issue("无法读取转换设置，已暂停。重新保存设置即可恢复。", "Settings could not be read."))?;
        if preferences.output_directory.is_some() && preferences.output_directory.as_deref().unwrap_or("").is_empty() {
            preferences.output_directory = None;
        }
        Ok(preferences)
    }

    pub fn save_settings(&self, settings: &DragPreferences) -> Result<(), String> {
        let body = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
        self.atomic_write("settings.json", &body)
    }

    pub fn load_history(&self) -> (Vec<Batch>, BTreeMap<String, Vec<PathBuf>>) {
        let path = self.root.join("history.json");
        if !path.exists() {
            return (Vec::new(), BTreeMap::new());
        }
        let Ok(meta) = std::fs::metadata(&path) else { return (Vec::new(), BTreeMap::new()) };
        if meta.len() > MAXIMUM_HISTORY_BYTES {
            return (Vec::new(), BTreeMap::new());
        }
        let Ok(text) = std::fs::read_to_string(&path) else { return (Vec::new(), BTreeMap::new()) };
        let Ok(state) = serde_json::from_str::<PersistedHistory>(&text) else {
            return (Vec::new(), BTreeMap::new());
        };
        let history: Vec<Batch> = state.history.into_iter().take(MAXIMUM_HISTORY).collect();
        let outputs: BTreeMap<String, Vec<PathBuf>> = state
            .outputs
            .into_iter()
            .filter(|(id, _)| history.iter().any(|batch| &batch.id == id))
            .map(|(id, paths)| (id, paths.into_iter().map(PathBuf::from).collect()))
            .collect();
        (history, outputs)
    }

    pub fn save_history(&self, history: &[Batch], outputs: &BTreeMap<String, Vec<PathBuf>>) -> Result<(), String> {
        let state = PersistedHistory {
            version: 1,
            history: history.to_vec(),
            outputs: outputs
                .iter()
                .map(|(id, paths)| (id.clone(), paths.iter().map(|path| path.to_string_lossy().into_owned()).collect()))
                .collect(),
        };
        let body = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
        self.atomic_write("history.json", &body)
    }

    /// Atomic write: temp file in the same directory, backup-renamed swap so
    /// a crash leaves either the old or the new content, never a truncated
    /// file (pocket/store.rs pattern).
    fn atomic_write(&self, name: &str, body: &[u8]) -> Result<(), String> {
        let target = self.root.join(name);
        let temp = self.root.join(format!(".{name}-{}.tmp", std::process::id()));
        std::fs::write(&temp, body).map_err(|e| issue("保存失败，原有内容未改变", &format!("Could not save: {e}")))?;
        if target.exists() {
            let backup = self.root.join(format!(".{name}.bak"));
            if std::fs::rename(&target, &backup).is_err() {
                let _ = std::fs::remove_file(&temp);
                return Err(issue("保存失败，原有内容未改变", "Could not save: rename failed"));
            }
            if std::fs::rename(&temp, &target).is_err() {
                let _ = std::fs::rename(&backup, &target);
                let _ = std::fs::remove_file(&temp);
                return Err(issue("保存失败，原有内容未改变", "Could not save: rename failed"));
            }
            let _ = std::fs::remove_file(&backup);
        } else if let Err(error) = std::fs::rename(&temp, &target) {
            let _ = std::fs::remove_file(&temp);
            return Err(issue("保存失败，原有内容未改变", &format!("Could not save: {error}")));
        }
        Ok(())
    }
}
