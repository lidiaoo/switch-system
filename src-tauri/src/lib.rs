use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager,
};

const SOURCE_FILES: [(&str, &str, &str); 3] = [
    ("linux", "Linux", "PreviousBoot-linux"),
    ("windows", "Windows", "PreviousBoot-windows"),
    ("macos", "macOS", "PreviousBoot-mac"),
];
const ESP_PARTTYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const FAT_FILESYSTEMS: [&str; 6] = ["vfat", "fat", "fat12", "fat16", "fat32", "exfat"];

#[derive(Default)]
struct AppState {
    vars_dir: Option<PathBuf>,
    explicit_dir: Option<PathBuf>,
    active_mount: Option<ActiveMount>,
    variables: Vec<VariableInfo>,
    message: String,
    error: Option<String>,
}

#[derive(Clone)]
struct ActiveMount {
    device: PathBuf,
    mountpoint: PathBuf,
}

impl ActiveMount {
    fn unmount(self) -> io::Result<()> {
        Command::new("umount")
            .arg(&self.mountpoint)
            .status()
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other("umount failed"))
                }
            })?;
        let _ = fs::remove_dir(&self.mountpoint);
        Ok(())
    }

    fn remount(&self, mode: &str) -> io::Result<()> {
        let output = Command::new("mount")
            .arg("-o")
            .arg(format!("remount,{mode}"))
            .arg(&self.device)
            .arg(&self.mountpoint)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).trim(),
            ))
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VariableInfo {
    system: String,
    display_name: String,
    source_name: String,
    source: PathBuf,
    exists: bool,
    active: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppStatus {
    vars_dir: Option<PathBuf>,
    variables: Vec<VariableInfo>,
    message: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<LsblkDevice>,
}

#[derive(Deserialize)]
struct LsblkDevice {
    path: Option<String>,
    fstype: Option<String>,
    label: Option<String>,
    partlabel: Option<String>,
    parttype: Option<String>,
    mountpoints: Option<Vec<Option<String>>>,
}

struct ScanResult {
    vars_dir: PathBuf,
    active_mount: Option<ActiveMount>,
    variables: Vec<VariableInfo>,
}

fn file_hash(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hashes_match(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (file_hash(left), file_hash(right)) else {
        return false;
    };
    left == right
}

fn display_path(path: &Path) -> PathBuf {
    let path_text = path.as_os_str().to_string_lossy();
    if let Some(unc_path) = path_text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc_path}"))
    } else if let Some(local_path) = path_text.strip_prefix(r"\\?\") {
        PathBuf::from(local_path)
    } else {
        path.to_path_buf()
    }
}

fn inspect_variables(directory: &Path) -> io::Result<Vec<VariableInfo>> {
    let target = directory.join("PreviousBoot");
    let mut variables = Vec::new();
    for (system, display_name, source_name) in SOURCE_FILES {
        let source = directory.join(source_name);
        let exists = source.is_file();
        variables.push(VariableInfo {
            system: system.to_string(),
            display_name: display_name.to_string(),
            source_name: source_name.to_string(),
            source: source.clone(),
            exists,
            active: exists && hashes_match(&source, &target),
        });
    }
    Ok(variables)
}

fn push_unique(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if let Ok(resolved) = candidate.canonicalize() {
        if resolved.is_dir() && !candidates.contains(&resolved) {
            candidates.push(resolved);
        }
    }
}

fn mounted_candidate_directories() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if cfg!(windows) {
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            push_unique(&mut candidates, PathBuf::from(root).join("EFI/refind/vars"));
        }
    } else {
        for path in [
            "/Volumes/REFIND",
            "/boot/efi",
            "/efi",
            "/boot",
            "/mnt/REFIND",
            "/media/REFIND",
        ] {
            push_unique(&mut candidates, PathBuf::from(path).join("EFI/refind/vars"));
        }
        for base in ["/Volumes", "/mnt", "/media", "/run/media"] {
            let base = PathBuf::from(base);
            let Ok(entries) = fs::read_dir(&base) else {
                continue;
            };
            for entry in entries.flatten() {
                let root = entry.path();
                push_unique(&mut candidates, root.join("EFI/refind/vars"));
                let Ok(nested) = fs::read_dir(&root) else {
                    continue;
                };
                for nested_entry in nested.flatten() {
                    push_unique(&mut candidates, nested_entry.path().join("EFI/refind/vars"));
                }
            }
        }
    }
    candidates
}

fn scan_unmounted_partition() -> Result<ScanResult, String> {
    if !cfg!(target_os = "linux") {
        return Err("未找到已挂载的 EFI/refind/vars 目录".to_string());
    }

    let output = Command::new("lsblk")
        .args([
            "--json",
            "--output",
            "PATH,TYPE,FSTYPE,LABEL,PARTLABEL,PARTTYPE,MOUNTPOINTS",
        ])
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("无法执行 lsblk: {error}"))?;
    let devices: LsblkOutput = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("无法解析 lsblk 输出: {error}"))?;

    let mut candidates = Vec::new();
    for device in devices.blockdevices {
        let Some(device_path) = device.path else {
            continue;
        };
        let Some(filesystem) = device.fstype else {
            continue;
        };
        if !FAT_FILESYSTEMS.contains(&filesystem.as_str()) {
            continue;
        }
        let mounted = device
            .mountpoints
            .unwrap_or_default()
            .iter()
            .flatten()
            .any(|mountpoint| !mountpoint.is_empty());
        if mounted {
            continue;
        }
        let label = device.label.unwrap_or_default().to_lowercase();
        let partlabel = device.partlabel.unwrap_or_default().to_lowercase();
        let parttype = device.parttype.unwrap_or_default().to_lowercase();
        let hints = format!("{label} {partlabel}");
        let priority = if hints.contains("refind") {
            0
        } else if parttype == ESP_PARTTYPE || hints.contains("efi system partition") {
            1
        } else {
            2
        };
        candidates.push((priority, device_path, filesystem));
    }
    candidates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));

    let mountpoint = std::env::temp_dir().join(format!("refind-switcher-{}", std::process::id()));
    let _ = fs::remove_dir(&mountpoint);
    fs::create_dir(&mountpoint).map_err(|error| format!("无法创建临时挂载点: {error}"))?;

    for (_, device, filesystem) in candidates {
        let result = Command::new("mount")
            .args(["-o", "ro", "-t", &filesystem])
            .arg(&device)
            .arg(&mountpoint)
            .output();
        match result {
            Ok(output) if output.status.success() => {
                let vars_dir = mountpoint.join("EFI/refind/vars");
                if vars_dir.is_dir() {
                    let resolved = vars_dir
                        .canonicalize()
                        .map_err(|error| format!("无法解析变量目录: {error}"))?;
                    let variables = inspect_variables(&resolved)
                        .map_err(|error| format!("无法读取变量目录: {error}"))?;
                    return Ok(ScanResult {
                        vars_dir: resolved,
                        active_mount: Some(ActiveMount {
                            device: PathBuf::from(device),
                            mountpoint,
                        }),
                        variables,
                    });
                }
                let _ = Command::new("umount").arg(&mountpoint).status();
            }
            Ok(output) => {
                let _ = format!(
                    "mount {device}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(_) => {}
        }
    }
    let _ = fs::remove_dir(&mountpoint);

    let root_hint = if cfg!(unix) {
        "；如分区未挂载，请以 root 运行应用"
    } else {
        ""
    };
    Err(format!(
        "已扫描未挂载的 FAT/ESP 分区，但未找到 EFI/refind/vars 目录{root_hint}"
    ))
}

fn scan_vars_directory(explicit_dir: Option<&Path>) -> Result<ScanResult, String> {
    if let Some(directory) = explicit_dir {
        if !directory.is_dir() {
            return Err(format!("rEFInd 变量目录不存在: {}", directory.display()));
        }
        let variables =
            inspect_variables(directory).map_err(|error| format!("无法读取变量目录: {error}"))?;
        return Ok(ScanResult {
            vars_dir: directory.to_path_buf(),
            active_mount: None,
            variables,
        });
    }

    let candidates = mounted_candidate_directories();
    if candidates.len() > 1 {
        let names = candidates
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "找到多个 rEFInd 变量目录，请在主窗口指定其中一个：\n{names}"
        ));
    }
    if let Some(directory) = candidates.first() {
        let variables =
            inspect_variables(directory).map_err(|error| format!("无法读取变量目录: {error}"))?;
        return Ok(ScanResult {
            vars_dir: directory.clone(),
            active_mount: None,
            variables,
        });
    }
    scan_unmounted_partition()
}

fn state_snapshot(app: &AppHandle) -> AppStatus {
    let state = app.state::<Mutex<AppState>>();
    let state = state.lock().expect("state mutex poisoned");
    AppStatus {
        vars_dir: state.vars_dir.as_deref().map(display_path),
        variables: state.variables.clone(),
        message: state.message.clone(),
        error: state.error.clone(),
    }
}

fn publish_status(app: &AppHandle, status: &AppStatus) {
    let _ = app.emit("status-changed", status);
}

fn refresh_and_update(app: &AppHandle) -> AppStatus {
    let (explicit_dir, old_mount) = {
        let state = app.state::<Mutex<AppState>>();
        let mut state = state.lock().expect("state mutex poisoned");
        (state.explicit_dir.clone(), state.active_mount.take())
    };
    if let Some(mount) = old_mount {
        let _ = mount.unmount();
    }

    let mut status = match scan_vars_directory(explicit_dir.as_deref()) {
        Ok(result) => {
            let message = result
                .variables
                .iter()
                .find(|item| item.active)
                .map(|item| format!("下次启动：{}", item.display_name))
                .unwrap_or_else(|| "已连接 rEFInd；PreviousBoot 未匹配模板".to_string());
            let status = AppStatus {
                vars_dir: Some(display_path(&result.vars_dir)),
                variables: result.variables.clone(),
                message: message.clone(),
                error: None,
            };
            let state = app.state::<Mutex<AppState>>();
            let mut state = state.lock().expect("state mutex poisoned");
            state.vars_dir = Some(result.vars_dir);
            state.active_mount = result.active_mount;
            state.variables = result.variables;
            state.message = message;
            state.error = None;
            status
        }
        Err(error) => {
            let message = "未找到可用的 rEFInd 变量目录".to_string();
            let status = AppStatus {
                vars_dir: None,
                variables: Vec::new(),
                message: message.clone(),
                error: Some(error),
            };
            let state = app.state::<Mutex<AppState>>();
            let mut state = state.lock().expect("state mutex poisoned");
            state.vars_dir = None;
            state.variables = Vec::new();
            state.message = message;
            state.error = status.error.clone();
            status
        }
    };

    if let Err(error) = update_tray_menu(app) {
        status.error.get_or_insert_with(|| error.to_string());
    }
    publish_status(app, &status);
    status
}

fn switch_and_update(app: &AppHandle, system: &str) -> AppStatus {
    let selected = SOURCE_FILES
        .iter()
        .find(|(candidate, _, _)| *candidate == system)
        .cloned();
    let Some((_system, display_name, source_name)) = selected else {
        let mut status = state_snapshot(app);
        status.error = Some(format!("未知系统: {system}"));
        return status;
    };

    let (directory, active_mount) = {
        let state = app.state::<Mutex<AppState>>();
        let state = state.lock().expect("state mutex poisoned");
        (state.vars_dir.clone(), state.active_mount.clone())
    };
    let Some(directory) = directory else {
        refresh_and_update(app);
        let mut status = state_snapshot(app);
        status.error = Some("变量目录尚未就绪".to_string());
        return status;
    };

    let result = write_with_mount(active_mount.as_ref(), || {
        let source = directory.join(source_name);
        let target = directory.join("PreviousBoot");
        if !source.is_file() {
            return Err(format!("变量文件不存在: {}", source.display()));
        }
        atomic_copy(&source, &target)
            .and_then(|_| {
                if hashes_match(&source, &target) {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "写入校验失败: {}",
                        target.display()
                    )))
                }
            })
            .map_err(|error| format!("写入失败: {error}"))
    });

    match result {
        Ok(()) => refresh_and_update(app),
        Err(error) => update_failure_status(app, &format!("切换 {display_name} 失败"), error),
    }
}

fn write_with_mount<F>(active_mount: Option<&ActiveMount>, operation: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    if let Some(mount) = active_mount.as_ref() {
        mount
            .remount("rw")
            .map_err(|error| format!("无法将分区重新挂载为可写: {error}"))?;
    }
    let result = operation();
    if let Some(mount) = active_mount.as_ref() {
        let _ = mount.remount("ro");
    }
    result
}

fn update_failure_status(app: &AppHandle, message: &str, error: String) -> AppStatus {
    let (existing_directory, existing_variables) = {
        let state = app.state::<Mutex<AppState>>();
        let state = state.lock().expect("state mutex poisoned");
        (state.vars_dir.clone(), state.variables.clone())
    };
    let status = AppStatus {
        vars_dir: existing_directory,
        variables: existing_variables,
        message: message.to_string(),
        error: Some(error),
    };
    {
        let state = app.state::<Mutex<AppState>>();
        let mut state = state.lock().expect("state mutex poisoned");
        state.message = message.to_string();
        state.error = status.error.clone();
    }
    let _ = update_tray_menu(app);
    publish_status(app, &status);
    status
}

fn temporary_target(target: &Path) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".{}.{}.{}.tmp",
            target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("target"),
            std::process::id(),
            unique
        ))
}

fn atomic_copy(source: &Path, target: &Path) -> io::Result<()> {
    let temporary = temporary_target(target);
    let result = (|| -> io::Result<()> {
        fs::copy(source, &temporary)?;
        let file = fs::OpenOptions::new().write(true).open(&temporary)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_tray_image() -> Image<'static> {
    let mut rgba = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64 {
        for x in 0..64 {
            let border = x < 3 || y < 3 || x >= 61 || y >= 61;
            let edge = (9..13).contains(&x) || (29..33).contains(&x) || (51..55).contains(&x);
            let color = if border || edge {
                [31, 38, 50, 255]
            } else if x < 29 {
                [243, 83, 37, 255]
            } else if x < 51 {
                [129, 188, 6, 255]
            } else {
                [5, 166, 240, 255]
            };
            rgba.extend_from_slice(&color);
        }
    }
    Image::new_owned(rgba, 64, 64)
}

fn update_tray_menu(app: &AppHandle) -> tauri::Result<()> {
    let (message, variables, error) = {
        let state = app.state::<Mutex<AppState>>();
        let state = state.lock().expect("state mutex poisoned");
        (
            state.message.clone(),
            state.variables.clone(),
            state.error.clone(),
        )
    };

    let menu = Menu::new(app)?;
    let status = MenuItem::with_id(app, "status", &message, false, None::<&str>)?;
    menu.append(&status)?;
    let first_separator = PredefinedMenuItem::separator(app)?;
    menu.append(&first_separator)?;

    for variable in &variables {
        let label = if variable.active {
            format!("● {}", variable.display_name)
        } else {
            variable.display_name.clone()
        };
        let item = MenuItem::with_id(
            app,
            variable.system.clone(),
            label,
            variable.exists,
            None::<&str>,
        )?;
        menu.append(&item)?;
    }
    let second_separator = PredefinedMenuItem::separator(app)?;
    menu.append(&second_separator)?;
    let refresh = MenuItem::with_id(app, "refresh", "刷新", true, None::<&str>)?;
    menu.append(&refresh)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    menu.append(&quit)?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu))?;
        let tooltip = error.unwrap_or_else(|| format!("rEFInd 启动切换器：{message}"));
        tray.set_tooltip(Some(tooltip))?;
    }
    Ok(())
}

fn spawn_menu_action(app: &AppHandle, action: &str) {
    let app = app.clone();
    let action = action.to_string();
    std::thread::spawn(move || match action.as_str() {
        "refresh" => {
            refresh_and_update(&app);
        }
        "quit" => app.exit(0),
        "linux" | "windows" | "macos" => {
            switch_and_update(&app, &action);
        }
        _ => {}
    });
}

#[tauri::command]
fn current_status(app: AppHandle) -> AppStatus {
    state_snapshot(&app)
}

#[tauri::command]
fn refresh(app: AppHandle) -> AppStatus {
    refresh_and_update(&app)
}

#[tauri::command]
fn set_vars_dir(app: AppHandle, path: Option<String>) -> AppStatus {
    let directory = path
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    {
        let state = app.state::<Mutex<AppState>>();
        let mut state = state.lock().expect("state mutex poisoned");
        state.explicit_dir = directory;
    }
    refresh_and_update(&app)
}

#[tauri::command]
fn switch_system(app: AppHandle, system: String) -> AppStatus {
    switch_and_update(&app, &system)
}

pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::<AppState>::default())
        .setup(|app| {
            let status =
                MenuItem::with_id(app.handle(), "status", "正在扫描…", false, None::<&str>)?;
            let refresh = MenuItem::with_id(app.handle(), "refresh", "刷新", true, None::<&str>)?;
            let quit = MenuItem::with_id(app.handle(), "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app.handle(), &[&status, &refresh, &quit])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(create_tray_image())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let action = event.id().as_ref().to_string();
                    spawn_menu_action(app, &action);
                })
                .build(app.handle())?;

            let app_handle = app.handle().clone();
            std::thread::spawn(move || refresh_and_update(&app_handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            current_status,
            refresh,
            set_vars_dir,
            switch_system
        ])
        .run(tauri::generate_context!())
        .expect("failed to run rEFInd Switcher");
}
