// Nine-slot pocket library (port of PocketStore.swift, Windows-adjusted):
// files are plain local paths (Windows has no security-scoped bookmarks),
// images and rich text are durable assets under keyboard-pocket/assets/, and
// every mutation pushes into a bounded undo history persisted atomically.
// Assets are deliberately retained for undo and never auto-deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::keys::SLOT_COUNT;

pub const MAXIMUM_HISTORY: usize = 20;
pub const MAXIMUM_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const MAXIMUM_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAXIMUM_FILES: usize = 256;
const MAXIMUM_STATE_BYTES: u64 = 64 * 1024 * 1024;
const IMAGE_EXTENSIONS: [&str; 10] = ["png", "jpg", "jpeg", "tif", "tiff", "gif", "webp", "bmp", "ico", "heic"];

pub fn is_image_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PocketItem {
    pub id: String,
    pub kind: String, // text | richText | link | image | files
    pub title: String,
    pub text: Option<String>,
    pub files: Vec<String>, // absolute local paths (files kind)
    #[serde(rename = "imagePath")]
    pub image_path: Option<String>,
    #[serde(rename = "richTextPath")]
    pub rich_text_path: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String, // ISO-8601 UTC
    pub pinned: bool,
}

impl Default for PocketItem {
    fn default() -> Self {
        PocketItem {
            id: String::new(),
            kind: "text".into(),
            title: String::new(),
            text: None,
            files: Vec::new(),
            image_path: None,
            rich_text_path: None,
            created_at: String::new(),
            pinned: false,
        }
    }
}

impl PocketItem {
    pub fn created_unix(&self) -> i64 {
        // Accepts "2026-11-15T08:09:10Z" (our writer) and tolerates garbage.
        parse_iso8601(&self.created_at).unwrap_or(0)
    }

    /// UI-visible title follows the Mac generatedTitle rules: fresh captures
    /// from clipboard/image show stable bilingual labels; user titles stay.
    pub fn display_title(&self) -> String {
        match self.kind.as_str() {
            "image" if self.title == "图片" => "图片 / Image".into(),
            "files" if self.title.ends_with("个文件") => format!(
                "{} 个文件 / {} files",
                self.files.len(),
                self.files.len()
            ),
            "text" | "richText" if self.title == "文字" => "文字 / Text".into(),
            _ => self.title.clone(),
        }
    }
}

pub fn parse_iso8601(value: &str) -> Option<i64> {
    // Fixed format produced by iso_now(): 2026-11-15T08:09:10Z
    let bytes = value.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[19] != b'Z' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days-to-seconds via the same civil arithmetic (reversed).
    let z = civil_to_days(year, month, day);
    Some(z * 86_400 + hour * 3600 + minute * 60 + second)
}

fn civil_to_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedState {
    version: i32,
    slots: Vec<Option<PocketItem>>,
    history: Vec<Vec<Option<PocketItem>>>,
}

pub struct PocketStore {
    root: PathBuf,
    assets: PathBuf,
    pub slots: Vec<Option<PocketItem>>,
    history: Vec<Vec<Option<PocketItem>>>,
}

impl PocketStore {
    pub fn open(root: &Path) -> Result<PocketStore, String> {
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
        let mut store = PocketStore { root: root.to_path_buf(), assets, slots: vec![None; SLOT_COUNT], history: Vec::new() };
        let state_path = root.join("pockets.json");
        if !state_path.exists() {
            return Ok(store);
        }
        let meta = std::fs::metadata(&state_path).map_err(|e| e.to_string())?;
        if meta.len() > MAXIMUM_STATE_BYTES {
            return Err("保存文件过大，未载入。 / The saved file is too large and was not loaded.".into());
        }
        let text = std::fs::read_to_string(&state_path).map_err(|e| format!("口袋数据格式不正确。 / {e}"))?;
        let state: PersistedState = serde_json::from_str(&text)
            .map_err(|_| "口袋数据格式不正确，原文件已保留。 / Pocket data could not be read.".to_string())?;
        if state.version != 1
            || state.slots.len() != SLOT_COUNT
            || state.history.len() > MAXIMUM_HISTORY
            || state.history.iter().any(|snapshot| snapshot.len() != SLOT_COUNT)
        {
            return Err("口袋数据格式不正确，原文件已保留。 / Pocket data could not be read.".into());
        }
        for item in state.slots.iter().flatten().chain(state.history.iter().flatten().flatten()) {
            store.validate_item(item)?;
        }
        store.slots = state.slots;
        store.history = state.history;
        Ok(store)
    }

    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn put(&mut self, item: PocketItem, index: usize) -> Result<(), String> {
        self.validate_item(&item)?;
        self.check(index)?;
        self.commit(next_slot_state(&self.slots, index, Some(item)))
    }

    pub fn clear(&mut self, index: usize) -> Result<(), String> {
        self.check(index)?;
        if self.slots[index].is_none() {
            return Err("这个槽位还是空的。 / This slot is empty.".into());
        }
        self.commit(next_slot_state(&self.slots, index, None))
    }

    pub fn clear_all(&mut self) -> Result<(), String> {
        if self.slots.iter().all(|slot| slot.is_none()) {
            return Ok(());
        }
        self.commit(vec![None; SLOT_COUNT])
    }

    /// Removes slots whose content is older than `retain_days` (0 = keep all).
    /// Goes through the normal commit path so the cleanup stays undoable.
    pub fn enforce_retention(&mut self, retain_days: i64) -> Result<bool, String> {
        if retain_days <= 0 {
            return Ok(false);
        }
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            - retain_days * 86_400;
        let expired: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.as_ref().is_some_and(|item| item.created_unix() > 0 && item.created_unix() < cutoff))
            .map(|(index, _)| index)
            .collect();
        if expired.is_empty() {
            return Ok(false);
        }
        let mut next = self.slots.clone();
        for index in expired {
            next[index] = None;
        }
        self.commit(next)?;
        Ok(true)
    }

    pub fn undo(&mut self) -> Result<(), String> {
        let Some(previous) = self.history.pop() else {
            return Err("没有可以撤销的操作。 / There is nothing to undo.".into());
        };
        let slots = previous;
        let next_history = self.history.clone();
        let state = PersistedState { version: 1, slots, history: next_history };
        self.persist(&state)?;
        self.slots = state.slots;
        self.history = state.history;
        Ok(())
    }

    fn commit(&mut self, next: Vec<Option<PocketItem>>) -> Result<(), String> {
        let mut next_history = Vec::with_capacity(self.history.len() + 1);
        next_history.extend(self.history.iter().cloned());
        next_history.push(self.slots.clone());
        while next_history.len() > MAXIMUM_HISTORY {
            next_history.remove(0);
        }
        let state = PersistedState { version: 1, slots: next, history: next_history };
        self.persist(&state)?;
        self.slots = state.slots;
        self.history = state.history;
        Ok(())
    }

    /// Atomic write: temp file in the same directory, then rename over the
    /// target so a crash can never leave a truncated pockets.json behind.
    fn persist(&self, state: &PersistedState) -> Result<(), String> {
        let body = serde_json::to_vec(state).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
        let target = self.root.join("pockets.json");
        let temp = self.root.join(format!(".pockets-{}.tmp", std::process::id()));
        std::fs::write(&temp, &body).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
        // windows rename semantics: remove the target first (std::fs::rename
        // fails on Windows when the destination exists).
        if target.exists() {
            let backup = self.root.join(".pockets.bak");
            std::fs::rename(&target, &backup).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
            if std::fs::rename(&temp, &target).is_err() {
                let _ = std::fs::rename(&backup, &target);
                return Err("保存失败，原有内容未改变：rename failed".into());
            }
            let _ = std::fs::remove_file(&backup);
        } else if let Err(e) = std::fs::rename(&temp, &target) {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("保存失败，原有内容未改变：{e}"));
        }
        Ok(())
    }

    /// Persists image bytes (already PNG-encoded) as an asset and returns the
    /// stored path. Caller is responsible for size validation.
    pub fn persist_image(&self, data: &[u8]) -> Result<String, String> {
        if data.len() > MAXIMUM_IMAGE_BYTES {
            return Err("图片不能超过 32 MB。 / Images cannot exceed 32 MB.".into());
        }
        self.persist_asset(data, "png")
    }

    pub fn persist_rich_text(&self, data: &[u8]) -> Result<String, String> {
        if data.len() > MAXIMUM_TEXT_BYTES {
            return Err("富文本不能超过 16 MB。 / Rich text cannot exceed 16 MB.".into());
        }
        self.persist_asset(data, "html")
    }

    fn persist_asset(&self, data: &[u8], extension: &str) -> Result<String, String> {
        let name = format!("{}.{}", uuid_v4(), extension);
        let target = self.assets.join(&name);
        let temp = self.assets.join(format!(".import-{name}"));
        std::fs::write(&temp, data).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
        std::fs::rename(&temp, &target).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
        Ok(target.to_string_lossy().into_owned())
    }

    /// Builds a files-kind item from a picker/drop selection. A single image
    /// file becomes an image asset (Mac itemFromFiles parity).
    pub fn item_from_files(&self, paths: Vec<PathBuf>) -> Result<PocketItem, String> {
        if paths.is_empty() {
            return Err("剪贴板中没有可保存的文字、图片、链接或文件。 / The clipboard has no supported content to store.".into());
        }
        if paths.len() > MAXIMUM_FILES {
            return Err("每个槽位最多保存 256 个文件。 / Each slot can hold up to 256 files.".into());
        }
        let mut unique: Vec<PathBuf> = Vec::new();
        let mut seen = HashSet::new();
        for path in paths {
            let canonical = normalize_path(&path);
            if seen.insert(canonical.clone()) {
                unique.push(canonical);
            }
        }
        for path in &unique {
            if !path.exists() {
                return Err(format!("文件已移动或删除：{} / The file was moved or deleted: {}", path.display(), path.display()));
            }
        }
        if unique.len() == 1 {
            let path = &unique[0];
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if is_image_extension(&name) && path.is_file() {
                let data = std::fs::read(path).map_err(|e| e.to_string())?;
                if data.len() > MAXIMUM_IMAGE_BYTES {
                    return Err("图片不能超过 32 MB。 / Images cannot exceed 32 MB.".into());
                }
                let stored = self.persist_image(&data)?;
                return Ok(PocketItem {
                    id: uuid_v4(),
                    kind: "image".into(),
                    title: name.chars().take(120).collect(),
                    image_path: Some(stored),
                    created_at: iso_now(),
                    ..PocketItem::default()
                });
            }
        }
        let title = if unique.len() == 1 {
            unique[0]
                .file_name()
                .map(|n| n.to_string_lossy().chars().take(120).collect())
                .unwrap_or_else(|| "文件 / Files".into())
        } else {
            format!("{} 个文件 / {} files", unique.len(), unique.len())
        };
        Ok(PocketItem {
            id: uuid_v4(),
            kind: "files".into(),
            title,
            files: unique.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            created_at: iso_now(),
            ..PocketItem::default()
        })
    }

    pub fn validate_item(&self, item: &PocketItem) -> Result<(), String> {
        let zh_en = |zh: &str, en: &str| format!("{zh} / {en}");
        if item.id.is_empty() || item.title.is_empty() || item.title.chars().count() > 120 {
            return Err(zh_en("口袋内容格式不正确。", "The pocket content has an invalid format."));
        }
        if !matches!(item.kind.as_str(), "text" | "richText" | "image" | "link" | "files") {
            return Err(zh_en("口袋内容格式不正确。", "The pocket content has an invalid format."));
        }
        if item.files.len() > MAXIMUM_FILES {
            return Err(zh_en("每个槽位最多保存 256 个文件。", "Each slot can hold up to 256 files."));
        }
        if let Some(text) = &item.text {
            if text.len() > MAXIMUM_TEXT_BYTES {
                return Err(zh_en("文字不能超过 16 MB。", "Text cannot exceed 16 MB."));
            }
        }
        for file in &item.files {
            if !Path::new(file).is_absolute() {
                return Err(zh_en("文件引用不是有效的本机 URL。", "The file reference is not a valid local path."));
            }
        }
        for path in [item.image_path.as_deref(), item.rich_text_path.as_deref()].into_iter().flatten() {
            let asset_prefix = self.assets.canonicalize().unwrap_or_else(|_| self.assets.clone());
            let stored = Path::new(path);
            let canonical = stored.canonicalize().unwrap_or_else(|_| stored.to_path_buf());
            if !canonical.starts_with(&asset_prefix) {
                return Err(zh_en("素材路径不在当前口袋数据目录。", "The asset is outside the current pocket data folder."));
            }
        }
        match item.kind.as_str() {
            "text" | "link" if item.text.is_none() => {
                return Err(zh_en("文字内容缺失。", "The text content is missing."));
            }
            "richText" if item.text.is_none() || item.rich_text_path.is_none() => {
                return Err(zh_en("富文本内容缺失。", "The rich text content is missing."));
            }
            "image" if item.image_path.is_none() => {
                return Err(zh_en("图片内容缺失。", "The image content is missing."));
            }
            "files" if item.files.is_empty() => {
                return Err(zh_en("文件引用为空。", "The file reference is empty."));
            }
            _ => {}
        }
        Ok(())
    }

    /// Snapshot pieces consumed by the status builder. `missing` follows the
    /// same resolution the retrieval uses (existence checked per file).
    pub fn slot_missing(&self, item: &PocketItem) -> bool {
        match item.kind.as_str() {
            "files" => item.files.iter().any(|file| !Path::new(file).exists()),
            "image" => item.image_path.as_deref().is_some_and(|p| !Path::new(p).exists()),
            "richText" => item.rich_text_path.as_deref().is_some_and(|p| !Path::new(p).exists()),
            _ => false,
        }
    }

    pub fn contains_folders(&self, item: &PocketItem) -> bool {
        item.kind == "files" && item.files.iter().any(|file| Path::new(file).is_dir())
    }

    /// Validates all referenced files before the clipboard changes (Mac
    /// PocketFileTransfer.write parity): one missing file aborts the write.
    pub fn file_access(&self, item: &PocketItem) -> Result<Vec<PathBuf>, String> {
        let mut paths = Vec::with_capacity(item.files.len());
        for file in &item.files {
            let path = PathBuf::from(file);
            if !path.exists() {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return Err(format!("文件已移动或删除：{name} / The file was moved or deleted: {name}"));
            }
            paths.push(path);
        }
        Ok(paths)
    }

    /// True when the stored item has any folder reference (drives ZIP).
    pub fn needs_archive(item: &PocketItem) -> bool {
        item.kind == "files" && item.files.iter().any(|file| Path::new(file).is_dir())
    }

    /// Values snapshot for the status builder (slots only).
    pub fn slots_value(&self) -> Vec<Value> {
        self.slots
            .iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                None => serde_json::json!({ "index": index, "empty": true }),
                Some(item) => {
                    let missing = self.slot_missing(item);
                    let preview = item.text.as_deref().unwrap_or(&item.title).chars().take(240).collect::<String>();
                    serde_json::json!({
                        "index": index,
                        "empty": false,
                        "id": item.id,
                        "kind": item.kind,
                        "title": item.display_title(),
                        "text": item.text.clone().unwrap_or_default(),
                        "files": item.files,
                        "createdAt": item.created_at,
                        "pinned": item.pinned,
                        "missing": missing,
                        "containsFolders": self.contains_folders(item),
                        "preview": preview,
                        "count": if item.kind == "files" { item.files.len() } else { 1 },
                        "fileCount": if item.kind == "files" { item.files.len() } else { 1 },
                    })
                }
            })
            .collect()
    }
}

fn next_slot_state(slots: &[Option<PocketItem>], index: usize, value: Option<PocketItem>) -> Vec<Option<PocketItem>> {
    let mut next = slots.to_vec();
    next[index] = value;
    next
}

fn check_index(index: usize) -> Result<(), String> {
    if index >= SLOT_COUNT {
        Err("槽位不存在。 / This slot does not exist.".into())
    } else {
        Ok(())
    }
}

impl PocketStore {
    fn check(&self, index: usize) -> Result<(), String> {
        check_index(index)
    }
}

pub fn normalize_path(path: &Path) -> PathBuf {
    // Windows canonicalization without network round-trips: \\?\ prefixes are
    // dropped again so stored paths stay user-readable.
    path.canonicalize()
        .map(|canonical| {
            let text = canonical.to_string_lossy();
            let stripped = text.strip_prefix(r"\\?\UNC\").map(|rest| format!(r"\\{rest}")).or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned));
            PathBuf::from(stripped.unwrap_or_else(|| text.into_owned()))
        })
        .unwrap_or_else(|_| path.to_path_buf())
}

pub fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    format_iso(secs)
}

pub fn format_iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// RFC 4122 v4 random UUID from the OS RNG (no external crates).
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if getrandom(&mut bytes).is_err() {
        // Fall back to a time/pid mix; uniqueness within this process's
        // lifetime is the requirement, not cryptographic strength.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let mut state = nanos ^ (pid << 64);
        for byte in bytes.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *byte = (state >> 64) as u8;
        }
    }
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn getrandom(buffer: &mut [u8]) -> Result<(), ()> {
    use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    // windows-sys 0.59 exposes BCryptGenRandom under Win32_Security_Cryptography.
    let ok = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if ok == 0 {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{name}-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text_item(id: &str, text: &str) -> PocketItem {
        PocketItem {
            id: id.into(),
            kind: "text".into(),
            title: "标题".into(),
            text: Some(text.into()),
            created_at: iso_now(),
            ..PocketItem::default()
        }
    }

    #[test]
    fn put_clear_and_undo_round_trip() {
        let root = temp_root("pocket-store-a");
        let mut store = PocketStore::open(&root).unwrap();
        assert!(!store.can_undo());
        store.put(text_item("a", "hello"), 0).unwrap();
        store.put(text_item("b", "second"), 0).unwrap();
        assert!(store.slots[0].is_some());
        assert_eq!(store.slots[0].as_ref().unwrap().id, "b");
        assert!(store.can_undo());
        store.undo().unwrap();
        assert_eq!(store.slots[0].as_ref().unwrap().id, "a");
        store.undo().unwrap();
        assert!(store.slots[0].is_none());
        assert!(!store.can_undo());
        assert!(store.undo().is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn atomic_write_survives_reload_with_history() {
        let root = temp_root("pocket-store-b");
        let mut store = PocketStore::open(&root).unwrap();
        store.put(text_item("a", "one"), 2).unwrap();
        store.put(text_item("b", "two"), 4).unwrap();
        // A reload must restore slots AND undo history exactly.
        let reloaded = PocketStore::open(&root).unwrap();
        assert_eq!(reloaded.slots[2].as_ref().unwrap().id, "a");
        assert_eq!(reloaded.slots[4].as_ref().unwrap().id, "b");
        assert!(reloaded.can_undo());
        // No temp leftovers: the atomic rename cleaned them up.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".pockets-"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn history_is_bounded() {
        let root = temp_root("pocket-store-c");
        let mut store = PocketStore::open(&root).unwrap();
        for round in 0..30 {
            store.put(text_item(&format!("i{round}"), "x"), 0).unwrap();
        }
        let mut count = 0;
        while store.can_undo() {
            store.undo().unwrap();
            count += 1;
        }
        assert_eq!(count, MAXIMUM_HISTORY);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_state_file_reports_error() {
        let root = temp_root("pocket-store-d");
        std::fs::write(root.join("pockets.json"), "not json at all").unwrap();
        assert!(PocketStore::open(&root).is_err());
        // Wrong version.
        std::fs::write(root.join("pockets.json"), r#"{"version":2,"slots":[],"history":[]}"#).unwrap();
        assert!(PocketStore::open(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retention_clears_expired_slots_only() {
        let root = temp_root("pocket-store-e");
        let mut store = PocketStore::open(&root).unwrap();
        let old = PocketItem {
            id: "old".into(),
            kind: "text".into(),
            title: "旧".into(),
            text: Some("x".into()),
            created_at: format_iso(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    - 10 * 86_400,
            ),
            ..PocketItem::default()
        };
        store.put(old, 1).unwrap();
        store.put(text_item("fresh", "y"), 2).unwrap();
        assert!(store.enforce_retention(7).unwrap());
        assert!(store.slots[1].is_none());
        assert!(store.slots[2].is_some());
        assert!(store.enforce_retention(0).unwrap() == false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn item_validation_rejects_foreign_asset_paths() {
        let root = temp_root("pocket-store-f");
        let store = PocketStore::open(&root).unwrap();
        let mut item = text_item("x", "t");
        item.kind = "image".into();
        item.image_path = Some(std::env::temp_dir().join("elsewhere.png").to_string_lossy().into_owned());
        item.text = None;
        assert!(store.validate_item(&item).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn item_from_files_classifies_single_image() {
        let root = temp_root("pocket-store-g");
        let store = PocketStore::open(&root).unwrap();
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        let source = root.parent().unwrap().join(format!("src-img-{}.png", uuid_v4()));
        std::fs::write(&source, png_bytes).unwrap();
        let item = store.item_from_files(vec![source.clone()]).unwrap();
        assert_eq!(item.kind, "image");
        let stored = PathBuf::from(item.image_path.clone().unwrap());
        assert!(stored.exists());
        assert_eq!(std::fs::read(&stored).unwrap(), png_bytes);
        // Multiple files stay a files-kind item with deduplicated paths.
        let other = root.parent().unwrap().join(format!("src-img-{}.txt", uuid_v4()));
        std::fs::write(&other, b"t").unwrap();
        let files_item = store
            .item_from_files(vec![source.clone(), source.clone(), other.clone(), other.clone()])
            .unwrap();
        assert_eq!(files_item.kind, "files");
        assert_eq!(files_item.files.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&other);
    }

    #[test]
    fn iso_parsing_and_formatting_round_trip() {
        let now = iso_now();
        let secs = parse_iso8601(&now).unwrap();
        assert_eq!(format_iso(secs), now);
        assert_eq!(parse_iso8601("garbage"), None);
        assert_eq!(parse_iso8601("2026-13-01T00:00:00Z"), None);
        // Hinnant civil round trip for a known date (2026-11-15).
        let known = parse_iso8601("2026-11-15T00:00:00Z").unwrap();
        assert_eq!(format_iso(known), "2026-11-15T00:00:00Z");
    }

    #[test]
    fn uuid_format_is_v4() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.split('-').map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(id.chars().skip(14).next().unwrap() == '4');
    }

    #[test]
    fn missing_file_detection_matches_retrieval() {
        let root = temp_root("pocket-store-h");
        let mut store = PocketStore::open(&root).unwrap();
        let existing = root.parent().unwrap().join(format!("exists-{}.txt", uuid_v4()));
        std::fs::write(&existing, b"x").unwrap();
        let item = PocketItem {
            id: "f".into(),
            kind: "files".into(),
            title: "f".into(),
            files: vec![existing.to_string_lossy().into_owned(), "Z:\\definitely-missing.bin".into()],
            created_at: iso_now(),
            ..PocketItem::default()
        };
        assert!(store.slot_missing(&item));
        assert!(store.file_access(&item).is_err());
        let single = PocketItem { files: vec![existing.to_string_lossy().into_owned()], ..item.clone() };
        assert!(!store.slot_missing(&single));
        assert!(store.file_access(&single).is_ok());
        assert!(store.contains_folders(&item.clone()) == false);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&existing);
    }

    #[test]
    fn display_title_uses_generated_labels() {
        let mut item = text_item("t", "body");
        assert_eq!(item.display_title(), "标题");
        item.kind = "image".into();
        item.title = "图片".into();
        assert_eq!(item.display_title(), "图片 / Image");
        item.kind = "files".into();
        item.title = "3 个文件".into();
        item.files = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(item.display_title(), "3 个文件 / 3 files");
    }
}
