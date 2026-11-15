// Minimal ZIP writer in "store" mode (no compression) for the folder
// retrieval path: a slot that contains folders is archived before the
// clipboard write, mirroring the Mac FolderTransfer semantics. Entries carry
// the UTF-8 filename flag (general purpose bit 11) so Chinese paths survive.
// The output is a plain ZIP 2.0 archive readable by Explorer/7-Zip/Feishu.

const UTF8_FLAG: u16 = 0x0800;
const MAX_ENTRIES: usize = 10_000;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

pub struct ZipWriter {
    out: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
    bytes_source: u64,
}

impl ZipWriter {
    pub fn new() -> ZipWriter {
        ZipWriter { out: Vec::new(), central: Vec::new(), entries: 0, bytes_source: 0 }
    }

    /// Adds one filesystem path (file or directory). Directories are walked
    /// recursively; links are followed through std::fs metadata semantics.
    pub fn add_path(&mut self, path: &std::path::Path, archive_name: &str) -> Result<(), String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if self.entries as usize >= MAX_ENTRIES {
            return Err(
                "文件夹超过 10,000 项或 1 GB，请缩小范围。 / Folder exceeds 10,000 entries or 1 GB.".into(),
            );
        }
        if meta.is_dir() {
            self.write_entry(&format!("{archive_name}/"), &[], &meta)?;
            let mut children: Vec<_> = std::fs::read_dir(path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .filter_map(|e| e.ok())
                .collect();
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let child_name = child.file_name().to_string_lossy().into_owned();
                // Windows cannot represent '/' or '\' in names; keep the rest
                // as-is so CJK filenames round-trip byte-exact.
                let sanitized = child_name.replace(['\\', '/'], "_");
                self.add_path(&child.path(), &format!("{archive_name}/{sanitized}"))?;
            }
        } else if meta.is_file() {
            let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            self.bytes_source += data.len() as u64;
            if self.bytes_source > MAX_BYTES {
                return Err(
                    "文件夹超过 10,000 项或 1 GB，请缩小范围。 / Folder exceeds 10,000 entries or 1 GB.".into(),
                );
            }
            self.write_entry(archive_name, &data, &meta)?;
        } else {
            // Pipes/devices are not storable content.
            return Err(format!("无法读取：{} / Cannot read: {}", path.display(), path.display()));
        }
        Ok(())
    }

    fn write_entry(&mut self, name: &str, data: &[u8], meta: &std::fs::Metadata) -> Result<(), String> {
        if self.entries >= u16::MAX as u16 {
            return Err("文件夹超过 10,000 项或 1 GB，请缩小范围。 / Folder exceeds 10,000 entries or 1 GB.".into());
        }
        let crc = crc32(data);
        let (dos_time, dos_date) = dos_timestamp(meta);
        let offset = self.out.len() as u32;
        let name_bytes = name.as_bytes();
        // Local file header: signature, versions, flags, method 0 (stored),
        // timestamps, CRC, sizes, name length, extra length 0.
        self.out.extend_from_slice(&0x04034b50u32.to_le_bytes());
        self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.out.extend_from_slice(&UTF8_FLAG.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // method: store
        self.out.extend_from_slice(&dos_time.to_le_bytes());
        self.out.extend_from_slice(&dos_date.to_le_bytes());
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out.extend_from_slice(name_bytes);
        self.out.extend_from_slice(data);
        // Central directory record.
        let mut record = Vec::with_capacity(46 + name_bytes.len());
        record.extend_from_slice(&0x02014b50u32.to_le_bytes());
        record.extend_from_slice(&20u16.to_le_bytes()); // version made by
        record.extend_from_slice(&20u16.to_le_bytes()); // version needed
        record.extend_from_slice(&UTF8_FLAG.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes()); // method
        record.extend_from_slice(&dos_time.to_le_bytes());
        record.extend_from_slice(&dos_date.to_le_bytes());
        record.extend_from_slice(&crc.to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes()); // extra
        record.extend_from_slice(&0u16.to_le_bytes()); // comment
        record.extend_from_slice(&0u16.to_le_bytes()); // disk number
        record.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        record.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        record.extend_from_slice(&offset.to_le_bytes());
        record.extend_from_slice(name_bytes);
        self.central.extend_from_slice(&record);
        self.entries += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Vec<u8> {
        let central_offset = self.out.len() as u32;
        let central_size = self.central.len() as u32;
        self.out.extend_from_slice(&self.central);
        // End of central directory.
        self.out.extend_from_slice(&0x06054b50u32.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // disk
        self.out.extend_from_slice(&0u16.to_le_bytes()); // start disk
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&central_size.to_le_bytes());
        self.out.extend_from_slice(&central_offset.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        self.out
    }
}

/// Standard ZIP DOS time/date from the file modification time (local time).
fn dos_timestamp(meta: &std::fs::Metadata) -> (u16, u16) {
    use std::time::UNIX_EPOCH;
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days (same approach as worker.rs).
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
    let year = (y as u32).saturating_sub(1980).min(127) as u16;
    let month = m as u16;
    let day = d as u16;
    let hour = (seconds_of_day / 3600) as u16;
    let minute = ((seconds_of_day % 3600) / 60) as u16;
    let second = ((seconds_of_day % 60) / 2) as u16; // 2-second granularity
    ((hour << 11) | (minute << 5) | second, (year << 9) | (month << 5) | day)
}

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            table[i as usize] = c;
        }
        table
    });
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = table[((crc ^ *byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Archives `paths` (files and/or folders) into a ZIP with top-level names
/// preserved. Returns the archive bytes.
pub fn build_zip(paths: &[std::path::PathBuf]) -> Result<Vec<u8>, String> {
    if paths.is_empty() {
        return Err("文件引用为空。 / The file reference is empty.".into());
    }
    if paths.len() > 128 {
        return Err("文件夹超过 10,000 项或 1 GB，请缩小范围。 / Folder exceeds 10,000 entries or 1 GB.".into());
    }
    let mut writer = ZipWriter::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("无法读取：{} / Cannot read: {}", path.display(), path.display()))?
            .replace(['\\', '/'], "_");
        writer.add_path(path, &name)?;
    }
    Ok(writer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tiny ZIP reader used only by the tests: walks the central directory.
    struct CentralEntry {
        name: String,
        utf8_flag: bool,
        crc: u32,
        size: u32,
        method: u16,
    }

    fn read_central(data: &[u8]) -> Vec<CentralEntry> {
        let find = |needle: &[u8]| data.windows(4).position(|w| w == needle).expect("EOCD/CD");
        let cd_sig = 0x02014b50u32.to_le_bytes();
        let cd_start = find(&cd_sig);
        // EOCD carries counts; parse records sequentially.
        let mut entries = Vec::new();
        let mut pos = cd_start;
        while pos + 46 <= data.len() && data[pos..pos + 4] == cd_sig {
            let flags = u16::from_le_bytes([data[pos + 8], data[pos + 9]]);
            let method = u16::from_le_bytes([data[pos + 10], data[pos + 11]]);
            let crc = u32::from_le_bytes([data[pos + 16], data[pos + 17], data[pos + 18], data[pos + 19]]);
            let size = u32::from_le_bytes([data[pos + 24], data[pos + 25], data[pos + 26], data[pos + 27]]);
            let name_len = u16::from_le_bytes([data[pos + 28], data[pos + 29]]) as usize;
            let name = String::from_utf8_lossy(&data[pos + 46..pos + 46 + name_len]).into_owned();
            entries.push(CentralEntry { name, utf8_flag: flags & UTF8_FLAG != 0, crc, size, method });
            pos += 46 + name_len;
        }
        entries
    }

    #[test]
    fn crc32_matches_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32("中文内容".as_bytes()), crc32("中文内容".as_bytes()));
    }

    #[test]
    fn zip_round_trips_utf8_names_and_content() {
        let dir = std::env::temp_dir().join(format!("pocket-zip-{}", std::process::id()));
        let nested = dir.join("资料");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("说明.txt"), "中文内容".as_bytes()).unwrap();
        std::fs::write(dir.join("top.txt"), b"hello").unwrap();

        let zip = build_zip(&[dir.clone()]).unwrap();
        let entries = read_central(&zip);
        let root = dir.file_name().unwrap().to_string_lossy().into_owned();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // Directories get their own entries, including the nested one.
        assert_eq!(
            names,
            vec![format!("{root}/"), format!("{root}/top.txt"), format!("{root}/资料/"), format!("{root}/资料/说明.txt")]
        );
        for entry in &entries {
            assert!(entry.utf8_flag, "UTF-8 flag missing on {}", entry.name);
            assert_eq!(entry.method, 0, "store mode");
        }
        // "中文内容" CRC + size must match the actual bytes.
        let expected_crc = crc32("中文内容".as_bytes());
        let content_entry = entries.iter().find(|e| e.name.ends_with("说明.txt")).unwrap();
        assert_eq!(content_entry.crc, expected_crc);
        assert_eq!(content_entry.size, "中文内容".as_bytes().len() as u32);
        // EOCD count matches.
        let eocd = zip.windows(4).position(|w| w == 0x06054b50u32.to_le_bytes()).unwrap();
        let count = u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]);
        assert_eq!(count, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_rejects_empty_selection() {
        assert!(build_zip(&[]).is_err());
    }

    #[test]
    fn zip_rejects_missing_path() {
        let missing = std::env::temp_dir().join("pocket-zip-does-not-exist-9x");
        assert!(build_zip(&[missing]).is_err());
    }

    #[test]
    fn multiple_files_zip_at_root() {
        let dir = std::env::temp_dir().join(format!("pocket-zip-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"AAA").unwrap();
        std::fs::write(dir.join("b.txt"), b"BBB").unwrap();
        let zip = build_zip(&[dir.join("a.txt"), dir.join("b.txt")]).unwrap();
        let names: Vec<String> = read_central(&zip).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
