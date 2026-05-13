use swiftlib::{
    fs,
    keyboard::{read_scancode, read_scancode_tap},
    process,
    task::yield_now,
};
use viewkit::{ipc_proto, Window};

fn main() {
    println!("[Dock] start");
    let width: u16 = 320;
    let height: u16 = 100;

    let mut window = match Window::new(width, height, ipc_proto::LAYER_STATUS) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[Dock] window init failed: {}", e);
            return;
        }
    };
    println!("[Dock] window ready id={}", window.id());

    println!("[Dock] listing apps...");
    let apps = list_app_bundles();

    println!("[Dock] apps count={}", apps.len());
    let mut sel = 0usize;

    println!("[Dock] rendering...");
    let pixels = render_dock_component(&apps, sel, width as usize, height as usize);
    println!("[Dock] render done (pixels={})", pixels.len());
    
    println!("[Dock] presenting...");
    if let Err(e) = window.present(&pixels) {
        eprintln!("[Dock] present failed: {}", e);
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
                if sel > 0 {
                    sel -= 1;
                }
                let pixels = render_dock_component(&apps, sel, width as usize, height as usize);
                let _ = window.present(&pixels);
            }
            // Right arrow (press)
            if sc == 0x4D {
                if sel + 1 < apps.len() {
                    sel += 1;
                }
                let pixels = render_dock_component(&apps, sel, width as usize, height as usize);
                let _ = window.present(&pixels);
            }
            // Enter (press)
            if sc == 0x1C {
                if let Some((app, _icon)) = apps.get(sel) {
                    let path = format!("/applications/{}/entry.elf", app);
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

fn render_dock_component(
    apps: &Vec<(String, Option<String>)>,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<u32> {
    use viewkit::VComponent;
    
    let appicon_template = include_str!("components/appicon.html");

    let mut icons = Vec::new();
    for (i, (name, icon_opt)) in apps.iter().enumerate() {
        let mut appicon = VComponent::from_str(appicon_template);
        
        if let Some(path) = icon_opt {
            println!("[Dokc] loading icon for {} from {}", name, path);
            appicon = appicon.image(path.clone());
        } else {
            println!("[Dock] no icon for {}, using default", name);
            let label = name.trim_end_matches(".app");
            appicon = appicon.text(label.to_string());
        }
        
        if i == selected {
            appicon = appicon.class("selected");
        }
        
        icons.push(appicon);
    }

    let dock_template = include_str!("components/dock.html");
    let dock = VComponent::from_str(dock_template)
        .children(icons);

    viewkit::render_component_to_pixmap(&dock, width as u32, height as u32)
}

fn read_file(path: &str, max_size: usize) -> Option<Vec<u8>> {
    match fs::read_file_via_fs(path, max_size) {
        Ok(Some(data)) => Some(data),
        _ => None,
    }
}

fn list_app_bundles() -> Vec<(String, Option<String>)> {
    let mut apps = Vec::new();
    let dir_path = "/applications/";
    
    match fs::open_via_fs(dir_path) {
        Ok(fd) => {
            let mut buf = vec![0u8; 4096];
            for _ in 0..4096 {
                let n = fs::readdir(fd, &mut buf);
                if n == 0 {
                    break;
                }
                if n > 0xFFFF_FFFF_0000_0000 {
                    break;
                }
                let n = n as usize;
                if n > buf.len() {
                    break;
                }
                
                if n >= 2 {
                    let name_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
                    if name_len > 0 && name_len <= n - 2 {
                        let name_bytes = &buf[2..2 + name_len];
                        if let Ok(name_str) = String::from_utf8(name_bytes.to_vec()) {
                            if name_str.ends_with(".app") {
                                let app_path = format!("{}{}", dir_path, name_str);
                                let about_toml_path = format!("{}/about.toml", app_path);
                                let icon = read_icon_from_about_toml(&about_toml_path);
                                apps.push((name_str, icon));
                            }
                        }
                    }
                }
            }
            fs::close_via_fs(fd);
        }
        Err(_) => {}
    }
    
    apps.sort_by(|a, b| a.0.cmp(&b.0));
    apps
}

fn read_icon_from_about_toml(path: &str) -> Option<String> {
    let contents = read_file(path, 8192)?;
    let text = String::from_utf8(contents).ok()?;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("icon") && line.contains('=') {
            if let Some(value) = line.split('=').nth(1) {
                let value = value
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'' || c == ' ');
                if !value.is_empty() {
                    if value.starts_with('/') {
                        return Some(value.to_string());
                    }
                    if let Some(dir) = path.rsplit_once('/') {
                        let base = dir.0;
                        return Some(format!("{}/{}", base, value));
                    }
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}
