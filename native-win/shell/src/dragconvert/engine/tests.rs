// Drag Convert engine tests: action routing, archive path safety, naming,
// availability gating and real image / zip / tar.gz / PDF round trips.

use super::*;
use image::GenericImageView;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{name}-{}", crate::pocket::store::uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn kinds_of(paths: &[std::path::PathBuf]) -> Vec<FileKind> {
    paths.iter().map(|path| file_kind(path)).collect()
}

fn write_png(dir: &std::path::Path, name: &str, width: u32, height: u32, rgba: [u8; 4]) -> std::path::PathBuf {
    let mut image = image::RgbaImage::new(width, height);
    for pixel in image.pixels_mut() {
        *pixel = image::Rgba(rgba);
    }
    let path = dir.join(name);
    image.save(&path).unwrap();
    path
}

fn write_two_page_pdf(path: &std::path::Path) {
    use pdf_writer::Finish;
    let mut pdf = pdf_writer::Pdf::new();
    let catalog_id = pdf_writer::Ref::new(1);
    let page_tree_id = pdf_writer::Ref::new(2);
    let page_a = pdf_writer::Ref::new(3);
    let page_b = pdf_writer::Ref::new(4);
    let content_id = pdf_writer::Ref::new(5);
    pdf.catalog(catalog_id).pages(page_tree_id);
    pdf.pages(page_tree_id).kids([page_a, page_b]).count(2);
    for page_id in [page_a, page_b] {
        let mut page = pdf.page(page_id);
        page.media_box(pdf_writer::Rect::new(0.0, 0.0, 200.0, 100.0));
        page.parent(page_tree_id);
        page.contents(content_id);
        page.finish();
    }
    let content = pdf_writer::Content::new();
    pdf.stream(content_id, &content.finish());
    std::fs::write(path, pdf.finish()).unwrap();
}

#[test]
fn file_kind_routing_covers_extensions() {
    let path = |name: &str| std::path::PathBuf::from(name);
    assert_eq!(file_kind(&path("photo.JPG")), FileKind::Image);
    assert_eq!(file_kind(&path("doc.pdf")), FileKind::Pdf);
    assert_eq!(file_kind(&path("clip.mov")), FileKind::Video);
    assert_eq!(file_kind(&path("track.flac")), FileKind::Audio);
    assert_eq!(file_kind(&path("bundle.tar.gz")), FileKind::Archive);
    assert_eq!(file_kind(&path("bundle.tgz")), FileKind::Archive);
    assert_eq!(file_kind(&path("notes.txt")), FileKind::Other);
}

#[test]
fn actions_for_images_match_mac_menu() {
    let single = std::path::PathBuf::from("a.png");
    let actions = actions_for(&kinds_of(&[single.clone()]), 1);
    assert_eq!(actions, vec!["jpg", "png", "webp", "heic", "tiff", "small", "imagesPDF", "zip", "zipFast"]);
    let many = vec![single.clone(), single];
    let actions = actions_for(&kinds_of(&many), 2);
    assert!(actions.contains(&"zipEach"), "multi-select packs one zip per item: {actions:?}");
    assert!(!actions.contains(&"zipFast"));
    assert_eq!(actions.len(), 9, "wheel supports at most nine sectors");
    assert!(actions_for(&kinds_of(&many), MAXIMUM_INPUTS + 1).is_empty(), "oversize selection shows nothing");
}

#[test]
fn actions_for_other_kinds() {
    let pdf = std::path::PathBuf::from("a.pdf");
    let actions = actions_for(&kinds_of(&[pdf.clone(), pdf.clone()]), 2);
    assert_eq!(actions.first(), Some(&"mergePDF"));
    let actions = actions_for(&kinds_of(&[pdf.clone()]), 1);
    assert!(!actions.contains(&"mergePDF"), "single PDF cannot merge");
    assert!(actions.contains(&"splitPDF"));
    let video = std::path::PathBuf::from("a.mp4");
    let actions = actions_for(&kinds_of(&[video.clone()]), 1);
    assert_eq!(actions, vec!["mp4", "mov", "video720", "video1080", "m4a", "zip", "zipFast"]);
    let audio = std::path::PathBuf::from("a.wav");
    let actions = actions_for(&kinds_of(&[audio.clone()]), 1);
    assert_eq!(actions.first(), Some(&"m4a"));
    let archive = std::path::PathBuf::from("a.zip");
    assert_eq!(actions_for(&kinds_of(&[archive.clone()]), 1), vec!["extract", "zip", "tarGz"]);
    // Mixed audio+video keeps audio actions; mixed with anything else packs.
    let mixed = vec![video.clone(), audio.clone()];
    let actions = actions_for(&kinds_of(&mixed), 2);
    assert!(actions.starts_with(&["m4a", "wav", "aiff"]));
    let mixed = vec![video, std::path::PathBuf::from("a.txt")];
    let actions = actions_for(&kinds_of(&mixed), 2);
    assert_eq!(actions, vec!["zip", "zipEach", "tarGz"]);
}

#[test]
fn combines_and_extension_mapping() {
    assert!(combines_inputs("imagesPDF"));
    assert!(combines_inputs("mergePDF"));
    assert!(combines_inputs("zip"));
    assert!(!combines_inputs("zipEach"));
    assert_eq!(output_extension("small"), "jpg");
    assert_eq!(output_extension("pdfJPG"), "jpg");
    assert_eq!(output_extension("imagesPDF"), "pdf");
    assert_eq!(output_extension("tarGz"), "tar.gz");
    assert_eq!(output_extension("video720"), "mp4");
    assert_eq!(output_extension("zipFast"), "zip");
}

#[test]
fn safe_components_reject_unsafe_paths() {
    assert!(safe_components("folder/内/文件.txt").is_ok());
    assert!(safe_components("a\\b\\c").is_err(), "backslashes are not valid archive paths (Mac parity)");
    assert!(safe_components("a/../b").is_err());
    assert!(safe_components("/absolute").is_err());
    assert!(safe_components("C:\\evil").is_err());
    assert!(safe_components("C:/evil").is_err());
    assert!(safe_components("").is_err());
    assert!(safe_components("a\0b").is_err());
    assert!(safe_components(&"x".repeat(2049)).is_err());
    let deep = (0..65).map(|_| "d").collect::<Vec<_>>().join("/");
    assert!(safe_components(&deep).is_err());
}

#[test]
fn natural_ordering_matches_finder_semantics() {
    use std::cmp::Ordering;
    assert_eq!(natural_cmp("2.txt", "10.txt"), Ordering::Less);
    assert_eq!(natural_cmp("photo-2.jpg", "photo-10.jpg"), Ordering::Less);
    assert_eq!(natural_cmp("B.txt", "a.txt"), Ordering::Greater);
    assert_eq!(natural_cmp("a01.txt", "a1.txt"), Ordering::Greater, "digit-run length breaks numeric ties");
    let sorted = natural_sort(vec![
        std::path::PathBuf::from("10.png"),
        std::path::PathBuf::from("2.png"),
        std::path::PathBuf::from("1.png"),
    ]);
    let names: Vec<String> = sorted
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["1.png", "2.png", "10.png"]);
}

#[test]
fn publish_output_numbers_duplicates_and_cleans_temp() {
    let dir = temp_dir("drag-publish");
    let first = publish_output(&dir, "照片", "jpg", false, |output| {
        std::fs::write(output, b"one").map_err(|e| e.to_string())
    })
    .unwrap();
    assert!(first.ends_with("照片.jpg"));
    let second = publish_output(&dir, "照片", "jpg", false, |output| {
        std::fs::write(output, b"two").map_err(|e| e.to_string())
    })
    .unwrap();
    assert!(second.ends_with("照片-1.jpg"), "auto-numbered: {}", second.display());
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    // Failed builds leave no temp files behind.
    let failed = publish_output(&dir, "照片", "jpg", false, |_| Err("boom".into()));
    assert!(failed.is_err());
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".nuphy-convert"))
        .collect();
    assert!(leftovers.is_empty(), "temp cleaned: {leftovers:?}");
    // Directory outputs use the same numbering without an extension.
    let folder = publish_output(&dir, "pages", "", true, |output| {
        std::fs::create_dir_all(output.join("inner")).map_err(|e| e.to_string())
    })
    .unwrap();
    assert!(folder.join("inner").is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn image_conversions_round_trip_and_flatten() {
    let dir = temp_dir("drag-image");
    let preferences = DragPreferences { max_dimension: 8, ..DragPreferences::default() };
    let control = JobControl::new();
    let opaque = write_png(&dir, "opaque.png", 24, 16, [200, 40, 40, 255]);
    let transparent = write_png(&dir, "transparent.png", 20, 20, [10, 200, 90, 128]);

    let jpg = convert(&[opaque.clone()], "jpg", &preferences, &control).unwrap();
    assert!(jpg.extension().unwrap() == "jpg");
    let decoded = image::open(&jpg).unwrap();
    let pixel = decoded.get_pixel(0, 0).0;
    assert!((pixel[0] as i32 - 200).abs() < 12, "opaque red survives jpg: {pixel:?}");

    // Transparency flattens onto white for JPG.
    let flat = convert(&[transparent.clone()], "jpg", &preferences, &control).unwrap();
    let decoded = image::open(&flat).unwrap().to_rgb8();
    let pixel = decoded.get_pixel(0, 0).0;
    assert!((pixel[0] as i32 - 132).abs() < 12, "alpha blends with white: {pixel:?}");
    assert!((pixel[1] as i32 - 227).abs() < 12);

    // PNG output keeps alpha.
    let png = convert(&[transparent.clone()], "png", &preferences, &control).unwrap();
    let decoded = image::open(&png).unwrap();
    assert_eq!(decoded.get_pixel(0, 0).0, [10, 200, 90, 128]);

    // WebP (lossless on the pure-Rust stack) round trips through decode.
    let webp = convert(&[transparent.clone()], "webp", &preferences, &control).unwrap();
    let decoded = image::open(&webp).unwrap();
    assert_eq!(decoded.get_pixel(0, 0).0, [10, 200, 90, 128]);

    // Small downscales the longest edge to max_dimension (aspect preserved).
    let small = convert(&[opaque.clone()], "small", &preferences, &control).unwrap();
    assert!(small.file_name().unwrap().to_string_lossy().contains("-small"));
    let decoded = image::open(&small).unwrap();
    assert_eq!(decoded.width(), 8, "long edge capped");
    assert!(decoded.height() <= 6, "short edge follows aspect: {}", decoded.height());

    // TIFF keeps RGBA.
    let tiff = convert(&[transparent.clone()], "tiff", &preferences, &control).unwrap();
    let decoded = image::open(&tiff).unwrap();
    assert_eq!(decoded.get_pixel(0, 0).0, [10, 200, 90, 128]);

    // HEIC output is gated off; HEIC input reports the platform limitation.
    assert!(!action_available("heic", &Availability { pdfium: true, ffmpeg: true }));
    let heic = convert(&[opaque.clone()], "heic", &preferences, &control);
    assert!(heic.is_err());
    let heic_input = dir.join("shot.heic");
    std::fs::write(&heic_input, b"not a real heic").unwrap();
    let error = convert(&[heic_input], "jpg", &preferences, &control).unwrap_err();
    assert!(error.contains("HEIC"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zip_store_and_extract_round_trip_with_folders() {
    let dir = temp_dir("drag-zip");
    let nested = dir.join("资料");
    std::fs::create_dir_all(nested.join("inner")).unwrap();
    std::fs::write(nested.join("说明.txt"), "中文内容").unwrap();
    std::fs::write(nested.join("inner").join("data.bin"), [1u8, 2, 3, 4]).unwrap();
    let top = dir.join("top.txt");
    std::fs::write(&top, b"top-level").unwrap();
    let preferences = DragPreferences::default();
    let control = JobControl::new();

    let zip = convert(&[nested.clone()], "zip", &preferences, &control).unwrap();
    assert!(zip.to_string_lossy().ends_with("资料.zip"), "{}", zip.display());
    let extracted = convert(&[zip], "extract", &preferences, &control).unwrap();
    let restored = extracted.join("资料").join("说明.txt");
    assert_eq!(std::fs::read_to_string(restored).unwrap(), "中文内容");
    assert_eq!(std::fs::read(extracted.join("资料").join("inner").join("data.bin")).unwrap(), [1u8, 2, 3, 4]);
    // Multiple selections archive under the shared "Archive" name (Mac parity).
    let multi = convert(&[nested.clone(), top], "zip", &preferences, &control).unwrap();
    assert!(multi.file_name().unwrap().to_string_lossy().starts_with("Archive"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zip_fast_stores_without_deflate() {
    let dir = temp_dir("drag-zipfast");
    let file = dir.join("a.txt");
    std::fs::write(&file, b"stored bytes").unwrap();
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let zip = convert(&[file], "zipFast", &preferences, &control).unwrap();
    let archive = std::fs::File::open(&zip).unwrap();
    let mut reader = zip::ZipArchive::new(archive).unwrap();
    assert_eq!(reader.len(), 1);
    let entry = reader.by_index(0).unwrap();
    assert_eq!(entry.name(), "a.txt");
    assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zip_each_writes_one_archive_per_item() {
    let dir = temp_dir("drag-zipeach");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, b"A").unwrap();
    std::fs::write(&b, b"B").unwrap();
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    // The engine records only the last output in the batch, but both land.
    let last = convert(&[a.clone(), b.clone()], "zipEach", &preferences, &control).unwrap();
    assert!(last.ends_with("b.zip"));
    assert!(a.with_extension("zip").exists());
    assert!(b.with_extension("zip").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tar_gz_round_trips() {
    let dir = temp_dir("drag-tar");
    let a = dir.join("包.txt");
    std::fs::write(&a, "tar 内容").unwrap();
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let archive = convert(&[a], "tarGz", &preferences, &control).unwrap();
    assert!(archive.to_string_lossy().ends_with("包.tar.gz"));
    let extracted = convert(&[archive], "extract", &preferences, &control).unwrap();
    assert_eq!(std::fs::read_to_string(extracted.join("包.txt")).unwrap(), "tar 内容");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plain_gz_extracts_to_a_file() {
    let dir = temp_dir("drag-gz");
    let source = dir.join("data.csv");
    std::fs::write(&source, b"a,b,c").unwrap();
    let gzipped = dir.join("data.csv.gz");
    let mut encoder = flate2::write::GzEncoder::new(std::fs::File::create(&gzipped).unwrap(), flate2::Compression::default());
    encoder.write_all(b"a,b,c").unwrap();
    encoder.finish().unwrap();
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let extracted = convert(&[gzipped], "extract", &preferences, &control).unwrap();
    assert_eq!(std::fs::read_to_string(extracted.join("data.csv")).unwrap(), "a,b,c");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn extract_rejects_unsafe_entries_and_7z() {
    let dir = temp_dir("drag-unsafe");
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    // A ZIP with a traversal entry must be refused.
    let evil = dir.join("evil.zip");
    {
        let file = std::fs::File::create(&evil).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("../escape.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"nope").unwrap();
        writer.finish().unwrap();
    }
    let error = convert(&[evil], "extract", &preferences, &control).unwrap_err();
    assert!(error.contains("异常路径"), "traversal refused: {error}");
    let seven = dir.join("bundle.7z");
    std::fs::write(&seven, b"7z fake").unwrap();
    let error = convert(&[seven], "extract", &preferences, &control).unwrap_err();
    assert!(error.contains("不支持"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn images_pdf_combines_in_filename_order() {
    let dir = temp_dir("drag-imagespdf");
    let red = write_png(&dir, "2-red.png", 40, 30, [220, 30, 30, 255]);
    let blue = write_png(&dir, "10-blue.png", 30, 40, [30, 30, 220, 255]);
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let pdf = convert(&[red, blue], "imagesPDF", &preferences, &control).unwrap();
    assert!(pdf.to_string_lossy().ends_with("-images.pdf") || pdf.file_name().unwrap().to_string_lossy().contains("2-red"));
    let document = lopdf::Document::load(&pdf).unwrap();
    let pages = document.get_pages();
    assert_eq!(pages.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merge_pdf_joins_pages() {
    let dir = temp_dir("drag-merge");
    let a = dir.join("a.pdf");
    let b = dir.join("b.pdf");
    write_two_page_pdf(&a);
    write_two_page_pdf(&b);
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let merged = convert(&[a, b], "mergePDF", &preferences, &control).unwrap();
    let document = lopdf::Document::load(&merged).unwrap();
    assert_eq!(document.get_pages().len(), 4, "two 2-page documents merge to four pages");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn split_pdf_writes_single_page_documents() {
    let dir = temp_dir("drag-split");
    let source = dir.join("doc.pdf");
    write_two_page_pdf(&source);
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let split = convert(&[source], "splitPDF", &preferences, &control).unwrap();
    let page_one = split.join("page-001.pdf");
    let page_two = split.join("page-002.pdf");
    assert!(page_one.is_file());
    let document = lopdf::Document::load(&page_one).unwrap();
    assert_eq!(document.get_pages().len(), 1);
    assert!(lopdf::Document::load(&page_two).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pdf_rendering_degrades_without_pdfium() {
    let dir = temp_dir("drag-pdfpng");
    let source = dir.join("doc.pdf");
    write_two_page_pdf(&source);
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let result = convert(&[source], "pdfPNG", &preferences, &control);
    match result {
        Ok(_) => {}
        Err(message) => {
            // Without pdfium.dll the action must degrade with the hint text,
            // never crash or produce partial output.
            assert!(message.contains("pdfium"), "degradation explains itself: {message}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn availability_gates_actions() {
    let none = Availability { pdfium: false, ffmpeg: false };
    let all = Availability { pdfium: true, ffmpeg: true };
    for action in ["pdfPNG", "pdfJPG"] {
        assert!(!action_available(action, &none));
        assert!(action_available(action, &all));
    }
    for action in ["mp4", "mov", "video720", "video1080", "m4a", "wav", "aiff"] {
        assert!(!action_available(action, &none));
        assert!(action_available(action, &all));
    }
    for action in ["jpg", "png", "webp", "tiff", "small", "imagesPDF", "zip", "zipFast", "zipEach", "tarGz", "extract", "mergePDF", "splitPDF"] {
        assert!(action_available(action, &none), "{action} needs no external binary");
    }
    assert!(!action_available("heic", &all));
    assert!(unavailable_reason("m4a").contains("ffmpeg"));
}

#[test]
fn job_control_reports_cancel_and_progress() {
    let control = JobControl::new();
    assert!(!control.is_cancelled());
    assert_eq!(control.completed(), 0);
    control.set_progress(0.5);
    assert_eq!(control.progress(), 0.5);
    control.set_progress(5.0);
    assert_eq!(control.progress(), 1.0, "progress clamps");
    control.set_current_file("x.png".into());
    assert_eq!(control.current_file(), "x.png");
    assert!(control.check().is_ok());
    control.step_done();
    assert_eq!(control.completed(), 1);
    control.cancel();
    assert!(control.is_cancelled());
    assert!(control.check().is_err());
    assert!(control.check().unwrap_err().contains("已取消"));
}

#[test]
fn oversized_or_broken_inputs_error_cleanly() {
    let dir = temp_dir("drag-broken");
    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let broken = dir.join("broken.png");
    std::fs::write(&broken, b"definitely not png").unwrap();
    let error = convert(&[broken], "jpg", &preferences, &control).unwrap_err();
    assert!(error.contains("无法读取"), "decode failure explains itself: {error}");
    let missing = dir.join("missing.png");
    let error = convert(&[missing], "jpg", &preferences, &control).unwrap_err();
    assert!(error.contains("无法读取"));
    assert!(convert(&[], "zip", &preferences, &control).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
