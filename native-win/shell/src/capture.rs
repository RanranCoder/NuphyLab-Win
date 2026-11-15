// Screen-ambient capture through DXGI Desktop Duplication: the primary
// monitor is duplicated on the GPU, copied to a staging texture and averaged
// into a 6x16 zone grid that mirrors the keyboard's aspect. Pixels never
// leave this process — only zone averages do. Session management mirrors
// audio.rs: desired state arrives from the /api/state poll, a generation
// counter stops stale threads, and every failure is reported, never swallowed.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use windows::core::{Interface, Result as ComResult};
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11CreateDevice, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::CoInitializeEx;

use crate::https;
use crate::worker::WorkerLog;

/// Zone grid fixed for v1: 6 keyboard rows x 16 columns.
pub const ZONE_ROWS: usize = 6;
pub const ZONE_COLS: usize = 16;
const EMIT_INTERVAL: Duration = Duration::from_millis(45);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// Averages a BGRA bitmap into a rows x cols grid of RGB triples. `row_pitch`
/// is the byte distance between rows (GPU staging textures are padded).
pub fn zone_average(pixels: &[u8], row_pitch: usize, width: usize, height: usize, rows: usize, cols: usize) -> Vec<[u8; 3]> {
    let mut zones = Vec::with_capacity(rows * cols);
    if width == 0 || height == 0 || pixels.len() < row_pitch * height || row_pitch < width * 4 {
        return vec![[0, 0, 0]; rows * cols];
    }
    for row in 0..rows {
        let y0 = height * row / rows;
        let y1 = (height * (row + 1) / rows).max(y0 + 1);
        let step_y = ((y1 - y0) / 6).max(1);
        for col in 0..cols {
            let x0 = width * col / cols;
            let x1 = (width * (col + 1) / cols).max(x0 + 1);
            let step_x = ((x1 - x0) / 6).max(1);
            let (mut sum, mut count) = ([0u64; 3], 0u64);
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    let offset = y * row_pitch + x * 4;
                    // BGRA memory order.
                    sum[0] += pixels[offset + 2] as u64;
                    sum[1] += pixels[offset + 1] as u64;
                    sum[2] += pixels[offset] as u64;
                    count += 1;
                    x += step_x;
                }
                y += step_y;
            }
            zones.push(if count == 0 {
                [0, 0, 0]
            } else {
                [(sum[0] / count) as u8, (sum[1] / count) as u8, (sum[2] / count) as u8]
            });
        }
    }
    zones
}

// ---------------------------------------------------------------------------
// Session management, mirroring audio.rs.

static SESSION: Mutex<String> = Mutex::new(String::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn dispatch(packet: Value, base: &str, token: &str) {
    https::post_command(base, token, &packet, Duration::from_secs(2));
}

fn packet(session: &str, status: &str, error: Option<&str>) -> Value {
    let mut packet = json!({"op": "screenPacket", "session": session, "status": status});
    if let Some(error) = error {
        packet["error"] = json!(error);
    }
    packet
}

/// Desired state comes from the /api/state poll (0.5 s), audio.sync parity.
pub fn sync(desired: Option<&Value>, base: &str, token: &str, log: &Arc<WorkerLog>) {
    let desired = desired.filter(|value| {
        value.get("enabled") == Some(&Value::Bool(true))
            && value.get("session").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
            && value.get("source").and_then(Value::as_str) == Some("screen")
    });
    let Some(desired) = desired else {
        stop_active();
        return;
    };
    let session = desired.get("session").and_then(Value::as_str).unwrap_or_default().to_owned();
    {
        let mut guard = SESSION.lock().unwrap();
        if *guard == session {
            return;
        }
        *guard = session.clone();
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    dispatch(packet(&session, "requesting", None), base, token);
    let log = log.clone();
    let base = base.to_owned();
    let token = token.to_owned();
    std::thread::spawn(move || {
        capture_thread(session, base, token, log, generation);
    });
}

pub fn stop_active() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    SESSION.lock().unwrap().clear();
}

struct Pipeline {
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    duplication: IDXGIOutputDuplication,
    width: usize,
    height: usize,
    /// Desktop-coordinate origin of the duplicated output (for region math).
    output_desktop: (i32, i32),
}

fn create_device() -> ComResult<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        // Hardware first, basic-render driver (VMs, GPU-less machines) second.
        for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            if D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .is_ok()
                && device.is_some()
                && context.is_some()
            {
                return Ok((device.unwrap(), context.unwrap()));
            }
        }
        Err(windows::core::Error::from(windows::core::HRESULT(-1)))
    }
}

fn create_pipeline() -> ComResult<Pipeline> {
    unsafe {
        // Desktop duplication is COM-based; S_FALSE (already initialized) is
        // a success code, so only Err(_) matters here.
        let _ = CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);

        let (device, context) = create_device()?;
        let adapter: IDXGIAdapter1 = device.cast::<IDXGIDevice>()?.GetAdapter()?.cast()?;
        // The primary monitor decides which output gets duplicated.
        let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut output: Option<IDXGIOutput> = None;
        let mut index = 0u32;
        while let Ok(candidate) = adapter.EnumOutputs(index) {
            if let Ok(desc) = candidate.GetDesc() {
                if desc.Monitor == primary {
                    output = Some(candidate);
                    break;
                }
            }
            index += 1;
        }
        build_pipeline(device, context, output)
    }
}

/// One-shot region capture for Smart Selection: duplicates the output that
/// contains the rect (falling back to the first output), grabs one frame and
/// encodes the sub-rect as PNG. `rect` is (x0, y0, x1, y1) in virtual-desktop
/// coordinates; it is clamped to the chosen output.
pub fn capture_region_png(rect: (i32, i32, i32, i32)) -> Result<Vec<u8>, String> {
    let (rect_x0, rect_y0, rect_x1, rect_y1) = rect;
    if rect_x1 - rect_x0 < 1 || rect_y1 - rect_y0 < 1 {
        return Err("选区太小".into());
    }
    unsafe {
        let _ = CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let output = find_output_for(rect_x0, rect_y0)?;
    let pipeline = match output {
        Some(output) => unsafe {
            let _ = CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            let (device, context) = create_device().map_err(|e| e.to_string())?;
            build_pipeline(device, context, Some(output)).map_err(|e| e.to_string())?
        },
        None => create_pipeline().map_err(|e| e.to_string())?,
    };
    unsafe {
        let output_desc = pipeline.duplication.GetDesc();
        let rotation = output_desc.Rotation.0;
        if !(0..=1).contains(&rotation) {
            return Err("暂不支持旋转显示器的框选捕获".into());
        }
        let desktop = pipeline.output_desktop;
        // Clamp the request into the duplicated output's desktop coordinates.
        let x0 = rect_x0.max(desktop.0);
        let y0 = rect_y0.max(desktop.1);
        let x1 = rect_x1.min(desktop.0 + pipeline.width as i32);
        let y1 = rect_y1.min(desktop.1 + pipeline.height as i32);
        if x1 - x0 < 1 || y1 - y0 < 1 {
            return Err("选区与显示器不相交".into());
        }
        // Grab one fresh frame (a static desktop may need a few timeouts; the
        // cursor usually keeps the duplication producing frames).
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut captured: Option<(Vec<u8>, usize)> = None;
        while Instant::now() < deadline {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match pipeline.duplication.AcquireNextFrame(150, &mut frame_info, &mut resource) {
                Ok(()) => {
                    if let Some(texture) = resource.and_then(|resource| resource.cast::<ID3D11Texture2D>().ok()) {
                        pipeline.context.CopyResource(&pipeline.staging, &texture);
                    }
                    let _ = pipeline.duplication.ReleaseFrame();
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    if pipeline.context.Map(&pipeline.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_ok() {
                        let length = mapped.RowPitch as usize * pipeline.height;
                        let pixels = std::slice::from_raw_parts(mapped.pData as *const u8, length);
                        captured = Some((pixels.to_vec(), mapped.RowPitch as usize));
                        pipeline.context.Unmap(&pipeline.staging, 0);
                    }
                    break;
                }
                Err(error) => {
                    if error.code() != DXGI_ERROR_WAIT_TIMEOUT {
                        return Err(format!("屏幕帧获取失败：{error}"));
                    }
                }
            }
        }
        let Some((pixels, row_pitch)) = captured else {
            return Err("屏幕帧获取超时".into());
        };
        // Extract the sub-rect and convert BGRA → RGBA.
        let width = (x1 - x0) as usize;
        let height = (y1 - y0) as usize;
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            let row = (y0 as usize + y) * row_pitch;
            for x in 0..width {
                let offset = row + (x0 as usize + x) * 4;
                if offset + 3 >= pixels.len() {
                    return Err("屏幕数据越界".into());
                }
                // BGRA memory order.
                rgba.push(pixels[offset + 2]);
                rgba.push(pixels[offset + 1]);
                rgba.push(pixels[offset]);
                rgba.push(pixels[offset + 3]);
            }
        }
        encode_png(&rgba, width as u32, height as u32)
    }
}

/// Finds the adapter output whose desktop coordinates contain (x, y).
fn find_output_for(x: i32, y: i32) -> Result<Option<IDXGIOutput>, String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| e.to_string())?;
        let mut fallback: Option<IDXGIOutput> = None;
        let mut adapter_index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            let mut output_index = 0u32;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                if let Ok(desc) = output.GetDesc() {
                    let desktop = desc.DesktopCoordinates;
                    if x >= desktop.left && y >= desktop.top && x < desktop.right && y < desktop.bottom {
                        return Ok(Some(output));
                    }
                    if fallback.is_none() {
                        fallback = Some(output);
                    }
                }
                output_index += 1;
            }
            adapter_index += 1;
        }
        Ok(fallback)
    }
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(encoded)
}

unsafe fn build_pipeline(device: ID3D11Device, context: ID3D11DeviceContext, output: Option<IDXGIOutput>) -> ComResult<Pipeline> {
    let output = output.ok_or(windows::core::Error::from(windows::core::HRESULT(-1)))?;
    let desc = output.GetDesc()?;
    let desktop = (desc.DesktopCoordinates.left, desc.DesktopCoordinates.top);
    let output1: IDXGIOutput1 = output.cast()?;
    // Ask for BGRA8 explicitly (IDXGIOutput5, Win10 1703+). CopyResource
    // requires identical formats, and an HDR / 10-bit desktop would
    // otherwise hand us a surface the staging texture cannot receive — a
    // silently black frame. Older outputs fall back to the native format.
    let requested: windows::core::Result<IDXGIOutputDuplication> = output
        .cast::<IDXGIOutput5>()
        .and_then(|output5| output5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]));
    let duplication: IDXGIOutputDuplication = match requested {
        Ok(duplication) => duplication,
        Err(_) => output1.DuplicateOutput(&device)?,
    };

    let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(1) as usize;
    let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(1) as usize;
    // The duplicated surface uses the mode's own dimensions; on a rotated
    // (portrait) display those differ from the desktop rect. Sizing the
    // staging texture from the surface keeps CopyResource valid there too
    // (the zone grid then follows the surface orientation).
    let surface = duplication.GetDesc();
    let surface_width = if surface.ModeDesc.Width > 0 { surface.ModeDesc.Width as usize } else { width };
    let surface_height = if surface.ModeDesc.Height > 0 { surface.ModeDesc.Height as usize } else { height };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: surface_width as u32,
        Height: surface_height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
    let staging = staging.ok_or(windows::core::Error::from(windows::core::HRESULT(-1)))?;
    Ok(Pipeline { context, staging, duplication, width: surface_width, height: surface_height, output_desktop: desktop })
}

fn capture_thread(session: String, base: String, token: String, log: Arc<WorkerLog>, generation: u64) {
    let failed = |code: &str| {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        SESSION.lock().unwrap().clear();
        dispatch(packet(&session, "error", Some(code)), &base, &token);
    };
    let stale = || GENERATION.load(Ordering::SeqCst) != generation;
    let mut pipeline_failures = 0u32;
    loop {
        if stale() {
            return;
        }
        let pipeline = match create_pipeline() {
            Ok(pipeline) => pipeline,
            Err(error) => {
                pipeline_failures += 1;
                if pipeline_failures > 5 {
                    log.append(format!("[screen] pipeline creation kept failing: {error}\n").as_bytes());
                    failed("screenCapture");
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        pipeline_failures = 0;
        dispatch(packet(&session, "listening", None), &base, &token);
        log.append(
            format!("[screen] duplication started: {}x{}\n", pipeline.width, pipeline.height).as_bytes(),
        );
        let mut last_emit = Instant::now();
        let mut last_keepalive = Instant::now();
        let mut recreate = false;
        loop {
            if stale() {
                break;
            }
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let result = unsafe {
                pipeline
                    .duplication
                    .AcquireNextFrame(100, &mut frame_info, &mut resource)
            };
            if result.is_ok() {
                unsafe {
                    // Copy while the frame is acquired; release before mapping.
                    if let Some(texture) = resource.and_then(|resource| resource.cast::<ID3D11Texture2D>().ok()) {
                        pipeline.context.CopyResource(&pipeline.staging, &texture);
                    }
                    let _ = pipeline.duplication.ReleaseFrame();
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    if pipeline
                        .context
                        .Map(&pipeline.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .is_ok()
                    {
                        let length = mapped.RowPitch as usize * pipeline.height;
                        let pixels = std::slice::from_raw_parts(mapped.pData as *const u8, length);
                        let zones = zone_average(
                            pixels,
                            mapped.RowPitch as usize,
                            pipeline.width,
                            pipeline.height,
                            ZONE_ROWS,
                            ZONE_COLS,
                        );
                        pipeline.context.Unmap(&pipeline.staging, 0);
                        if last_emit.elapsed() >= EMIT_INTERVAL {
                            let value = json!({
                                "op": "screenPacket", "session": session, "status": "listening",
                                "rows": ZONE_ROWS, "cols": ZONE_COLS,
                                "zones": zones.iter().map(|[r, g, b]| json!([r, g, b])).collect::<Vec<_>>(),
                            });
                            dispatch(value, &base, &token);
                            last_emit = Instant::now();
                            last_keepalive = last_emit;
                        }
                    } else {
                        recreate = true;
                    }
                }
            } else if let Err(error) = &result {
                let code = error.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    // No screen changes: keep the listening status alive.
                    if last_keepalive.elapsed() > KEEPALIVE_INTERVAL {
                        dispatch(packet(&session, "listening", None), &base, &token);
                        last_keepalive = Instant::now();
                    }
                } else if code == DXGI_ERROR_ACCESS_LOST {
                    // Mode change / fullscreen transition / desktop switch.
                    recreate = true;
                } else {
                    log.append(format!("[screen] AcquireNextFrame failed: {code}\n").as_bytes());
                    failed("screenCapture");
                    return;
                }
            }
            if recreate {
                std::thread::sleep(Duration::from_millis(300));
                break; // outer loop recreates
            }
        }
        if stale() {
            log.append(b"[screen] capture stopped\n");
            return;
        }
    }
}

/// Standalone DXGI validation used by examples/screen-probe.rs: duplicates the
/// primary output for `seconds`, averages every frame's zones and reports how
/// much the picture actually changed. No pixels are stored.
pub struct ProbeSummary {
    pub width: usize,
    pub height: usize,
    pub frames: u32,
    pub zone_spread: u8,
    pub zones: Vec<[u8; 3]>,
    pub source_format: String,
    pub rotation: i32,
    pub copy_failures: u32,
}

pub fn probe_duplication(seconds: f64) -> Result<ProbeSummary, String> {
    let pipeline = create_pipeline().map_err(|error| format!("pipeline: {error}"))?;
    let (source_format, rotation) = unsafe {
        let desc = pipeline.duplication.GetDesc();
        (format!("{:?}", desc.ModeDesc.Format), desc.Rotation.0)
    };
    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.5));
    let mut frames = 0u32;
    let mut copy_failures = 0u32;
    let mut accumulated = vec![[0u64; 3]; ZONE_ROWS * ZONE_COLS];
    let mut min_channel = [255u8; 3];
    let mut max_channel = [0u8; 3];
    while Instant::now() < deadline {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let result = unsafe { pipeline.duplication.AcquireNextFrame(200, &mut frame_info, &mut resource) };
        if result.is_err() {
            continue;
        }
        unsafe {
            match resource.and_then(|resource| resource.cast::<ID3D11Texture2D>().ok()) {
                Some(texture) => pipeline.context.CopyResource(&pipeline.staging, &texture),
                None => copy_failures += 1,
            }
            let _ = pipeline.duplication.ReleaseFrame();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if pipeline
                .context
                .Map(&pipeline.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .is_ok()
            {
                let length = mapped.RowPitch as usize * pipeline.height;
                let pixels = std::slice::from_raw_parts(mapped.pData as *const u8, length);
                let zones = zone_average(
                    pixels,
                    mapped.RowPitch as usize,
                    pipeline.width,
                    pipeline.height,
                    ZONE_ROWS,
                    ZONE_COLS,
                );
                pipeline.context.Unmap(&pipeline.staging, 0);
                frames += 1;
                for (zone, sum) in zones.iter().zip(accumulated.iter_mut()) {
                    for axis in 0..3 {
                        sum[axis] += zone[axis] as u64;
                        min_channel[axis] = min_channel[axis].min(zone[axis]);
                        max_channel[axis] = max_channel[axis].max(zone[axis]);
                    }
                }
            }
        }
    }
    let count = frames.max(1) as u64;
    let zones = accumulated
        .iter()
        .map(|sum| [(sum[0] / count) as u8, (sum[1] / count) as u8, (sum[2] / count) as u8])
        .collect();
    // How much the brightest and darkest zone differ: a black window that only
    // captured desktop wallpaper shows a small spread, a real screen is larger.
    let zone_spread = (0..3)
        .map(|axis| max_channel[axis].saturating_sub(min_channel[axis]))
        .max()
        .unwrap_or(0);
    Ok(ProbeSummary {
        width: pipeline.width,
        height: pipeline.height,
        frames,
        zone_spread,
        zones,
        source_format,
        rotation,
        copy_failures,
    })
}

/// Renders a probe summary as plain text (one zone row per line).
pub fn probe_report(summary: &ProbeSummary) -> String {
    let mut out = format!(
        "output: {}x{}\nframes captured: {}\nsource format: {}\nrotation: {}\ncopy failures: {}\nzone spread (max channel delta): {}\n",
        summary.width,
        summary.height,
        summary.frames,
        summary.source_format,
        summary.rotation,
        summary.copy_failures,
        summary.zone_spread
    );
    for row in 0..ZONE_ROWS {
        let line = (0..ZONE_COLS)
            .map(|col| {
                let [r, g, b] = summary.zones[row * ZONE_COLS + col];
                format!("#{r:02x}{g:02x}{b:02x}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("row {row}: {line}\n"));
    }
    if summary.frames == 0 {
        out.push_str("no frames acquired — another app may hold the duplication\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_quadrants_average_to_their_own_colors() {
        // 4x2 BGRA image: top half blue, bottom half green.
        let width = 4;
        let height = 2;
        let pitch = width * 4;
        let mut pixels = vec![0u8; pitch * height];
        for byte in pixels.chunks_exact_mut(4).take(width * height / 2) {
            byte[0] = 255; // blue quadrants: B=255
        }
        for byte in pixels.chunks_exact_mut(4).skip(width * height / 2) {
            byte[1] = 200; // green quadrants: G=200
        }
        let zones = zone_average(&pixels, pitch, width, height, 2, 2);
        assert_eq!(zones.len(), 4);
        for (index, zone) in zones.iter().enumerate() {
            if index < 2 {
                assert_eq!(zone, &[0, 0, 255]);
            } else {
                assert_eq!(zone, &[0, 200, 0]);
            }
        }
    }

    #[test]
    fn row_pitch_padding_is_skipped() {
        // Width 2 with 4 bytes of padding per row; only the first 8 bytes of
        // each row are pixels.
        let mut pixels = vec![0u8; 12 * 2];
        for row in 0..2 {
            pixels[row * 12] = 10; // B
            pixels[row * 12 + 1] = 20; // G
            pixels[row * 12 + 2] = 30; // R
            pixels[row * 12 + 3..row * 12 + 8].fill(255); // second pixel: white
        }
        let zones = zone_average(&pixels, 12, 2, 2, 1, 1);
        // Average of (30,20,10) and (255,255,255) rounded down.
        assert_eq!(zones[0], [142, 137, 132]);
    }

    #[test]
    fn degenerate_inputs_yield_black_zones() {
        assert_eq!(zone_average(&[], 0, 0, 0, 6, 16).len(), 6 * 16);
        assert_eq!(zone_average(&[0; 16], 4, 4, 1, 2, 2), vec![[0, 0, 0]; 4]);
    }
}
