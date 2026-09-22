use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
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
/// 设置变更事件：托盘菜单改动后推给主窗口，保证两边一致。
const SETTINGS_CHANGED_EVENT: &str = "settings-changed";
/// 重启结果事件：把失败原因回传给主窗口。
const RESTART_RESULT_EVENT: &str = "restart-result";

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
    owned: bool,
}

impl ActiveMount {
    fn unmount(self) -> io::Result<()> {
        if !self.owned {
            return Ok(());
        }
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
        return linux_invoking_user_home()
            .as_deref()
            .and_then(linux_user_ids_from_home);
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
    temporary_mount: bool,
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
    hide_from_dock_taskbar: bool,
    close_action: CloseAction,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: false,
            start_in_tray: false,
            hide_from_dock_taskbar: true,
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

#[cfg(target_os = "macos")]
fn app_bundle_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.canonicalize().ok())
        .and_then(|executable| executable.ancestors().nth(3).map(Path::to_path_buf))
        .filter(|bundle| {
            bundle
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
                && bundle.join("Contents").is_dir()
        })
}

#[cfg(target_os = "macos")]
fn running_from_app_bundle() -> bool {
    app_bundle_path().is_some()
}

/// macOS 开机启动的实现方式。
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutostartBackend {
    /// macOS 13+ 且应用已签名：走系统登录项接口（SMAppService），
    /// 条目由用户在「系统设置 → 通用 → 登录项与扩展」中管理。
    LoginItem,
    /// 未签名 / 未以 .app 运行 / macOS 12 及以下：退回 ~/Library/LaunchAgents。
    ///
    /// 系统登录项接口要求应用带有效签名，未签名的 bundle 调用
    /// `registerAndReturnError()` 只会拿到
    /// `SMAppServiceErrorDomain code=1 "Operation not permitted"`，
    /// 因此这种情况必须用兼容方式，否则开关点了没有任何效果。
    LaunchAgent,
}

#[cfg(target_os = "macos")]
fn autostart_backend_for(supported: bool, in_bundle: bool, signed: bool) -> AutostartBackend {
    if supported && in_bundle && signed {
        AutostartBackend::LoginItem
    } else {
        AutostartBackend::LaunchAgent
    }
}

#[cfg(target_os = "macos")]
fn autostart_backend() -> AutostartBackend {
    autostart_backend_for(
        login_item::is_supported(),
        running_from_app_bundle(),
        app_bundle_is_signed(),
    )
}

/// 当前 .app 是否带有有效代码签名（ad-hoc 签名同样算有效）。
///
/// SMAppService 只接受已签名的应用，所以这里实际跑一次 `codesign --verify`
/// 探测，而不是要求必须是 Developer ID 证书。
#[cfg(target_os = "macos")]
fn app_bundle_is_signed() -> bool {
    static SIGNED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SIGNED.get_or_init(|| {
        let Some(bundle) = app_bundle_path() else {
            return false;
        };
        Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg(&bundle)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// 系统登录项注册失败时的提示。已签名的应用也可能因为系统登录项数据库
/// 状态异常而注册失败，这时会退回 LaunchAgent，需要把原因告诉用户。
#[cfg(target_os = "macos")]
fn login_item_failure() -> Option<String> {
    static NOTICE: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    NOTICE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("autostart notice mutex poisoned")
        .clone()
}

#[cfg(target_os = "macos")]
fn note_login_item_failure(error: &str) {
    static NOTICE: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    *NOTICE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("autostart notice mutex poisoned") = Some(error.to_string());
}

#[cfg(target_os = "macos")]
fn clear_login_item_failure() {
    static NOTICE: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    *NOTICE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("autostart notice mutex poisoned") = None;
}

/// 兼容方式（LaunchAgent）生效时说明为什么没用系统登录项。
#[cfg(target_os = "macos")]
fn launch_agent_hint() -> String {
    if !login_item::is_supported() {
        return "当前系统版本低于 macOS 13，已改用兼容方式（~/Library/LaunchAgents）实现开机启动"
            .to_string();
    }
    if !running_from_app_bundle() {
        return "当前未以 .app 形式运行，已改用兼容方式（~/Library/LaunchAgents）实现开机启动"
            .to_string();
    }
    "当前应用没有代码签名，系统登录项接口不可用，已改用兼容方式（~/Library/LaunchAgents）实现开机启动；使用签名版本后可在「系统设置 → 通用 → 登录项与扩展」中管理"
        .to_string()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    #[serde(flatten)]
    settings: Settings,
    autostart_state: &'static str,
    autostart_hint: Option<String>,
}

/// 界面提示：先说明系统状态，再说明实际用的是哪种开机启动实现。
#[cfg(not(target_os = "macos"))]
fn autostart_hint(state: AutostartState) -> Option<String> {
    state.hint().map(str::to_string)
}

#[cfg(target_os = "macos")]
fn autostart_hint(state: AutostartState) -> Option<String> {
    macos_autostart_hint(
        state,
        autostart_backend(),
        login_item_failure(),
        launch_agent_hint(),
    )
}

/// 提示语的纯逻辑部分，和运行环境解耦，便于单测。
#[cfg(target_os = "macos")]
fn macos_autostart_hint(
    state: AutostartState,
    backend: AutostartBackend,
    login_item_error: Option<String>,
    compatibility_hint: String,
) -> Option<String> {
    // 需要用户在系统设置里勾选时，优先给这条可操作的提示。
    if state == AutostartState::RequiresApproval {
        return state.hint().map(str::to_string);
    }
    if let Some(error) = login_item_error {
        return Some(format!(
            "系统登录项注册失败（{error}），已改用兼容方式（~/Library/LaunchAgents）实现开机启动"
        ));
    }
    // 已经开启但不是通过系统登录项实现的，说明现在用的是兼容方式。
    if state.is_on() && backend == AutostartBackend::LaunchAgent {
        return Some(compatibility_hint);
    }
    state.hint().map(str::to_string)
}

impl SettingsView {
    fn new(settings: Settings, state: AutostartState) -> Self {
        Self {
            settings,
            autostart_state: state.as_str(),
            autostart_hint: autostart_hint(state),
        }
    }
}

/// 重启结果，用于把失败原因回传给主窗口。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RestartResult {
    ok: bool,
    message: String,
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

#[derive(Clone)]
struct MountedScanCandidate {
    vars_dir: PathBuf,
    active_mount: Option<ActiveMount>,
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

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(path: &str) -> PathBuf {
    let mut decoded = String::with_capacity(path.len());
    let mut characters = path.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let mut escape = String::new();
            for _ in 0..3 {
                if let Some(digit) = characters.next() {
                    escape.push(digit);
                }
            }
            if let Ok(byte) = u8::from_str_radix(&escape, 8) {
                decoded.push(byte as char);
                continue;
            }
            decoded.push(character);
            decoded.push_str(&escape);
        } else {
            decoded.push(character);
        }
    }
    PathBuf::from(decoded)
}

#[cfg(target_os = "linux")]
fn is_owned_mountpoint(mountpoint: &Path) -> bool {
    let temp_dir = std::env::temp_dir();
    mountpoint.starts_with(&temp_dir)
        && mountpoint
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("refind-switcher-"))
}

#[cfg(target_os = "linux")]
fn mounted_scan_candidates() -> Vec<MountedScanCandidate> {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for line in mountinfo.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(mountpoint) = fields.get(4) else {
            continue;
        };
        let Some(options) = fields.get(5) else {
            continue;
        };
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let Some(fstype) = fields.get(separator + 1) else {
            continue;
        };
        let Some(device) = fields.get(separator + 2) else {
            continue;
        };
        if !FAT_FILESYSTEMS.contains(fstype) {
            continue;
        }

        let mountpoint = decode_mountinfo_path(mountpoint);
        let vars_dir = mountpoint.join("EFI/refind/vars");
        let Ok(resolved) = vars_dir.canonicalize() else {
            continue;
        };
        if !resolved.is_dir()
            || candidates
                .iter()
                .any(|candidate: &MountedScanCandidate| candidate.vars_dir == resolved)
        {
            continue;
        }

        let read_only = options.split(',').any(|option| option == "ro");
        let owned = is_owned_mountpoint(&mountpoint);
        let active_mount = if owned || read_only {
            Some(ActiveMount {
                device: decode_mountinfo_path(device),
                mountpoint,
                owned,
            })
        } else {
            None
        };
        candidates.push(MountedScanCandidate {
            vars_dir: resolved,
            active_mount,
        });
    }
    candidates
}

#[cfg(not(target_os = "linux"))]
fn mounted_scan_candidates() -> Vec<MountedScanCandidate> {
    mounted_candidate_directories()
        .into_iter()
        .map(|vars_dir| MountedScanCandidate {
            vars_dir,
            active_mount: None,
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn active_mount_for_directory(directory: &Path, resolved_directory: &Path) -> Option<ActiveMount> {
    mounted_scan_candidates()
        .into_iter()
        .filter_map(|candidate| candidate.active_mount)
        .find(|mount| {
            resolved_directory.starts_with(&mount.mountpoint)
                || directory.starts_with(&mount.mountpoint)
        })
}

#[cfg(not(target_os = "linux"))]
fn active_mount_for_directory(
    _directory: &Path,
    _resolved_directory: &Path,
) -> Option<ActiveMount> {
    None
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
                            owned: true,
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
        "；应用已尝试通过 pkexec 临时提权挂载，若取消授权或 pkexec 不可用，请手动挂载后再扫描"
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
        let resolved_directory = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.to_path_buf());
        let variables = inspect_variables(&resolved_directory)
            .map_err(|error| format!("无法读取变量目录: {error}"))?;
        return Ok(ScanResult {
            vars_dir: resolved_directory.clone(),
            active_mount: active_mount_for_directory(directory, &resolved_directory),
            variables,
        });
    }

    let mut candidates = mounted_scan_candidates();
    candidates.sort_by_key(|candidate| {
        candidate
            .active_mount
            .as_ref()
            .map(|mount| !mount.owned)
            .unwrap_or(true)
    });
    if candidates.len() > 1 {
        let names = candidates
            .iter()
            .map(|candidate| format!("  {}", candidate.vars_dir.display()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "找到多个 rEFInd 变量目录，请在主窗口指定其中一个：\n{names}"
        ));
    }
    if let Some(directory) = candidates.first() {
        let variables = inspect_variables(&directory.vars_dir)
            .map_err(|error| format!("无法读取变量目录: {error}"))?;
        return Ok(ScanResult {
            vars_dir: directory.vars_dir.clone(),
            active_mount: directory.active_mount.clone(),
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
        temporary_mount: state.active_mount.as_ref().is_some_and(|mount| mount.owned),
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
                temporary_mount: result
                    .active_mount
                    .as_ref()
                    .is_some_and(|mount| mount.owned),
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
                temporary_mount: false,
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
        status.temporary_mount = {
            let state = app.state::<Mutex<AppState>>();
            let state = state.lock().expect("state mutex poisoned");
            state.active_mount.as_ref().is_some_and(|mount| mount.owned)
        };
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
        temporary_mount: {
            let state = app.state::<Mutex<AppState>>();
            let state = state.lock().expect("state mutex poisoned");
            state.active_mount.as_ref().is_some_and(|mount| mount.owned)
        },
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
    tauri::include_image!("icons/icon.png")
}

fn update_tray_menu(app: &AppHandle) -> tauri::Result<()> {
    let (message, temporary_mount, settings, variables, error) = {
        let state = app.state::<Mutex<AppState>>();
        let state = state.lock().expect("state mutex poisoned");
        (
            state.message.clone(),
            state.active_mount.as_ref().is_some_and(|mount| mount.owned),
            stored_settings(app),
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
    if temporary_mount {
        let unmount =
            MenuItem::with_id(app, "unmount-temporary", "卸载临时挂载", true, None::<&str>)?;
        menu.append(&unmount)?;
    }
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    menu.append(&show)?;
    let settings_separator = PredefinedMenuItem::separator(app)?;
    menu.append(&settings_separator)?;
    let settings_label = MenuItem::with_id(app, "settings-label", "应用设置", false, None::<&str>)?;
    menu.append(&settings_label)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "setting-autostart",
        "开机启动",
        true,
        settings.autostart,
        None::<&str>,
    )?;
    menu.append(&autostart)?;
    let start_in_tray = CheckMenuItem::with_id(
        app,
        "setting-start-in-tray",
        "启动时隐藏主界面",
        true,
        settings.start_in_tray,
        None::<&str>,
    )?;
    menu.append(&start_in_tray)?;
    let hide_from_dock_taskbar = CheckMenuItem::with_id(
        app,
        "setting-hide-from-dock-taskbar",
        "隐藏程序（任务栏/Dock）",
        true,
        settings.hide_from_dock_taskbar,
        None::<&str>,
    )?;
    menu.append(&hide_from_dock_taskbar)?;
    let close_action_menu = Submenu::with_id(app, "setting-close-action", "点击窗口 ❌ 时", true)?;
    for (action, label) in [
        (CloseAction::Tray, "最小化到托盘"),
        (CloseAction::Quit, "退出程序"),
    ] {
        let item = CheckMenuItem::with_id(
            app,
            format!("setting-close-action:{action:?}").to_lowercase(),
            label,
            true,
            settings.close_action == action,
            None::<&str>,
        )?;
        close_action_menu.append(&item)?;
    }
    menu.append(&close_action_menu)?;
    let refresh = MenuItem::with_id(app, "refresh", "扫描", true, None::<&str>)?;
    menu.append(&refresh)?;
    let restart = MenuItem::with_id(app, "restart", "重启系统", true, None::<&str>)?;
    menu.append(&restart)?;
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
        "unmount-temporary" => {
            unmount_temporary(app);
        }
        "setting-autostart" => toggle_autostart_setting(&app),
        action if action.starts_with("setting-") && action.ends_with(":true") => {
            update_tray_setting(&app, action.strip_suffix(":true").unwrap(), true);
        }
        action if action.starts_with("setting-") && action.ends_with(":false") => {
            update_tray_setting(&app, action.strip_suffix(":false").unwrap(), false);
        }
        action if action.starts_with("setting-close-action:") => {
            update_close_action_setting(
                &app,
                action.strip_prefix("setting-close-action:").unwrap(),
            );
        }
        "quit" => app.exit(0),
        "restart" => spawn_restart(&app),
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
        // SMAppServiceStatus 枚举: 0=NotRegistered, 1=Enabled, 2=RequiresApproval,
        // 3=NotFound。应用被更新或移动后，旧注册会短暂返回 NotFound(3)；
        // 不能把它当作 Unavailable（会误导为“未以 .app 运行”），
        // 应视为未注册，让注册流程/启动自愈重新注册。
        if status == SMAppServiceStatus::Enabled {
            AutostartState::Enabled
        } else if status == SMAppServiceStatus::RequiresApproval {
            AutostartState::RequiresApproval
        } else if status == SMAppServiceStatus::NotRegistered
            || status == SMAppServiceStatus::NotFound
        {
            AutostartState::Disabled
        } else {
            AutostartState::Unavailable
        }
    }

    pub fn set(enabled: bool) -> Result<AutostartState, String> {
        let service = main_app()?;
        let mut result = unsafe {
            if enabled {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        // 应用被更新/替换后，旧注册可能处于失效状态，直接注册会失败；
        // 先注销失效的旧注册再重试一次（Apple 推荐的恢复方式）。
        if result.is_err() && enabled {
            let _ = unsafe { service.unregisterAndReturnError() };
            result = unsafe { service.registerAndReturnError() };
        }
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
                    "无法{action}开机启动: {} (code={})",
                    error.localizedDescription(),
                    error.code()
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

#[cfg(target_os = "macos")]
fn macos_autostart_state(app: &AppHandle) -> AutostartState {
    match autostart_backend() {
        AutostartBackend::LoginItem => login_item::state(),
        AutostartBackend::LaunchAgent => legacy_autostart_state(app),
    }
}

#[cfg(target_os = "macos")]
fn macos_autostart_set(app: &AppHandle, enabled: bool) -> Result<AutostartState, String> {
    match autostart_backend() {
        AutostartBackend::LoginItem => match login_item::set(enabled) {
            Ok(state) => {
                clear_login_item_failure();
                Ok(state)
            }
            // 已签名的应用也可能因为系统登录项数据库状态异常而注册失败，
            // 这时退回 LaunchAgent，保证开机启动开关依然生效。
            Err(error) => {
                eprintln!("系统登录项接口调用失败，改用兼容方式: {error}");
                note_login_item_failure(&error);
                legacy_autostart_set(app, enabled)
            }
        },
        AutostartBackend::LaunchAgent => legacy_autostart_set(app, enabled),
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
    macos_autostart_state(app)
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
    macos_autostart_set(app, enabled)
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

/// 设置变更后统一入口：重建托盘菜单并通知主窗口。
///
/// 主窗口和托盘菜单是两份独立的 UI，任何一边改了设置都必须走这里，
/// 否则另一边会一直显示旧状态（勾选框和系统真实状态不一致）。
fn publish_settings(app: &AppHandle) -> SettingsView {
    let view = view_settings(app);
    if let Err(error) = update_tray_menu(app) {
        eprintln!("刷新托盘菜单失败: {error}");
    }
    // 记一条日志，方便排查“主窗口/托盘显示不一致”这类问题。
    eprintln!(
        "设置已同步：autostart={} start_in_tray={} hide_from_dock_taskbar={} close_action={:?}",
        view.settings.autostart,
        view.settings.start_in_tray,
        view.settings.hide_from_dock_taskbar,
        view.settings.close_action
    );
    if let Err(error) = app.emit(SETTINGS_CHANGED_EVENT, &view) {
        eprintln!("通知主窗口设置变更失败: {error}");
    }
    view
}

#[cfg(target_os = "macos")]
fn cleanup_legacy_autostart(app: &AppHandle) {
    // 只有系统登录项真正生效时才清理 LaunchAgent 文件，否则会把兼容方式
    // （未签名 / 未以 .app 运行）下正在使用的自启动项一起删掉。
    if autostart_backend() == AutostartBackend::LoginItem
        && login_item::state() == AutostartState::Enabled
    {
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
    if !stored_settings(app).hide_from_dock_taskbar {
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    }
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn unmount_temporary(app: AppHandle) -> AppStatus {
    let mount = {
        let state = app.state::<Mutex<AppState>>();
        let mut state = state.lock().expect("state mutex poisoned");
        state
            .active_mount
            .take_if(|mount| mount.owned)
            .map(|mount| mount.clone())
    };

    let Some(mount) = mount else {
        let mut status = state_snapshot(&app);
        status.message = "没有应用创建的临时挂载".to_string();
        status.error = None;
        publish_status(&app, &status);
        return status;
    };

    let mountpoint = mount.mountpoint.clone();

    let status = match mount.clone().unmount() {
        Ok(()) => {
            let state = app.state::<Mutex<AppState>>();
            let mut state = state.lock().expect("state mutex poisoned");
            state.vars_dir = None;
            state.variables = Vec::new();
            state.message = "已卸载应用创建的临时挂载".to_string();
            state.error = None;
            AppStatus {
                vars_dir: None,
                temporary_mount: false,
                variables: Vec::new(),
                message: state.message.clone(),
                error: None,
            }
        }
        Err(error) => {
            {
                let state = app.state::<Mutex<AppState>>();
                let mut state = state.lock().expect("state mutex poisoned");
                state.active_mount = Some(mount);
            }
            let mut status = state_snapshot(&app);
            status.message = "卸载临时挂载失败".to_string();
            status.error = Some(format!("无法卸载 {}: {error}", mountpoint.display()));
            status
        }
    };
    update_tray_menu(&app)
        .map_err(|error| error.to_string())
        .ok();
    publish_status(&app, &status);
    status
}

fn toggle_autostart_setting(app: &AppHandle) {
    let enabled = !stored_settings(app).autostart;
    match autostart_set(app, enabled).and_then(|state| {
        let mut settings = stored_settings(app);
        settings.autostart = state.is_on();
        save_settings(app, &settings)
    }) {
        Ok(_) => {
            publish_settings(app);
        }
        Err(error) => {
            // 失败时也要同步一次：让主窗口看到系统里的真实状态和提示。
            publish_settings(app);
            eprintln!("切换开机启动失败: {error}");
        }
    }
}

fn update_tray_setting(app: &AppHandle, action: &str, enabled: bool) {
    let mut settings = stored_settings(app);
    match action {
        "setting-start-in-tray" => settings.start_in_tray = enabled,
        "setting-hide-from-dock-taskbar" => settings.hide_from_dock_taskbar = enabled,
        _ => return,
    }
    let apply_result = match action {
        "setting-hide-from-dock-taskbar" => apply_dock_taskbar_visibility(app, enabled),
        _ => Ok(()),
    };
    match save_settings(app, &settings).and_then(|_| apply_result) {
        Ok(()) => {
            publish_settings(app);
        }
        Err(error) => {
            publish_settings(app);
            eprintln!("切换应用设置失败: {error}");
        }
    }
}

fn update_close_action_setting(app: &AppHandle, action: &str) {
    let Some(close_action) = CloseAction::parse(action.trim()) else {
        return;
    };
    let mut settings = stored_settings(app);
    settings.close_action = close_action;
    if save_settings(app, &settings).is_ok() {
        publish_settings(app);
    }
}

/// 一次重启尝试的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
enum RestartAttemptOutcome {
    /// 命令执行成功，系统即将重启。
    Success,
    /// 系统里没有这个命令，可以换下一个候选。
    Missing(String),
    /// 命令存在但执行失败，通常是权限不足或用户取消了授权弹窗。
    Failed(String),
}

impl RestartAttemptOutcome {
    fn describe(&self) -> String {
        match self {
            Self::Success => "已提交重启".to_string(),
            Self::Missing(reason) | Self::Failed(reason) => reason.clone(),
        }
    }
}

/// 一条尝试失败之后该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartStep {
    /// 重启已提交，结束。
    Done,
    /// 换下一个候选命令（例如发行版里没有 systemctl）。
    Next,
    /// 权限不足，用 pkexec 提权重试同一条命令。
    Escalate,
    /// 停手：命令能跑但失败了，多半是用户取消了授权，不能换个方式照样重启。
    Stop,
}

fn next_restart_step(outcome: &RestartAttemptOutcome, can_escalate: bool) -> RestartStep {
    match outcome {
        RestartAttemptOutcome::Success => RestartStep::Done,
        RestartAttemptOutcome::Missing(_) => RestartStep::Next,
        RestartAttemptOutcome::Failed(_) if can_escalate => RestartStep::Escalate,
        RestartAttemptOutcome::Failed(_) => RestartStep::Stop,
    }
}

/// 各平台的重启候选命令，按顺序尝试。
///
/// macOS 用 AppleScript 弹系统自带的管理员授权框后执行 `shutdown -r now`
/// （应用本身没有也不需要 root 权限）；Linux 先试免密的 `systemctl reboot`
/// （本地活动会话下 logind 允许），失败再走 pkexec 图形授权；Windows 用
/// 自带的 `shutdown /r`。
#[cfg(target_os = "macos")]
fn restart_attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![(
        "osascript",
        vec![
            "-e",
            "do shell script \"/sbin/shutdown -r now\" with administrator privileges",
        ],
    )]
}

#[cfg(target_os = "linux")]
fn restart_attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("systemctl", vec!["reboot"]),
        // 没有 systemd 的发行版（elogind 等）用 loginctl。
        ("loginctl", vec!["reboot"]),
    ]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn restart_attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![("shutdown", vec!["/r", "/t", "0"])]
}

/// 需要管理员权限时的等价命令：Linux 用 pkexec 弹图形授权框。
/// 其他平台没有额外方案（macOS 的候选命令本身就是提权的）。
fn escalate_restart(program: &str, args: &[&str]) -> Option<(&'static str, Vec<String>)> {
    #[cfg(target_os = "linux")]
    {
        if is_root() {
            return None;
        }
        let mut escalated = Vec::with_capacity(args.len() + 1);
        escalated.push(program.to_string());
        escalated.extend(args.iter().map(|arg| arg.to_string()));
        Some(("pkexec", escalated))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (program, args);
        None
    }
}

fn run_restart_command(program: &str, args: &[&str]) -> RestartAttemptOutcome {
    let display = format!("{program} {}", args.join(" "));
    match Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => RestartAttemptOutcome::Success,
        Ok(output) => {
            let code = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "未知".to_string());
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if detail.is_empty() {
                RestartAttemptOutcome::Failed(format!("{display} 失败（退出码 {code}）"))
            } else {
                RestartAttemptOutcome::Failed(format!("{display} 失败（退出码 {code}）：{detail}"))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            RestartAttemptOutcome::Missing(format!("{display}：{error}"))
        }
        Err(error) => RestartAttemptOutcome::Failed(format!("{display} 无法执行: {error}")),
    }
}

fn run_restart() -> Result<(), String> {
    let mut failures = Vec::new();
    for (program, args) in restart_attempts() {
        let outcome = run_restart_command(program, &args);
        let escalation = escalate_restart(program, &args);
        match next_restart_step(&outcome, escalation.is_some()) {
            RestartStep::Done => {
                eprintln!("已通过 {program} 提交重启");
                return Ok(());
            }
            RestartStep::Next => {
                failures.push(outcome.describe());
                continue;
            }
            RestartStep::Escalate => {
                let Some((escalated_program, escalated_args)) = escalation else {
                    return Err(format!("重启失败：{}", outcome.describe()));
                };
                let escalated_args = escalated_args
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                return match run_restart_command(escalated_program, &escalated_args) {
                    RestartAttemptOutcome::Success => {
                        eprintln!("已通过 {escalated_program} 提交重启");
                        Ok(())
                    }
                    other => Err(format!(
                        "重启需要管理员授权，提权未完成：{}",
                        other.describe()
                    )),
                };
            }
            RestartStep::Stop => {
                return Err(format!(
                    "重启失败：{}（可能是没有权限或取消了授权）",
                    outcome.describe()
                ))
            }
        }
    }
    Err(format!(
        "没有可用的重启命令（已尝试：{}）",
        failures.join("；")
    ))
}

/// 后台重启，并把结果推给主窗口。
///
/// 提权弹窗可能停很久，所以不能在命令线程里同步跑；同时用原子标志避免
/// 连点两次提交两个重启请求。
fn spawn_restart(app: &AppHandle) {
    static RESTARTING: AtomicBool = AtomicBool::new(false);
    if RESTARTING.swap(true, Ordering::SeqCst) {
        eprintln!("已有重启请求在处理中，忽略重复点击");
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let result = run_restart();
        match result {
            Ok(()) => {
                let _ = app.emit(
                    RESTART_RESULT_EVENT,
                    RestartResult {
                        ok: true,
                        message: "已提交重启，系统即将重启".to_string(),
                    },
                );
            }
            Err(error) => {
                eprintln!("重启失败: {error}");
                // 成功时系统马上就不在了，失败时允许再点一次。
                RESTARTING.store(false, Ordering::SeqCst);
                let _ = app.emit(
                    RESTART_RESULT_EVENT,
                    RestartResult {
                        ok: false,
                        message: error.clone(),
                    },
                );
                // 从托盘触发时主窗口可能没打开，显示出来让用户看到失败原因。
                // 窗口/激活策略属于 AppKit 主线程操作，这里显式切回主线程。
                let window_app = app.clone();
                if let Err(error) = app.run_on_main_thread(move || show_main_window(&window_app)) {
                    eprintln!("显示主窗口失败: {error}");
                }
            }
        }
    });
}

fn apply_dock_taskbar_visibility(app: &AppHandle, hide: bool) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        window
            .set_skip_taskbar(hide)
            .map_err(|error| format!("无法更新任务栏显示状态: {error}"))?;
    }
    #[cfg(target_os = "macos")]
    let _macos_policy_updated = app
        .set_activation_policy(if hide {
            tauri::ActivationPolicy::Accessory
        } else {
            tauri::ActivationPolicy::Regular
        })
        .map_err(|error| format!("无法更新 Dock 显示状态: {error}"))?;
    Ok(())
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
    save_settings(&app, &settings)?;
    Ok(publish_settings(&app))
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
    Ok(publish_settings(&app))
}

#[tauri::command]
fn set_start_in_tray(app: AppHandle, enabled: bool) -> Result<SettingsView, String> {
    let mut settings = stored_settings(&app);
    settings.start_in_tray = enabled;
    save_settings(&app, &settings)?;
    Ok(publish_settings(&app))
}

#[tauri::command]
fn set_hide_from_dock_taskbar(app: AppHandle, enabled: bool) -> Result<SettingsView, String> {
    let mut settings = stored_settings(&app);
    settings.hide_from_dock_taskbar = enabled;
    save_settings(&app, &settings)?;
    apply_dock_taskbar_visibility(&app, enabled)?;
    Ok(publish_settings(&app))
}

/// 重启系统。提权弹窗可能等很久，所以立刻返回，结果通过事件推送。
#[tauri::command]
fn restart_system(app: AppHandle) {
    spawn_restart(&app);
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
            let hide_from_dock_taskbar = stored_settings(app.handle()).hide_from_dock_taskbar;
            let start_in_tray = stored_settings(app.handle()).start_in_tray;
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(if hide_from_dock_taskbar || start_in_tray {
                tauri::ActivationPolicy::Accessory
            } else {
                tauri::ActivationPolicy::Regular
            });
            apply_dock_taskbar_visibility(app.handle(), hide_from_dock_taskbar)
                .map_err(|error| error.to_string())?;
            if start_in_tray {
                if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                    let _ = window.hide();
                }
            } else {
                show_main_window(app.handle());
            }

            let status =
                MenuItem::with_id(app.handle(), "status", "正在扫描…", false, None::<&str>)?;
            let show = MenuItem::with_id(app.handle(), "show", "显示主窗口", true, None::<&str>)?;
            let refresh = MenuItem::with_id(app.handle(), "refresh", "扫描", true, None::<&str>)?;
            let quit = MenuItem::with_id(app.handle(), "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app.handle(), &[&status, &show, &refresh, &quit])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(create_tray_image())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        show_main_window(app);
                    }
                })
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
            unmount_temporary,
            get_settings,
            set_autostart,
            open_login_items_settings,
            set_close_action,
            set_start_in_tray,
            set_hide_from_dock_taskbar,
            restart_system
        ])
        .run(tauri::generate_context!())
        .expect("failed to run rEFInd Switcher");
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn macos_login_item_backend_requires_signature() {
        // 未签名：系统登录项接口只会返回 “Operation not permitted”，必须走兼容方式。
        assert_eq!(
            autostart_backend_for(true, true, false),
            AutostartBackend::LaunchAgent
        );
        // 未以 .app 形式运行（例如 tauri dev）：同样不能用系统登录项接口。
        assert_eq!(
            autostart_backend_for(true, false, true),
            AutostartBackend::LaunchAgent
        );
        // macOS 12 及以下没有 SMAppService。
        assert_eq!(
            autostart_backend_for(false, true, true),
            AutostartBackend::LaunchAgent
        );
        // 已签名的 .app + macOS 13+ 才用系统登录项。
        assert_eq!(
            autostart_backend_for(true, true, true),
            AutostartBackend::LoginItem
        );
    }

    #[test]
    fn macos_autostart_hint_explains_compatibility_mode() {
        let compatibility = "当前应用没有代码签名".to_string();

        // 兼容方式生效且已开启：提示说明为什么没用系统登录项。
        assert_eq!(
            macos_autostart_hint(
                AutostartState::Enabled,
                AutostartBackend::LaunchAgent,
                None,
                compatibility.clone()
            ),
            Some(compatibility.clone())
        );
        // 走系统登录项且正常：不需要额外提示。
        assert_eq!(
            macos_autostart_hint(
                AutostartState::Enabled,
                AutostartBackend::LoginItem,
                None,
                compatibility.clone()
            ),
            None
        );
        // 没开启时不唠叨兼容方式。
        assert_eq!(
            macos_autostart_hint(
                AutostartState::Disabled,
                AutostartBackend::LaunchAgent,
                None,
                compatibility.clone()
            ),
            None
        );
        // 需要用户在系统设置里批准：优先给可操作的提示。
        assert_eq!(
            macos_autostart_hint(
                AutostartState::RequiresApproval,
                AutostartBackend::LoginItem,
                None,
                compatibility.clone()
            ),
            AutostartState::RequiresApproval.hint().map(str::to_string)
        );
        // 注册失败后退回兼容方式：把失败原因带出来。
        let hint = macos_autostart_hint(
            AutostartState::Enabled,
            AutostartBackend::LaunchAgent,
            Some("Operation not permitted (code=1)".to_string()),
            compatibility,
        )
        .expect("应有提示");
        assert!(hint.contains("Operation not permitted"));
        assert!(hint.contains("兼容方式"));
    }

    #[test]
    fn macos_dev_binary_never_uses_login_item_api() {
        // 单测二进制不在 .app 内，也没有签名，必须判为兼容方式；
        // 否则（旧行为）会去调用系统登录项接口并拿到 Operation not permitted。
        assert!(!running_from_app_bundle());
        assert!(!app_bundle_is_signed());
        assert_eq!(autostart_backend(), AutostartBackend::LaunchAgent);

        // 开启状态下界面会说明当前用的是兼容方式。
        let hint = autostart_hint(AutostartState::Enabled).expect("应有兼容方式提示");
        assert!(hint.contains("兼容方式"), "实际提示: {hint}");
    }

    #[test]
    fn restart_attempt_chain_stops_after_cancelled_authorization() {
        use RestartAttemptOutcome::{Failed, Missing, Success};

        // 命令执行成功：结束。
        assert_eq!(next_restart_step(&Success, true), RestartStep::Done);
        // 系统里没有这个命令：换下一个候选（例如没有 systemd 的发行版）。
        assert_eq!(
            next_restart_step(&Missing("没有 systemctl".to_string()), true),
            RestartStep::Next
        );
        // 权限不足且还有提权方案：用 pkexec 重试。
        assert_eq!(
            next_restart_step(
                &Failed("Interactive authentication required".to_string()),
                true
            ),
            RestartStep::Escalate
        );
        // 提权也失败了（多半是用户取消授权）：停手，
        // 不能“取消授权之后换个方式照样把机器重启了”。
        assert_eq!(
            next_restart_step(&Failed("Request dismissed".to_string()), false),
            RestartStep::Stop
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_restart_uses_administrator_privileges() {
        let attempts = restart_attempts();
        assert_eq!(attempts.len(), 1, "macOS 只走一条提权路径");
        let (program, args) = &attempts[0];
        assert_eq!(*program, "osascript");
        let script = args.join(" ");
        assert!(
            script.contains("administrator privileges"),
            "实际脚本: {script}"
        );
        assert!(
            script.contains("/sbin/shutdown -r now"),
            "实际脚本: {script}"
        );
        // 候选命令本身就带管理员授权，不需要再提权一层。
        assert!(escalate_restart(program, args.as_slice()).is_none());
    }

    #[test]
    fn restart_command_runner_classifies_results() {
        // 真的去执行进程，验证成功/找不到命令/失败三种判定和 stderr 捕获。
        assert_eq!(
            run_restart_command("/usr/bin/true", &[]),
            RestartAttemptOutcome::Success
        );
        assert!(matches!(
            run_restart_command("/nonexistent-restart-binary", &[]),
            RestartAttemptOutcome::Missing(_)
        ));

        // 模拟“用户取消授权”：退出码非零 + stderr 说明，必须归类为失败并带上原因。
        let outcome =
            run_restart_command("/bin/sh", &["-c", "echo 'Request dismissed' >&2; exit 126"]);
        assert!(matches!(outcome, RestartAttemptOutcome::Failed(_)));
        let description = outcome.describe();
        assert!(description.contains("126"), "应带退出码: {description}");
        assert!(
            description.contains("Request dismissed"),
            "应带 stderr: {description}"
        );
    }

    #[test]
    fn frontend_listens_to_backend_events() {
        // 前后端通过事件名和命令名对接，任何一边改名了都会让联动失效，
        // 这里直接对着前端源码断言契约。
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui/app.js");
        let Ok(source) = fs::read_to_string(&path) else {
            return;
        };
        for event in [SETTINGS_CHANGED_EVENT, RESTART_RESULT_EVENT] {
            assert!(
                source.contains(&format!("listen(\"{event}\"")),
                "前端没有监听 {event}"
            );
        }
        assert!(
            source.contains("invoke(\"restart_system\")"),
            "前端没有调用 restart_system 命令"
        );
    }
}
