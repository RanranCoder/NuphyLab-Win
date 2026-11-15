// Unit tests for the updater: version math, feed parsing, signature and
// digest verification, the check state machine over file:// fixtures, and the
// installer command line. Network is never touched — fixtures are written to
// a temp directory and served through file:/// URLs.

use std::cmp::Ordering;
use std::path::PathBuf;

use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;

use super::crypto;
use super::feed::{compare_versions, is_os_supported, parse_appcast, parse_version, ReleaseNotes};
use super::{file_url_path, installer_command_line, perform_check, resolve_feed_url, Phase, UpdaterCore};

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(triple >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[triple as usize & 63] as char } else { '=' });
    }
    out
}

fn temp_file(name: &str, body: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("nuphylab-updater-tests-{}-{}", std::process::id(), name));
    std::fs::write(&path, body).expect("write fixture");
    path
}

fn file_url(path: &PathBuf) -> String {
    format!("file:///{}", path.display().to_string().replace('\\', "/"))
}

const GOOD_ITEM: &str = r#"<item>
  <title>9.9.9</title>
  <sparkle:version>9.9.9</sparkle:version>
  <sparkle:minimumSystemVersion>10.0</sparkle:minimumSystemVersion>
  <description sparkle:format="plain-text"><![CDATA[NUPHY-RELEASE-NOTES 1
version:9.9.9
[zh-CN]
修复若干问题。
[en]
Fixed several issues.
]]></description>
  <enclosure url="http://127.0.0.1:9/setup.exe" length="12345" type="application/octet-stream" sparkle:edSignature="AAAA" sparkle:sha256="00"/>
</item>"#;

#[test]
fn version_comparison_is_numeric_not_lexical() {
    let older = parse_version("1.2.8");
    let newer = parse_version("1.2.47");
    assert_eq!(compare_versions(&newer, &older), Ordering::Greater);
    assert_eq!(compare_versions(&older, &newer), Ordering::Less);
    assert_eq!(compare_versions(&parse_version("1.0.12"), &parse_version("v1.0.12")), Ordering::Equal);
    assert_eq!(compare_versions(&parse_version("1.2"), &parse_version("1.2.0")), Ordering::Equal);
    assert_eq!(compare_versions(&parse_version("1.2"), &parse_version("1.2.1")), Ordering::Less);
}

#[test]
fn version_parsing_tolerates_prefixes_and_suffixes() {
    assert_eq!(parse_version(" 10.0.19045 "), vec![10, 0, 19045]);
    assert_eq!(parse_version("V3.2.1-beta.2"), vec![3, 2, 1]);
    assert!(parse_version("unknown").is_empty());
}

#[test]
fn appcast_items_parse_all_fields() {
    let xml = format!(r#"<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0"><channel><title>NuPhy Lab</title>{GOOD_ITEM}</channel></rss>"#);
    let items = parse_appcast(&xml).expect("parse");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.version, "9.9.9");
    assert_eq!(item.url, "http://127.0.0.1:9/setup.exe");
    assert_eq!(item.length, 12345);
    assert_eq!(item.signature, "AAAA");
    assert_eq!(item.sha256, "00");
    assert_eq!(item.minimum_system, "10.0");
}

#[test]
fn appcast_keeps_multiple_items_in_feed_order() {
    let xml = format!(
        r#"<rss version="2.0"><channel><item><sparkle:version>2.0.0</sparkle:version><enclosure url="http://x/2" length="1"/></item>{GOOD_ITEM}</channel></rss>"#
    );
    let items = parse_appcast(&xml).expect("parse");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].version, "2.0.0");
    assert_eq!(items[1].version, "9.9.9");
}

#[test]
fn appcast_without_items_is_rejected() {
    assert!(parse_appcast("<rss><channel><title>x</title></channel></rss>").is_err());
}

#[test]
fn release_notes_parse_both_languages() {
    let notes = ReleaseNotes::parse("NUPHY-RELEASE-NOTES 1\nversion:9.9.9\n[zh-CN]\n修复若干问题。\n第二行\n[en]\nFixed several issues.\nSecond line\n");
    assert_eq!(notes.version, "9.9.9");
    assert_eq!(notes.zh, "修复若干问题。\n第二行");
    assert_eq!(notes.en, "Fixed several issues.\nSecond line");
}

#[test]
fn release_notes_tolerate_missing_sections() {
    let notes = ReleaseNotes::parse("version:1.0.0\n[zh-CN]\n只有中文。");
    assert_eq!(notes.version, "1.0.0");
    assert_eq!(notes.zh, "只有中文。");
    assert_eq!(notes.en, "");
    let empty = ReleaseNotes::parse("");
    assert_eq!(empty, ReleaseNotes::default());
}

#[test]
fn signature_verification_accepts_real_and_rejects_tampered() {
    let seed = [7u8; 32];
    let signing = SigningKey::from_bytes(&seed);
    let data = b"installer-bytes";
    let signature = signing.sign(data);
    let public = base64(signing.verifying_key().as_bytes());
    let encoded = base64(&signature.to_bytes());
    assert_eq!(crypto::verify_signature(&public, &encoded, data), Ok(()));
    // Tampered payload / signature / key must all be refused.
    assert!(crypto::verify_signature(&public, &encoded, b"installer-bytes!").is_err());
    let mut flipped = signature.to_bytes();
    flipped[0] ^= 0x40;
    assert!(crypto::verify_signature(&public, &base64(&flipped), data).is_err());
    assert!(crypto::verify_signature(&base64(&[9u8; 32]), &encoded, data).is_err());
    // Structural rejects: wrong lengths and bad base64.
    assert!(crypto::verify_signature(&public, &base64(&[0u8; 63]), data).is_err());
    assert!(crypto::verify_signature(&base64(&[0u8; 31]), &encoded, data).is_err());
    assert!(crypto::verify_signature("not-base64!!", &encoded, data).is_err());
}

#[test]
fn base64_decode_is_strict_and_padding_aware() {
    assert_eq!(crypto::decode_base64("AAAA").unwrap(), vec![0, 0, 0]);
    assert_eq!(crypto::decode_base64("/w==").unwrap(), vec![255]);
    assert_eq!(crypto::decode_base64("aGVsbG8=").unwrap(), b"hello".to_vec());
    assert_eq!(crypto::decode_base64("aGVs\nbG8=").unwrap(), b"hello".to_vec());
    assert!(crypto::decode_base64("AAA").is_err());
    assert!(crypto::decode_base64("").is_err());
    assert!(crypto::decode_base64("!!!!").is_err());
}

#[test]
fn sha256_matches_reference_vectors() {
    assert_eq!(crypto::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(crypto::sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    // Same input → same digest, different input → different digest (the
    // property the tamper check relies on).
    assert_eq!(crypto::sha256_hex(b"installer"), crypto::sha256_hex(b"installer"));
    assert_ne!(crypto::sha256_hex(b"installer"), crypto::sha256_hex(b"installer!"));
}

#[test]
fn check_against_file_feed_transitions_idle_to_available() {
    let setup_bytes = b"fake setup bytes";
    let signature = SigningKey::from_bytes(&[7u8; 32]).sign(setup_bytes);
    let xml = format!(
        r#"<rss version="2.0"><channel>{}</channel></rss>"#,
        GOOD_ITEM
            .replace("9.9.9", "99.0.1")
            .replace("sparkle:edSignature=\"AAAA\"", &format!("sparkle:edSignature=\"{}\"", base64(&signature.to_bytes())))
            .replace("sparkle:sha256=\"00\"", &format!("sparkle:sha256=\"{}\"", crypto::sha256_hex(setup_bytes)))
    );
    let path = temp_file("appcast-good.xml", &xml);
    let mut core = UpdaterCore::open(&std::env::temp_dir());
    core.feed_override = Some(file_url(&path));
    let item = perform_check(&mut core, Some(&file_url(&path))).expect("check").expect("update offered");
    assert_eq!(item.version, "99.0.1");
    assert_eq!(core.phase, Phase::Available);
    assert!(!core.up_to_date);
    assert_eq!(core.remote.as_ref().unwrap().notes.zh, "修复若干问题。");
    // Snapshot shape the UI consumes.
    let snapshot = core.snapshot();
    assert_eq!(snapshot["phase"], "available");
    assert_eq!(snapshot["currentVersion"], super::current_version());
    assert_eq!(snapshot["notes"]["en"], "Fixed several issues.");
    assert!(snapshot["progress"].is_null());
    std::fs::remove_file(&path).ok();
}

#[test]
fn check_against_older_feed_reports_up_to_date() {
    let xml = format!(r#"<rss version="2.0"><channel>{}</channel></rss>"#, GOOD_ITEM.replace("9.9.9", "0.0.1"));
    let path = temp_file("appcast-older.xml", &xml);
    let mut core = UpdaterCore::open(&std::env::temp_dir());
    let offered = perform_check(&mut core, Some(&file_url(&path))).expect("check");
    assert!(offered.is_none());
    assert!(core.up_to_date);
    assert_eq!(core.phase, Phase::Idle);
    assert!(core.remote.is_none());
    std::fs::remove_file(&path).ok();
}

#[test]
fn check_rejects_unsupported_and_disabled_feeds() {
    let mut core = UpdaterCore::open(&std::env::temp_dir());
    assert!(perform_check(&mut core, Some("ftp://example.invalid/appcast.xml")).is_err());
    // Disabled markers resolve to no feed at all.
    assert!(resolve_feed_url(Some("off")).is_none());
    assert!(resolve_feed_url(Some("disabled")).is_none());
    std::env::remove_var("AIR_STUDIO_UPDATE_FEED");
    assert_eq!(resolve_feed_url(None).as_deref(), Some(super::DEFAULT_FEED_URL));
    assert_eq!(resolve_feed_url(Some("http://localhost:9/appcast.xml")).as_deref(), Some("http://localhost:9/appcast.xml"));
    assert_eq!(core.phase, Phase::Idle);
}

#[test]
fn check_surfaces_feed_errors() {
    let missing = file_url(&PathBuf::from("Z:/definitely/not/here.xml"));
    let mut core = UpdaterCore::open(&std::env::temp_dir());
    assert!(perform_check(&mut core, Some(&missing)).is_err());
    let broken = temp_file("appcast-broken.xml", "<rss><channel><item><sparkle:version>9.9.9</sparkle:version><enclosure/></item></channel></rss>");
    assert!(perform_check(&mut core, Some(&file_url(&broken))).is_err());
    std::fs::remove_file(&broken).ok();
}

#[test]
fn os_version_gate_honours_minimum_system_version() {
    assert!(is_os_supported((10, 0, 19045), "10.0"));
    assert!(is_os_supported((10, 0, 19045), "10.0.19045"));
    assert!(!is_os_supported((10, 0, 19045), "10.1"));
    assert!(!is_os_supported((10, 0, 19045), "11"));
    // Empty/unparseable minimums never block.
    assert!(is_os_supported((10, 0, 19045), ""));
    assert!(is_os_supported((10, 0, 19045), "not-a-version"));
}

#[test]
fn feed_url_helpers_gate_schemes_and_decode_file_urls() {
    assert!(super::feed::is_supported_feed_url("https://example.com/appcast-win.xml"));
    assert!(super::feed::is_supported_feed_url("http://localhost:8123/appcast-win.xml"));
    assert!(super::feed::is_supported_feed_url("file:///D:/tmp/appcast-win.xml"));
    assert!(!super::feed::is_supported_feed_url("ftp://example.invalid/a.xml"));
    assert!(!super::feed::is_supported_feed_url("/etc/appcast.xml"));
    assert_eq!(file_url_path("file:///D:/tmp/appcast-win.xml"), Some(PathBuf::from("D:\\tmp\\appcast-win.xml")));
    assert_eq!(
        file_url_path("file://localhost/D:/tmp/appcast-win.xml?x=1"),
        Some(PathBuf::from("D:\\tmp\\appcast-win.xml"))
    );
    assert_eq!(file_url_path("https://example.com/a.xml"), None);
}

#[test]
fn installer_command_line_carries_inno_silent_flags() {
    let command = installer_command_line(&PathBuf::from("C:\\cache\\setup-1.0.13.exe"));
    assert_eq!(
        command,
        "\"C:\\cache\\setup-1.0.13.exe\" /VERYSILENT /SUPPRESSMSGBOXES /RESTARTAPPLICATIONS /CLOSEAPPLICATIONS"
    );
}

#[test]
fn phase_names_match_the_ui_contract() {
    assert_eq!(Phase::Idle.as_str(), "idle");
    assert_eq!(Phase::Checking.as_str(), "checking");
    assert_eq!(Phase::Available.as_str(), "available");
    assert_eq!(Phase::Downloading.as_str(), "downloading");
    assert_eq!(Phase::Verified.as_str(), "verified");
    assert_eq!(Phase::Installing.as_str(), "installing");
    assert_eq!(Phase::Error.as_str(), "error");
}

#[test]
fn core_resets_clear_transient_state() {
    let mut core = UpdaterCore::open(&std::env::temp_dir());
    core.phase = Phase::Error;
    core.error = Some(("失败".into(), "failed".into()));
    core.reset(Phase::Idle);
    assert_eq!(core.phase, Phase::Idle);
    assert!(core.error.is_none());
    core.reset(Phase::Downloading);
    let snapshot: Value = core.snapshot();
    assert_eq!(snapshot["phase"], "downloading");
    assert_eq!(snapshot["progress"], 0);
}
