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
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const SOURCE_FILES: [(&str, &str, &str); 3] = [
    ("linux", "Linux", "PreviousBoot-linux"),
    ("windows", "Windows", "PreviousBoot-windows"),
    ("macos", "macOS", "PreviousBoot-mac"),
];
const ESP_PARTTYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const FAT_FILESYSTEMS: [&str; 6] = ["vfat", "fat", "fat12", "fat16", "fat32", "exfat"];
const SETTINGS_FILE: &str = "settings.json";
const MAIN_WINDOW_LABEL: &str = "main";

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
        mount_command("umount")
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
        let output = mount_command("mount")
            .arg("-o")
            .arg(remount_options(mode))
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

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(target_os = "linux")]
fn mount_command(program: &str) -> Command {
    if is_root() {
        Command::new(program)
    } else {
        let mut command = Command::new("pkexec");
        command.arg(program);
        command
    }
}

#[cfg(not(target_os = "linux"))]
fn mount_command(program: &str) -> Command {
    Command::new(program)
}

#[cfg(target_os = "linux")]
fn linux_user_home_by_name(username: &str) -> Option<PathBuf> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.first() != Some(&username) {
            return None;
        }
        Some(PathBuf::from(fields.get(5)?.to_string()))
    })
}

#[cfg(target_os = "linux")]
fn linux_user_home_by_uid(uid: u32) -> Option<PathBuf> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.get(2).and_then(|value| value.parse::<u32>().ok()) != Some(uid) {
            return None;
        }
        Some(PathBuf::from(fields.get(5)?.to_string()))
    })
}

#[cfg(target_os = "linux")]
fn linux_invoking_user_home() -> Option<PathBuf> {
    if let Some(username) = std::env::var_os("SUDO_USER") {
        return linux_user_home_by_name(&username.to_string_lossy());
    }
    if let Ok(uid) = std::env::var("PKEXEC_UID") {
        return uid.parse::<u32>().ok().and_then(linux_user_home_by_uid);
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_mount_user_ids() -> Option<(u32, u32)> {
    if is_root() {
        return linux_invoking_user_home().as_deref().and_then(linux_user_ids_from_home);
    }
    Some(unsafe { (libc::getuid(), libc::getgid()) })
}

#[cfg(target_os = "linux")]
fn mount_options(mode: &str) -> String {
    match linux_mount_user_ids() {
        Some((uid, gid)) => format!("{mode},uid={uid},gid={gid}"),
        None => mode.to_string(),
    }
}

#[cfg(target_os = "linux")]
fn remount_options(mode: &str) -> String {
    format!("remount,{}", mount_options(mode))
}

#[cfg(not(target_os = "linux"))]
fn remount_options(mode: &str) -> String {
    format!("remount,{mode}")
}

#[cfg(target_os = "linux")]
fn linux_user_ids_from_home(home: &Path) -> Option<(u32, u32)> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.get(5) != Some(&home.to_string_lossy().as_ref()) {
            return None;
        }
        Some((fields.get(2)?.parse().ok()?, fields.get(3)?.parse().ok()?))
    })
}

#[cfg(not(target_os = "linux"))]
fn mount_options(mode: &str) -> String {
    mode.to_string()
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CloseAction {
    Tray,
    Quit,
}

impl CloseAction {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "tray" => Some(Self::Tray),
            "quit" => Some(Self::Quit),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    autostart: bool,
    start_in_tray: bool,
    close_action: CloseAction,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: false,
            start_in_tray: false,
            close_action: CloseAction::Tray,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutostartState {
    Enabled,
    RequiresApproval,
    Disabled,
    Unavailable,
}

impl AutostartState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::RequiresApproval => "requires-approval",
            Self::Disabled => "disabled",
            Self::Unavailable => "unavailable",
        }
    }

    fn is_on(self) -> bool {
        matches!(self, Self::Enabled | Self::RequiresApproval)
    }

    fn hint(self) -> Option<&'static str> {
        match self {
            Self::RequiresApproval => {
                Some("已注册登录项，请在「系统设置 → 通用 → 登录项与扩展」中允许本应用")
            }
            Self::Unavailable => Some(
                "无法通过系统登录项接口管理开机启动（需以 .app 形式运行），可在「系统设置 → 通用 → 登录项与扩展」中手动添加",
            ),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    #[serde(flatten)]
    settings: Settings,
    autostart_state: &'static str,
    autostart_hint: Option<String>,
}

impl SettingsView {
    fn new(settings: Settings, state: AutostartState) -> Self {
        Self {
            settings,
            autostart_state: state.as_str(),
            autostart_hint: state.hint().map(str::to_string),
        }
    }
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
        let result = mount_command("mount")
            .args(["-o", &mount_options("ro"), "-t", &filesystem])
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
                let _ = mount_command("umount").arg(&mountpoint).status();
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

    let root_hint = if cfg!(target_os = "linux") {
        "；应用已尝试通过 pkexec 临时提权挂载，若取消授权或 pkexec 不可用，请手动挂载后刷新"
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
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    menu.append(&show)?;
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

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("无法定位应用配置目录: {error}"))?;
    Ok(directory.join(SETTINGS_FILE))
}

fn read_settings_file(app: &AppHandle) -> Settings {
    settings_path(app)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn write_settings_file(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    let directory = path
        .parent()
        .ok_or_else(|| "设置文件路径无效".to_string())?;
    fs::create_dir_all(directory).map_err(|error| format!("无法创建配置目录: {error}"))?;
    let contents = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("无法序列化设置: {error}"))?;
    let temporary = temporary_target(&path);
    let result = (|| -> io::Result<()> {
        fs::write(&temporary, contents.as_bytes())?;
        fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("无法保存设置: {error}"))
}

fn stored_settings(app: &AppHandle) -> Settings {
    let state = app.state::<Mutex<Settings>>();
    let settings = state.lock().expect("settings mutex poisoned").clone();
    settings
}

fn save_settings(app: &AppHandle, settings: &Settings) -> Result<Settings, String> {
    write_settings_file(app, settings)?;
    let state = app.state::<Mutex<Settings>>();
    *state.lock().expect("settings mutex poisoned") = settings.clone();
    Ok(settings.clone())
}

/// macOS 13+ 使用系统登录项（SMAppService），开机启动会出现在
/// 「系统设置 → 通用 → 登录项与扩展 → 登入时打开」里。
#[cfg(target_os = "macos")]
mod login_item {
    use objc2::rc::Retained;
    use objc2::runtime::AnyClass;
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    use super::AutostartState;

    pub fn is_supported() -> bool {
        AnyClass::get(c"SMAppService").is_some()
    }

    fn main_app() -> Result<Retained<SMAppService>, String> {
        if !is_supported() {
            return Err("当前系统版本不支持登录项（需要 macOS 13 及以上）".to_string());
        }
        Ok(unsafe { SMAppService::mainAppService() })
    }

    pub fn state() -> AutostartState {
        let Ok(service) = main_app() else {
            eprintln!("SMAppService 类不可用（macOS 低于 13）");
            return AutostartState::Unavailable;
        };
        let status = unsafe { service.status() };
        if status == SMAppServiceStatus::Enabled {
            AutostartState::Enabled
        } else if status == SMAppServiceStatus::RequiresApproval {
            AutostartState::RequiresApproval
        } else if status == SMAppServiceStatus::NotRegistered {
            AutostartState::Disabled
        } else {
            AutostartState::Unavailable
        }
    }

    pub fn set(enabled: bool) -> Result<AutostartState, String> {
        let service = main_app()?;
        let result = unsafe {
            if enabled {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        let state = state();
        match result {
            Ok(()) => Ok(state),
            Err(error) => {
                let already_there = if enabled {
                    state.is_on()
                } else {
                    state == AutostartState::Disabled
                };
                if already_there {
                    return Ok(state);
                }
                let action = if enabled { "开启" } else { "关闭" };
                Err(format!(
                    "无法{action}开机启动: {}",
                    error.localizedDescription()
                ))
            }
        }
    }

    pub fn open_system_settings() -> Result<(), String> {
        main_app()?;
        unsafe { SMAppService::openSystemSettingsLoginItems() };
        Ok(())
    }
}

fn legacy_autostart_state(app: &AppHandle) -> AutostartState {
    match app.autolaunch().is_enabled() {
        Ok(true) => AutostartState::Enabled,
        Ok(false) => AutostartState::Disabled,
        Err(_) => AutostartState::Unavailable,
    }
}

fn legacy_autostart_set(app: &AppHandle, enabled: bool) -> Result<AutostartState, String> {
    let manager = app.autolaunch();
    let action = if enabled { "开启" } else { "关闭" };
    if enabled {
        manager
            .enable()
            .map_err(|error| format!("无法{action}开机启动: {error}"))?;
    } else {
        manager
            .disable()
            .map_err(|error| format!("无法{action}开机启动: {error}"))?;
    }
    Ok(legacy_autostart_state(app))
}

#[cfg(target_os = "macos")]
fn autostart_state(app: &AppHandle) -> AutostartState {
    if login_item::is_supported() {
        login_item::state()
    } else {
        legacy_autostart_state(app)
    }
}

#[cfg(target_os = "linux")]
fn autostart_state(app: &AppHandle) -> AutostartState {
    linux_autostart_state(app)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn autostart_state(app: &AppHandle) -> AutostartState {
    legacy_autostart_state(app)
}

#[cfg(target_os = "macos")]
fn autostart_set(app: &AppHandle, enabled: bool) -> Result<AutostartState, String> {
    if login_item::is_supported() {
        login_item::set(enabled)
    } else {
        legacy_autostart_set(app, enabled)
    }
}

#[cfg(target_os = "linux")]
fn autostart_set(app: &AppHandle, enabled: bool) -> Result<AutostartState, String> {
    linux_autostart_set(app, enabled)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn autostart_set(app: &AppHandle, enabled: bool) -> Result<AutostartState, String> {
    legacy_autostart_set(app, enabled)
}
#[cfg(target_os = "linux")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn linux_autostart_file(app: &AppHandle) -> Result<PathBuf, String> {
    let invoking_user = if is_root() {
        linux_invoking_user_home()
    } else {
        Some(
            app.path()
                .home_dir()
                .map_err(|error| format!("无法定位用户主目录: {error}"))?,
        )
    };
    let home = invoking_user.ok_or_else(|| "无法识别当前登录用户".to_string())?;
    let directory = home.join(".config").join("autostart");
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建自启动目录: {error}"))?;
    let name = app.package_info().name.replace([' ', '/'], "-");
    Ok(directory.join(format!("{name}.desktop")))
}

#[cfg(target_os = "linux")]
fn linux_autostart_state(app: &AppHandle) -> AutostartState {
    match linux_autostart_file(app) {
        Ok(path) if path.exists() => AutostartState::Enabled,
        Ok(_) => AutostartState::Disabled,
        Err(_) => AutostartState::Unavailable,
    }
}

#[cfg(target_os = "linux")]
fn linux_autostart_set(app: &AppHandle, enabled: bool) -> Result<AutostartState, String> {
    let path = linux_autostart_file(app)?;
    if path.exists() {
        let stale = fs::read_to_string(&path)
            .map(|contents| {
                !contents.contains("rEFInd Switcher") && !contents.contains("refind-switcher")
            })
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
    if !enabled {
        if path.exists() {
            fs::remove_file(&path).map_err(|error| format!("无法关闭开机启动: {error}"))?;
        }
        return Ok(AutostartState::Disabled);
    }

    let executable = std::env::current_exe()
        .map_err(|error| format!("无法定位当前程序: {error}"))?
        .canonicalize()
        .map_err(|error| format!("无法解析当前程序路径: {error}"))?;
    let appimage = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let launch_path = appimage.unwrap_or(executable);
    let content = format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName={}\nComment=rEFInd Switcher\nExec={} --from-autostart\nStartupNotify=false\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        app.package_info().name,
        shell_quote(&launch_path.to_string_lossy())
    );
    fs::write(&path, content).map_err(|error| format!("无法写入用户自启动项: {error}"))?;
    Ok(AutostartState::Enabled)
}

/// 早期版本在 macOS 上写入 ~/Library/LaunchAgents 实现开机启动，
/// 改用系统登录项后需要清理，否则登录时会被重复拉起。
#[cfg(target_os = "macos")]
fn remove_legacy_launch_agents(app: &AppHandle) {
    let Ok(home) = app.path().home_dir() else {
        return;
    };
    let directory = home.join("Library").join("LaunchAgents");
    let name = app.package_info().name.clone();
    let identifier = app.config().identifier.clone();
    let candidates = [
        format!("{name}.plist"),
        format!("{}.plist", name.replace(' ', "-")),
        format!("{identifier}.plist"),
    ];
    for file in candidates {
        let path = directory.join(&file);
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if !contents.contains("refind-switcher") && !contents.contains(&name) {
            continue;
        }
        let _ = Command::new("launchctl")
            .arg("unload")
            .arg("--")
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_file(&path);
    }
}

fn view_settings(app: &AppHandle) -> SettingsView {
    let state = autostart_state(app);
    let mut settings = stored_settings(app);
    if settings.autostart != state.is_on() {
        settings.autostart = state.is_on();
        settings = save_settings(app, &settings).unwrap_or(settings);
    }
    SettingsView::new(settings, state)
}

#[cfg(target_os = "macos")]
fn cleanup_legacy_autostart(app: &AppHandle) {
    if login_item::is_supported() {
        remove_legacy_launch_agents(app);
    }
}

#[cfg(not(target_os = "macos"))]
fn cleanup_legacy_autostart(_app: &AppHandle) {}

/// 应用被更新或移动后，登录项可能失效；启动时按已保存的设置自愈一次。
fn startup_autostart_sync(app: &AppHandle) {
    cleanup_legacy_autostart(app);
    let desired = stored_settings(app).autostart;
    let state = autostart_state(app);
    eprintln!(
        "启动时检查开机启动：设置={desired}，系统状态={}",
        state.as_str()
    );
    if desired && state == AutostartState::Disabled {
        match autostart_set(app, true) {
            Ok(new_state) => eprintln!("已重新注册开机启动登录项，状态: {}", new_state.as_str()),
            Err(error) => eprintln!("重新注册开机启动登录项失败: {error}"),
        }
    }
}

fn show_main_window(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
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

#[tauri::command]
fn get_settings(app: AppHandle) -> SettingsView {
    view_settings(&app)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<SettingsView, String> {
    let state = autostart_set(&app, enabled)?;
    let mut settings = stored_settings(&app);
    settings.autostart = state.is_on();
    let settings = save_settings(&app, &settings)?;
    Ok(SettingsView::new(settings, state))
}

#[tauri::command]
fn open_login_items_settings(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = &app;
        login_item::open_system_settings()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &app;
        Err("仅 macOS 支持打开登录项设置".to_string())
    }
}

#[tauri::command]
fn set_close_action(app: AppHandle, action: String) -> Result<SettingsView, String> {
    let Some(close_action) = CloseAction::parse(action.trim()) else {
        return Err(format!("未知的关闭行为: {action}"));
    };
    let mut settings = stored_settings(&app);
    settings.close_action = close_action;
    save_settings(&app, &settings)?;
    Ok(view_settings(&app))
}

#[tauri::command]
fn set_start_in_tray(app: AppHandle, enabled: bool) -> Result<SettingsView, String> {
    let mut settings = stored_settings(&app);
    settings.start_in_tray = enabled;
    save_settings(&app, &settings)?;
    Ok(view_settings(&app))
}

pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::<AppState>::default())
        .manage(Mutex::<Settings>::default())
        // macOS 13+ 走系统登录项（见 login_item），这里的 LaunchAgent 方式仅作为
        // 旧版 macOS 以及 Windows / Linux 的实现。
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .on_window_event(|window, event| {
            let tauri::WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            if window.label() != MAIN_WINDOW_LABEL {
                return;
            }
            let app = window.app_handle().clone();
            api.prevent_close();
            if stored_settings(&app).close_action == CloseAction::Quit {
                app.exit(0);
                return;
            }
            let _ = window.hide();
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
        })
        .setup(|app| {
            let settings = read_settings_file(app.handle());
            {
                let state = app.state::<Mutex<Settings>>();
                *state.lock().expect("settings mutex poisoned") = settings;
            }
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            startup_autostart_sync(app.handle());
            view_settings(app.handle());
            if stored_settings(app.handle()).start_in_tray {
                if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                    let _ = window.hide();
                }
                #[cfg(target_os = "macos")]
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            let status =
                MenuItem::with_id(app.handle(), "status", "正在扫描…", false, None::<&str>)?;
            let show = MenuItem::with_id(app.handle(), "show", "显示主窗口", true, None::<&str>)?;
            let refresh = MenuItem::with_id(app.handle(), "refresh", "刷新", true, None::<&str>)?;
            let quit = MenuItem::with_id(app.handle(), "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app.handle(), &[&status, &show, &refresh, &quit])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(create_tray_image())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let action = event.id().as_ref().to_string();
                    if action == "show" {
                        show_main_window(app);
                        return;
                    }
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
            switch_system,
            get_settings,
            set_autostart,
            open_login_items_settings,
            set_close_action,
            set_start_in_tray
        ])
        .run(tauri::generate_context!())
        .expect("failed to run rEFInd Switcher");
}
