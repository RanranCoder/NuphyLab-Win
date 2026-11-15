// Appcast feed model and parsers (Sparkle RSS dialect, Windows variant).
// The feed is the single trust root of the updater: the item carries the
// version, the download URL, the byte length, the Ed25519 signature over the
// installer bytes and a SHA-256 digest, plus the bilingual release notes.
//
// Parsing is intentionally hand-rolled: the feed shape is fixed (we produce
// it ourselves with scripts/prepare-appcast-win.mjs) and the XML surface used
// is tiny — nested tags inside <item>, attributes on <enclosure> and one
// CDATA description. No XML dependency is pulled in for that.

use std::cmp::Ordering;

/// One `<item>` entry of the appcast (the newest one is what the updater acts on).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateItem {
    pub version: String,
    pub url: String,
    pub length: u64,
    /// Base64 Ed25519 signature over the installer file bytes.
    pub signature: String,
    /// Lowercase hex SHA-256 of the installer file bytes.
    pub sha256: String,
    /// Sparkle-style minimum OS version ("10.0" → (10, 0, 0)).
    pub minimum_system: String,
    pub notes: ReleaseNotes,
}

/// Bilingual release notes parsed from the CDATA description. Format
/// (mirrors the Mac feed, scripts/prepare-appcast.mjs):
///   NUPHY-RELEASE-NOTES 1
///   version:1.2.47
///   [zh-CN]
///   …
///   [en]
///   …
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReleaseNotes {
    pub version: String,
    pub zh: String,
    pub en: String,
}

impl ReleaseNotes {
    pub fn parse(body: &str) -> ReleaseNotes {
        let mut notes = ReleaseNotes::default();
        let mut section: Option<&'static str> = None;
        let mut zh = String::new();
        let mut en = String::new();
        for line in body.lines() {
            let trimmed = line.trim();
            match trimmed {
                "[zh-CN]" => {
                    section = Some("zh");
                    continue;
                }
                "[en]" => {
                    section = Some("en");
                    continue;
                }
                _ => {}
            }
            if let Some(rest) = trimmed.strip_prefix("version:") {
                if section.is_none() {
                    notes.version = rest.trim().to_string();
                }
                continue;
            }
            if trimmed.starts_with("NUPHY-RELEASE-NOTES") {
                continue;
            }
            match section {
                Some("zh") => {
                    zh.push_str(line);
                    zh.push('\n');
                }
                Some("en") => {
                    en.push_str(line);
                    en.push('\n');
                }
                _ => {}
            }
        }
        notes.zh = zh.trim().to_string();
        notes.en = en.trim().to_string();
        notes
    }
}

/// "1.2.47" / "v1.2.47" / "1.2.47-beta" → [1, 2, 47]. Pre-release or build
/// metadata ("-beta", "+1") ends the numeric part; segments without any digit
/// are ignored.
pub fn parse_version(text: &str) -> Vec<u64> {
    let cleaned = text.trim().trim_start_matches(|c| c == 'v' || c == 'V');
    let numeric = cleaned.split(['-', '+']).next().unwrap_or(cleaned);
    numeric
        .split('.')
        .filter_map(|segment| {
            let digits: String = segment.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                None
            } else {
                digits.parse::<u64>().ok()
            }
        })
        .collect()
}

/// Numeric semantic comparison: 1.2.47 > 1.2.8, missing segments count as 0
/// (1.2 == 1.2.0 < 1.2.1).
pub fn compare_versions(a: &[u64], b: &[u64]) -> Ordering {
    let width = a.len().max(b.len());
    for index in 0..width {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// True when the running OS satisfies the feed's minimumSystemVersion.
pub fn is_os_supported(current: (u64, u64, u64), minimum: &str) -> bool {
    if minimum.trim().is_empty() {
        return true;
    }
    let required = parse_version(minimum);
    if required.is_empty() {
        return true;
    }
    let running = vec![current.0, current.1, current.2];
    compare_versions(&running, &required) != Ordering::Less
}

fn xml_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        rest = &rest[position..];
        let semicolon = rest.find(';');
        let Some(semicolon) = semicolon else { break };
        let entity = &rest[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => None,
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &rest[semicolon + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn tag_value(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = item.find(&open)?;
    let after_open = &item[start..];
    let close = after_open.find('>')?;
    // Skip self-closing tags and make sure the ">" found belongs to the open
    // tag (attribute values may contain ">").
    if after_open[..close].ends_with('/') {
        return None;
    }
    let body = &after_open[close + 1..];
    let end_tag = format!("</{tag}>");
    let end = body.find(&end_tag)?;
    Some(body[..end].trim().to_string())
}

/// Full element text including the tag itself, for attribute extraction from
/// self-closing tags (`<enclosure … />`).
fn element_raw(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = item.find(&open)?;
    let after_open = &item[start..];
    let close = after_open.find('>')?;
    if after_open[..close].ends_with('/') {
        return Some(after_open[..=close].to_string());
    }
    let end_tag = format!("</{tag}>");
    let end = after_open.find(&end_tag)?;
    Some(after_open[..end + end_tag.len()].to_string())
}

fn cdata_value(item: &str, tag: &str) -> Option<String> {
    let raw = tag_value(item, tag)?;
    let open = raw.find("<![CDATA[")?;
    let body = &raw[open + 9..];
    let end = body.find("]]>")?;
    Some(body[..end].to_string())
}

fn attr_value(element: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let start = element.find(&needle)?;
    let quote = element.as_bytes().get(start + needle.len()).copied()?;
    let quote = quote as char;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = &element[start + needle.len() + 1..];
    let end = body.find(quote)?;
    Some(body[..end].trim().to_string())
}

/// Parses the RSS appcast and returns the items in feed order (newest first).
pub fn parse_appcast(xml: &str) -> Result<Vec<UpdateItem>, String> {
    let mut items = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<item>") {
        let body = &rest[start + 6..];
        let Some(end) = body.find("</item>") else { break };
        let item = &body[..end];
        rest = &body[end + 7..];
        let version = tag_value(item, "sparkle:version")
            .or_else(|| tag_value(item, "title"))
            .unwrap_or_default();
        if version.is_empty() {
            continue;
        }
        let enclosure = element_raw(item, "enclosure").unwrap_or_default();
        let url = xml_unescape(&attr_value(&enclosure, "url").unwrap_or_default());
        let signature = attr_value(&enclosure, "sparkle:edSignature").unwrap_or_default();
        let sha256 = attr_value(&enclosure, "sparkle:sha256").unwrap_or_default();
        let length = attr_value(&enclosure, "length")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let minimum_system = tag_value(item, "sparkle:minimumSystemVersion").unwrap_or_default();
        let notes_body = cdata_value(item, "description").unwrap_or_default();
        if url.is_empty() {
            return Err(format!("feed item {version} has no download url"));
        }
        items.push(UpdateItem {
            version,
            url,
            length,
            signature,
            sha256,
            minimum_system,
            notes: ReleaseNotes::parse(&notes_body),
        });
    }
    if items.is_empty() {
        return Err("feed has no items".to_string());
    }
    Ok(items)
}

/// Feed endpoints the updater accepts: http(s) for the production feed and
/// localhost file testing, plus file:/// URLs for offline QA runs.
pub fn is_supported_feed_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("file:///")
        || lower.starts_with("file://localhost/")
}
