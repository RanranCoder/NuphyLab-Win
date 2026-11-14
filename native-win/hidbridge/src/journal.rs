// Lighting backup journals. Ported from HIDBridge.swift's LightingBackup and
// LightingJournals: Mac/Win profiles live in separate files, a legacy single
// file migrates without rewriting its bytes, and every mutation of the record
// is validated against the durable original first.
use crate::error::{failure, Result};
use crate::protocol;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct LightingBackup {
    pub created_at: String,
    pub serial: String,
    pub firmware: Vec<u8>,
    pub state: Vec<u8>,
    pub rendered_colors: Vec<u8>,
    pub custom_colors: Option<Vec<u8>>,
    pub lighting_profile: Option<u8>,
}

pub fn iso8601_now() -> String {
    // RFC 3339 UTC timestamp without pulling in a date library.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let (hh, mm, ss) = (secs.rem_euclid(86400) / 3600, secs.rem_euclid(3600) / 60, secs.rem_euclid(60));
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl LightingBackup {
    pub fn to_json(&self) -> Value {
        let mut object = json!({
            "schema": 1,
            "createdAt": self.created_at,
            "vendorID": 0x19f5,
            "productID": 0x1028,
            "serial": self.serial,
            "firmware": self.firmware,
            "state": self.state,
            "renderedColors": self.rendered_colors,
        });
        if let Some(colors) = &self.custom_colors {
            object["customColors"] = json!(colors);
        }
        if let Some(profile) = self.lighting_profile {
            object["lightingProfile"] = json!(profile);
        }
        object
    }

    pub fn from_json(value: &Value) -> Result<LightingBackup> {
        let object = value.as_object().ok_or_else(|| failure("BACKUP_READ_FAILED", "The lighting backup is not a JSON object."))?;
        if object.get("schema").and_then(|v| v.as_i64()) != Some(1)
            || object.get("vendorID").and_then(|v| v.as_i64()) != Some(0x19f5)
            || object.get("productID").and_then(|v| v.as_i64()) != Some(0x1028)
        {
            return Err(failure("INVALID_BACKUP", "The lighting backup has an invalid schema or size."));
        }
        let int_array = |key: &str| -> Result<Vec<u8>> {
            object
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|v| v.as_u64().unwrap_or(256) as u8).collect())
                .ok_or_else(|| failure("BACKUP_READ_FAILED", format!("The lighting backup is missing {key}.")))
        };
        Ok(LightingBackup {
            created_at: object.get("createdAt").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            serial: object.get("serial").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            firmware: int_array("firmware")?,
            state: int_array("state")?,
            rendered_colors: int_array("renderedColors")?,
            custom_colors: object.get("customColors").and_then(|v| v.as_array()).map(|a| a.iter().map(|v| v.as_u64().unwrap_or(256) as u8).collect()),
            lighting_profile: object.get("lightingProfile").and_then(|v| v.as_u64()).map(|v| v as u8),
        })
    }
}

pub struct LightingJournals {
    pub legacy_url: PathBuf,
}

impl LightingJournals {
    pub fn new(legacy_url: PathBuf) -> LightingJournals {
        LightingJournals { legacy_url }
    }

    pub fn url(&self, profile: u8) -> PathBuf {
        self.legacy_url.with_extension(format!("profile-{profile}.json"))
    }

    pub fn profiles(&self) -> Vec<u8> {
        [0u8, 1].into_iter().filter(|p| self.url(*p).exists()).collect()
    }

    pub fn has_legacy(&self) -> bool {
        self.legacy_url.exists()
    }

    pub fn read(&self, url: &Path, serial: &str) -> Result<LightingBackup> {
        let raw = fs::read_to_string(url)
            .map_err(|e| failure("BACKUP_READ_FAILED", format!("Cannot read the existing lighting backup: {e}")))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|e| failure("BACKUP_READ_FAILED", format!("Cannot read the existing lighting backup: {e}")))?;
        let record = LightingBackup::from_json(&value)?;
        let profile_ok = record.lighting_profile.unwrap_or(0) <= 1;
        let size_ok = record.rendered_colors.len() == 312
            && (record.custom_colors.is_none() || record.custom_colors.as_ref().map(|c| c.len()) == Some(252));
        if !profile_ok || !size_ok || !protocol::readable_state(&record.state, &record.firmware) {
            return Err(failure("INVALID_BACKUP", "The lighting backup has an invalid schema or size."));
        }
        if !record.serial.is_empty() && !serial.is_empty() && record.serial != serial {
            return Err(failure("BACKUP_DEVICE_MISMATCH", "The pending backup belongs to a different keyboard serial number."));
        }
        Ok(record)
    }

    pub fn migrate_legacy(&self, serial: &str) -> Result<()> {
        if !self.has_legacy() {
            return Ok(());
        }
        let record = self.read(&self.legacy_url, serial)?;
        let destination = self.url(record.lighting_profile.unwrap_or(0));
        // move refuses to overwrite an existing journal. On a conflict, retain
        // both originals instead of choosing one and losing recovery.
        if destination.exists() || fs::rename(&self.legacy_url, &destination).is_err() {
            return Err(failure("BACKUP_MIGRATION_FAILED", "无法整理灯光恢复记录，原始备份已保留。"));
        }
        Ok(())
    }

    pub fn archive_before_firmware_change(&self, record: &LightingBackup, serial: &str) -> Result<PathBuf> {
        let source = self.url(record.lighting_profile.unwrap_or(0));
        // Validate the durable original again; never discard or overwrite it.
        let durable = self.read(&source, serial)?;
        if durable.firmware != record.firmware {
            return Err(failure("INVALID_BACKUP", "The recovery record changed during firmware migration."));
        }
        let directory = source.parent().unwrap().join("firmware-recovery-history");
        fs::create_dir_all(&directory)
            .map_err(|e| failure("BACKUP_SAVE_FAILED", format!("Cannot create recovery history: {e}")))?;
        let destination = directory.join(format!(
            "{}-{}-{}.json",
            source.file_stem().unwrap().to_string_lossy(),
            protocol::firmware_version(&record.firmware),
            uuid_like()
        ));
        if destination.exists() || fs::rename(&source, &destination).is_err() {
            return Err(failure("BACKUP_MIGRATION_FAILED", "无法归档灯光恢复记录，原始备份已保留。"));
        }
        Ok(destination)
    }

    pub fn save_custom_colors(&self, colors: &[u8], record: &LightingBackup, serial: &str) -> Result<LightingBackup> {
        if colors.len() != 252 {
            return Err(failure("INVALID_BACKUP", "Invalid custom palette size."));
        }
        let source = self.url(record.lighting_profile.unwrap_or(0));
        let original = self.read(&source, serial)?;
        if original.created_at != record.created_at || original.firmware != record.firmware || original.state != record.state {
            return Err(failure("INVALID_BACKUP", "The recovery record changed before palette capture."));
        }
        let mut updated = original;
        updated.custom_colors = Some(colors.to_vec());
        let temporary = source.with_extension(format!("json.{}.tmp", uuid_like()));
        let result = (|| {
            let object = updated.to_json();
            let mut file = fs::File::create_new(&temporary)
                .map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
            let mut text = serde_json::to_string_pretty(&object).map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
            text.push('\n');
            file.write_all(text.as_bytes()).map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
            file.sync_all().map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
            drop(file);
            if fs::rename(&temporary, &source).is_err() {
                return Err(failure("BACKUP_SAVE_FAILED", "Cannot preserve the original custom palette; no RGB frame was sent."));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(updated)
    }
}

fn uuid_like() -> String {
    // A random-enough suffix without a UUID crate: time + address entropy.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let stack = &nanos as *const u128 as usize;
    format!("{nanos:032x}{stack:04x}")
}
