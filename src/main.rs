use swiftlib::{
    ipc::{ipc_recv, ipc_send},
    keyboard::{read_scancode, read_scancode_tap},
    fs, io, process,
    privileged,
    task::{find_process_by_name, yield_now},
};

const IPC_BUF_SIZE: usize = 4128;
const KAGAMI_PROCESS_CANDIDATES: [&str; 3] =
    ["/Applications/Kagami.app/entry.elf", "Kagami.app", "entry.elf"];

const OP_REQ_CREATE_WINDOW: u32 = 1;
const OP_RES_WINDOW_CREATED: u32 = 2;
const OP_REQ_FLUSH_CHUNK: u32 = 4;
const OP_REQ_ATTACH_SHARED: u32 = 5;
const OP_REQ_PRESENT_SHARED: u32 = 6;
const OP_RES_SHARED_ATTACHED: u32 = 7;
const LAYER_STATUS: u8 = 2;

struct SharedSurface {
    virt_addr: u64,
    page_count: u64,
    total_pixels: usize,
}

fn main() {
    println!("[Dock] start");
    let kagami_tid = match parse_kagami_tid_from_args().or_else(find_kagami_tid) {
        Some(tid) => tid,
        None => {
            eprintln!("[Dock] Kagami not found");
            return;
        }
    };
    let width: u16 = 320;
    let height: u16 = 100;
    let window_id = match create_window(kagami_tid, width, height) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[Dock] create window failed: {}", e);
            return;
        }
    };

    let apps = list_app_bundles();
    let mut sel = 0usize;
    let pixels = render_dock_with_apps(width as usize, height as usize, &apps, sel);
    if let Err(e) = flush_window_shared(kagami_tid, window_id, width, height, &pixels) {
        eprintln!("[Dock] shared draw failed: {}", e);
        return;
    }
    println!("[Dock] shown");

    loop {
        let sc_opt = match read_scancode_tap() {
            Ok(Some(sc)) => Some(sc),
            Ok(None) => read_scancode(),
            Err(_) => read_scancode(),
        };
        if let Some(sc) = sc_opt {
            // ESC
            if sc == 0x01 || sc == 0x81 {
                println!("[Dock] exit");
                return;
            }
            // Left arrow (press)
            if sc == 0x4B {
                if sel > 0 { sel -= 1; }
                let pixels = render_dock_with_apps(width as usize, height as usize, &apps, sel);
                let _ = flush_window_shared(kagami_tid, window_id, width, height, &pixels);
            }
            // Right arrow (press)
            if sc == 0x4D {
                if sel + 1 < apps.len() { sel += 1; }
                let pixels = render_dock_with_apps(width as usize, height as usize, &apps, sel);
                let _ = flush_window_shared(kagami_tid, window_id, width, height, &pixels);
            }
            // Enter (press)
            if sc == 0x1C {
                if let Some((app, _icon)) = apps.get(sel) {
                    let path = format!("/Applications/{}/entry.elf", app);
                    match process::exec_with_args(&path, &[]) {
                        Ok(pid) => println!("[Dock] launched {} pid={}", app, pid),
                        Err(_) => eprintln!("[Dock] failed to launch {}", app),
                    }
                }
            }
        }
        yield_now();
    }
}

fn create_window(kagami_tid: u64, width: u16, height: u16) -> Result<u32, &'static str> {
    let mut req = [0u8; 9];
    req[0..4].copy_from_slice(&OP_REQ_CREATE_WINDOW.to_le_bytes());
    req[4..6].copy_from_slice(&width.to_le_bytes());
    req[6..8].copy_from_slice(&height.to_le_bytes());
    req[8] = LAYER_STATUS;
    if (ipc_send(kagami_tid, &req) as i64) < 0 {
        return Err("send create window failed");
    }
    let mut recv = [0u8; IPC_BUF_SIZE];
    for _ in 0..256 {
        let (sender, len) = ipc_recv(&mut recv);
        if sender != kagami_tid || len < 8 {
            yield_now();
            continue;
        }
        let op = u32::from_le_bytes([recv[0], recv[1], recv[2], recv[3]]);
        if op != OP_RES_WINDOW_CREATED {
            continue;
        }
        return Ok(u32::from_le_bytes([recv[4], recv[5], recv[6], recv[7]]));
    }
    Err("window create timeout")
}

fn flush_window_chunked(
    kagami_tid: u64,
    window_id: u32,
    width: u16,
    height: u16,
    pixels: &[u32],
) -> Result<(), &'static str> {
    let total = width as usize * height as usize;
    if pixels.len() < total {
        return Err("pixel buffer too small");
    }
    let chunk_header = 20usize;
    let max_chunk_pixels = (IPC_BUF_SIZE - chunk_header) / 4;
    let width_usize = width as usize;
    let height_usize = height as usize;
    let chunk_w = width_usize.min(96).max(1);
    let chunk_h = (max_chunk_pixels / chunk_w).max(1);

    let mut y0 = 0usize;
    while y0 < height_usize {
        let h = (height_usize - y0).min(chunk_h);
        let mut x0 = 0usize;
        while x0 < width_usize {
            let w = (width_usize - x0).min(chunk_w);
            let mut msg = vec![0u8; chunk_header + (w * h * 4)];
            msg[0..4].copy_from_slice(&OP_REQ_FLUSH_CHUNK.to_le_bytes());
            msg[4..8].copy_from_slice(&window_id.to_le_bytes());
            msg[8..10].copy_from_slice(&width.to_le_bytes());
            msg[10..12].copy_from_slice(&height.to_le_bytes());
            msg[12..14].copy_from_slice(&(x0 as u16).to_le_bytes());
            msg[14..16].copy_from_slice(&(y0 as u16).to_le_bytes());
            msg[16..18].copy_from_slice(&(w as u16).to_le_bytes());
            msg[18..20].copy_from_slice(&(h as u16).to_le_bytes());
            let mut off = chunk_header;
            for row in 0..h {
                let src_row = (y0 + row) * width_usize;
                for col in 0..w {
                    msg[off..off + 4]
                        .copy_from_slice(&pixels[src_row + x0 + col].to_le_bytes());
                    off += 4;
                }
            }
            if (ipc_send(kagami_tid, &msg) as i64) < 0 {
                return Err("send flush chunk failed");
            }
            x0 += w;
        }
        y0 += h;
    }
    Ok(())
}

fn flush_window_shared(
    kagami_tid: u64,
    window_id: u32,
    width: u16,
    height: u16,
    pixels: &[u32],
) -> Result<(), &'static str> {
    let total = width as usize * height as usize;
    if pixels.len() < total {
        return Err("pixel buffer too small");
    }
    let total_bytes = total.checked_mul(4).ok_or("size overflow")?;
    let page_count = total_bytes.div_ceil(4096);
    if page_count == 0 {
        return Err("shared surface page count out of range");
    }

    let mut phys_pages = vec![0u64; page_count];
    let virt_addr = unsafe {
        privileged::alloc_shared_pages(page_count as u64, Some(phys_pages.as_mut_slice()), 0)
    };
    if (virt_addr as i64) < 0 || virt_addr == 0 {
        return Err("alloc_shared_pages failed");
    }
    let surface = SharedSurface {
        virt_addr,
        page_count: page_count as u64,
        total_pixels: total,
    };
    blit_shared_surface(&surface, pixels);

    let mut attach = [0u8; 12];
    attach[0..4].copy_from_slice(&OP_REQ_ATTACH_SHARED.to_le_bytes());
    attach[4..8].copy_from_slice(&window_id.to_le_bytes());
    attach[8..10].copy_from_slice(&width.to_le_bytes());
    attach[10..12].copy_from_slice(&height.to_le_bytes());
    if (ipc_send(kagami_tid, &attach) as i64) < 0 {
        return Err("failed to send shared attach");
    }
    let send_pages_ret = unsafe { privileged::ipc_send_pages(kagami_tid, phys_pages.as_slice(), 0) };
    if (send_pages_ret as i64) < 0 {
        return Err("failed to send shared pages");
    }
    wait_shared_attach_ack(kagami_tid, window_id)?;
    for _ in 0..3 {
        present_shared(kagami_tid, window_id)?;
        yield_now();
    }
    Ok(())
}

fn wait_shared_attach_ack(kagami_tid: u64, window_id: u32) -> Result<(), &'static str> {
    let mut recv = [0u8; IPC_BUF_SIZE];
    for _ in 0..256 {
        let (sender, len) = ipc_recv(&mut recv);
        if sender != kagami_tid || len < 8 {
            yield_now();
            continue;
        }
        let op = u32::from_le_bytes([recv[0], recv[1], recv[2], recv[3]]);
        if op != OP_RES_SHARED_ATTACHED {
            continue;
        }
        let ack_window = u32::from_le_bytes([recv[4], recv[5], recv[6], recv[7]]);
        if ack_window == window_id {
            return Ok(());
        }
    }
    Err("shared attach ack timeout")
}

fn present_shared(kagami_tid: u64, window_id: u32) -> Result<(), &'static str> {
    let mut present = [0u8; 8];
    present[0..4].copy_from_slice(&OP_REQ_PRESENT_SHARED.to_le_bytes());
    present[4..8].copy_from_slice(&window_id.to_le_bytes());
    if (ipc_send(kagami_tid, &present) as i64) < 0 {
        return Err("failed to send shared present");
    }
    Ok(())
}

fn blit_shared_surface(surface: &SharedSurface, pixels: &[u32]) {
    let count = surface.total_pixels.min(pixels.len());
    let mapped_pixels = (surface.page_count as usize).saturating_mul(4096) / 4;
    let count = count.min(mapped_pixels);
    unsafe {
        let dst = core::slice::from_raw_parts_mut(surface.virt_addr as *mut u32, count);
        for (d, s) in dst.iter_mut().zip(pixels.iter().take(count)) {
            *d = *s;
        }
    }
}

fn read_file(path: &str, max_size: usize) -> Option<Vec<u8>> {
    if max_size == 0 { return None; }
    let fd = io::open(path, io::O_RDONLY);
    if fd < 0 { return None; }
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    while out.len() < max_size {
        let read_len = core::cmp::min(buf.len(), max_size - out.len());
        let n = io::read(fd as u64, &mut buf[..read_len]);
        if (n as i64) < 0 { let _ = io::close(fd as u64); return None; }
        let n = n as usize;
        if n == 0 { break; }
        out.extend_from_slice(&buf[..n]);
    }
    let _ = io::close(fd as u64);
    if out.is_empty() { None } else { Some(out) }
}

/// Returns (bundle_name, optional icon absolute path)
fn list_app_bundles() -> Vec<(String, Option<String>)> {
    let fd = io::open("/Applications", io::O_RDONLY);
    if fd < 0 {
        return Vec::new();
    }
    let mut buf = [0u8; 4096];
    let n = fs::readdir(fd as u64, &mut buf);
    let _ = io::close(fd as u64);
    if (n as i64) <= 0 {
        return Vec::new();
    }
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    for chunk in buf[..n as usize].split(|&b| b == b'\n') {
        if chunk.is_empty() { continue; }
        if let Ok(s) = core::str::from_utf8(chunk) {
            if s.ends_with(".app") {
                let bundle = s.to_string();
                // check about.toml for icon field
                let about_path = format!("/Applications/{}/about.toml", bundle);
                let mut icon_path: Option<String> = None;
                if let Some(data) = read_file(&about_path, 4096) {
                    if let Ok(text) = core::str::from_utf8(&data) {
                        for line in text.lines() {
                            let line = line.trim();
                            if line.starts_with("icon") {
                                if let Some(pos) = line.find('=') {
                                    let mut val = line[pos+1..].trim();
                                    if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                                        val = &val[1..val.len()-1];
                                    }
                                    if !val.is_empty() {
                                        let candidate = format!("/Applications/{}/{}", bundle, val);
                                        // existence check
                                        if let Some(_) = read_file(&candidate, 1) {
                                            icon_path = Some(candidate);
                                        }
                                    }
                                }
                            }
                            if icon_path.is_some() { break; }
                        }
                    }
                }
                // fallback: check common icon files at bundle root
                if icon_path.is_none() {
                    for fname in ["icon.png", "icon.jpeg", "icon.jpg"] {
                        let candidate = format!("/Applications/{}/{}", bundle, fname);
                        if let Some(_) = read_file(&candidate, 1) {
                            icon_path = Some(candidate);
                            break;
                        }
                    }
                }
                entries.push((bundle, icon_path));
            }
        }
    }
    entries
}

fn render_dock_with_apps(width: usize, height: usize, apps: &Vec<(String, Option<String>)>, selected: usize) -> Vec<u32> {
    let mut px = vec![0u32; width * height];
    let dock_w = (apps.len().saturating_mul(48).saturating_add(36)).min(width);
    let dock_h = 75i32;
    let dock_x = ((width as i32 - dock_w as i32) / 2).max(0);
    let dock_y = (height as i32 - dock_h).max(0);
    fill_rounded_rect(&mut px, width, dock_x, dock_y, dock_w as i32, dock_h, 22, 0x4BF6_F8FC);
    stroke_rounded_rect(&mut px, width, dock_x, dock_y, dock_w as i32, dock_h, 22, 0x4BCD_D7E4);

    let mut icon_x = dock_x + 18;
    let icon_y = dock_y + 18;
    for (i, (name, icon_opt)) in apps.iter().enumerate() {
        let color = if let Some(path) = icon_opt {
            palette_from_icon_path(path)
        } else {
            palette_from_name(name)
        };
        let ix = icon_x;
        let iy = icon_y;
        fill_rounded_rect(&mut px, width, ix, iy, 40, 40, 14, color);
        // selection highlight
        if i == selected {
            stroke_rounded_rect(&mut px, width, ix - 2, iy - 2, 44, 44, 16, 0xFF00_0000);
        }
        // draw first letter as simple label (white)
        let label = name.trim_end_matches(".app");
        if let Some(ch) = label.chars().next() {
            draw_char_on_icon(&mut px, width, ix, iy, ch);
        }
        icon_x += 48;
    }
    px
}

fn palette_from_icon_path(path: &str) -> u32 {
    if let Some(data) = read_file(path, 4096) {
        let mut h: u32 = 0;
        for b in data.iter().take(256) {
            h = h.wrapping_mul(131).wrapping_add(*b as u32);
        }
        let r = ((h >> 16) & 0xFF) as u32;
        let g = ((h >> 8) & 0xFF) as u32;
        let b = (h & 0xFF) as u32;
        0xFF00_0000 | (r << 16) | (g << 8) | b
    } else {
        0xFF60_A5FA
    }
}

fn palette_from_name(name: &str) -> u32 {
    // simple hash to color
    let mut h: u32 = 0;
    for b in name.as_bytes() {
        h = h.wrapping_mul(31).wrapping_add(*b as u32);
    }
    let r = ((h >> 16) & 0xFF) as u32;
    let g = ((h >> 8) & 0xFF) as u32;
    let b = (h & 0xFF) as u32;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

fn draw_char_on_icon(px: &mut [u32], stride: usize, ix: i32, iy: i32, ch: char) {
    // very small 6x8 font: render as a single pixel for now (center)
    let cx = ix + 20;
    let cy = iy + 12;
    if cx >= 0 && cy >= 0 && (cx as usize) < stride && (cy as usize) < (px.len() / stride) {
        let idx = (cy as usize) * stride + (cx as usize);
        px[idx] = 0xFFFFFFFF;
    }
}

fn fill_rounded_rect(
    px: &mut [u32],
    stride: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: u32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2).max(0);
    for yy in 0..h {
        for xx in 0..w {
            let cov = rounded_rect_coverage(xx, yy, w, h, r);
            if cov != 0 {
                blend_put(px, stride, x + xx, y + yy, color, cov);
            }
        }
    }
}

fn stroke_rounded_rect(
    px: &mut [u32],
    stride: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: u32,
) {
    if w <= 2 || h <= 2 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2).max(0);
    for yy in 0..h {
        for xx in 0..w {
            let outer = rounded_rect_coverage(xx, yy, w, h, r);
            let inner = rounded_rect_coverage(xx - 1, yy - 1, w - 2, h - 2, (r - 1).max(0));
            let cov = outer.saturating_sub(inner);
            if cov != 0 {
                blend_put(px, stride, x + xx, y + yy, color, cov);
            }
        }
    }
}

fn rounded_rect_coverage(xx: i32, yy: i32, w: i32, h: i32, r: i32) -> u8 {
    if w <= 0 || h <= 0 || xx < 0 || yy < 0 || xx >= w || yy >= h {
        return 0;
    }
    let samples = [
        (0.25f32, 0.25f32),
        (0.75f32, 0.25f32),
        (0.25f32, 0.75f32),
        (0.75f32, 0.75f32),
    ];
    let mut hit = 0u8;
    for (ox, oy) in samples {
        if inside_rounded_rect_f(xx as f32 + ox, yy as f32 + oy, w as f32, h as f32, r as f32) {
            hit += 1;
        }
    }
    hit.saturating_mul(64)
}

fn inside_rounded_rect_f(x: f32, y: f32, w: f32, h: f32, r: f32) -> bool {
    if x < 0.0 || y < 0.0 || x >= w || y >= h {
        return false;
    }
    if r <= 0.0 || (x >= r && x < w - r) || (y >= r && y < h - r) {
        return true;
    }
    let cx = if x < r { r } else { w - r };
    let cy = if y < r { r } else { h - r };
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= r * r
}

fn blend_put(px: &mut [u32], stride: usize, x: i32, y: i32, src: u32, alpha: u8) {
    if x < 0 || y < 0 || alpha == 0 {
        return;
    }
    let x = x as usize;
    let y = y as usize;
    let h = px.len() / stride;
    if x >= stride || y >= h {
        return;
    }
    let idx = y * stride + x;
    let dst = px[idx];
    let src_a = ((((src >> 24) & 0xFF) * (alpha as u32)) / 255) as u8;
    let src_px = ((src_a as u32) << 24) | (src & 0x00FF_FFFF);
    px[idx] = alpha_over(dst, src_px);
}

fn alpha_over(dst: u32, src: u32) -> u32 {
    let sa = (src >> 24) & 0xFF;
    if sa == 0 {
        return dst;
    }
    let da = (dst >> 24) & 0xFF;
    if da == 0 {
        return src;
    }
    let inv_sa = 255 - sa;
    let out_a = sa + (da * inv_sa + 127) / 255;
    if out_a == 0 {
        return 0;
    }
    let sr = (src >> 16) & 0xFF;
    let sg = (src >> 8) & 0xFF;
    let sb = src & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let src_w = sa * 255;
    let dst_w = da * inv_sa;
    let denom = out_a * 255;
    let r = (sr * src_w + dr * dst_w + denom / 2) / denom;
    let g = (sg * src_w + dg * dst_w + denom / 2) / denom;
    let b = (sb * src_w + db * dst_w + denom / 2) / denom;
    (out_a << 24) | (r << 16) | (g << 8) | b
}

fn blend_rgb(dst: u32, src: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let inv = 255u32.saturating_sub(a);
    let sr = (src >> 16) & 0xFF;
    let sg = (src >> 8) & 0xFF;
    let sb = src & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let r = (sr * a + dr * inv) / 255;
    let g = (sg * a + dg * inv) / 255;
    let b = (sb * a + db * inv) / 255;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

fn find_kagami_tid() -> Option<u64> {
    for name in KAGAMI_PROCESS_CANDIDATES {
        if let Some(tid) = find_process_by_name(name) {
            return Some(tid);
        }
    }
    None
}

fn parse_kagami_tid_from_args() -> Option<u64> {
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--kagami-tid=")
            && let Ok(tid) = rest.parse::<u64>()
            && tid != 0
        {
            return Some(tid);
        }
    }
    None
}
