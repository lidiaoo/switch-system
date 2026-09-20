# rEFInd Switcher

一个基于 Tauri 2 的跨平台系统托盘工具，用于覆盖 rEFInd 的 `EFI/refind/vars/PreviousBoot` 文件，从托盘菜单直接选择下次启动 Linux、Windows 或 macOS。

## 功能

- Windows / Linux / macOS 系统托盘菜单直接切换下次启动系统。
- 主窗口不占用 Windows 任务栏、Linux 任务栏或 macOS Dock，仅通过系统托盘常驻。
- 开机启动开关：登录系统后自动启动并常驻托盘。
- 可配置点击窗口 ❌ 时是最小化到托盘还是退出程序。
- Linux 提供 `deb`、`rpm` 和 `AppImage`；macOS 支持 Intel、Apple Silicon 和通用二进制。
- 自动扫描已挂载的 `EFI/refind/vars` 目录。
- Linux 支持通过 `lsblk` 查找未挂载的 FAT/ESP 分区并临时只读挂载；写入前会重新挂载为读写，写完再恢复只读。
- 主窗口可手动指定变量目录，用于处理多个 ESP 或非常规挂载路径。
- 写入使用同目录临时文件、原子替换和 SHA-256 校验。
- `PreviousBoot-linux/windows/mac` 仅作为模板读取，应用不会修改这三个模板文件。
- 内置 Windows/macOS/Linux 打包图标。

## 应用设置

主窗口「应用设置」区域提供以下设置：

- **开机启动**：登录系统后自动启动应用。
  - macOS 13 及以上调用系统登录项接口 `SMAppService.mainApp` 注册，条目会出现在「系统设置 → 通用 → 登录项与扩展 → 登入时打开」，应用更新或被移动位置后启动时会自动重新注册；macOS 12 及以下回退为 `~/Library/LaunchAgents/rEFInd Switcher.plist`（早期版本写入的 LaunchAgent 会在启动时自动清理）。
  - Windows 写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，Linux 写入 `~/.config/autostart`。
  - Linux 即使以 root 执行，也会识别 `SUDO_USER` / `PKEXEC_UID`，并把 desktop 文件写回实际登录用户的 `~/.config/autostart`。
  - Linux 通用二进制/AppImage 的路径会被写入 desktop 文件；桌面会话启动该程序后仍以当前登录用户运行。
  - 若系统返回「需要批准」（`requiresApproval`），主窗口会给出提示和「打开登录项与扩展设置」按钮，在系统设置里勾选即可。
- **启动时隐藏主界面**：登录/启动后只在托盘待命，不直接打开主窗口。
- **点击窗口 ❌ 时**：`最小化到托盘`（隐藏主窗口继续后台运行，托盘菜单「显示主窗口」可恢复）或 `退出程序`（同时退出托盘）。

设置持久化在应用配置目录的 `settings.json` 中。开机启动以系统真实状态为准：在系统设置／登录项里手动修改后，重新打开主窗口会自动同步显示。

## 环境要求

- Node.js 18+
- Rust 1.77.2+ 与 Cargo
- 平台构建依赖见 [Tauri 2 Prerequisites](https://v2.tauri.app/start/prerequisites/)

## 开发运行

```bash
npm install
npm run tauri dev
```

构建安装包：

```bash
npm run tauri build
```

平台专用构建：

```bash
# Linux: deb + rpm + AppImage
npm run build:linux

# macOS Intel
npm run build:mac-intel

# macOS Apple Silicon
npm run build:mac-arm

# macOS 通用二进制，同时包含 Intel 和 Apple Silicon
npm run build:mac-universal
```

Linux 包必须在 Linux 环境生成；macOS 包必须在 macOS 环境生成。请先在目标平台安装 Tauri 2 所需的系统依赖（Linux 需要 WebKit/GTK、AppImage 工具链等），再运行对应命令。

如果 AppImage 打包阶段出现 `strip: unknown type [0x13] section '.relr.dyn'` 或 `failed to run linuxdeploy`，可先使用 `NO_STRIP=1 npm run build:linux` 跳过 `linuxdeploy` 的 strip 步骤。该环境变量仅影响本次命令，不会修改项目配置。

## 构建产物

所有构建与打包只使用默认目录 `src-tauri/target`，不要再指定 `--target-dir` 或设置 `CARGO_TARGET_DIR`（此前 Windows 侧产生的 `src-tauri/target-new` 已废弃）。

安装包输出位置：

- Windows：`src-tauri/target/release/bundle/nsis/*.exe`、`src-tauri/target/release/bundle/msi/*.msi`
- macOS：`src-tauri/target/release/bundle/macos/*.app`、`src-tauri/target/release/bundle/dmg/*.dmg`
- Linux：`src-tauri/target/release/bundle/deb/*.deb`、`src-tauri/target/release/bundle/rpm/*.rpm`、`src-tauri/target/release/bundle/appimage/*.AppImage`
- 指定 `--target <triple>` 时位于 `src-tauri/target/<triple>/release/bundle`

## 权限说明

写入 ESP 中的 rEFInd 变量文件需要管理员/root 权限。

- Windows：请以管理员身份运行应用；开发时也需要在管理员终端执行 `npm run tauri dev`。
- Linux：应用默认以普通用户运行。ESP 未挂载时，会通过 `pkexec` 弹出系统授权并临时挂载分区（可用授权用户密码）；挂载选项会绑定到实际登录用户。已挂载且可写时不要求 root，也不再要求整个应用以 root 启动。
- macOS：如果 ESP 未挂载，需要先用系统工具挂载，或在主窗口指定已挂载的变量目录。

> Linux GNOME 默认没有传统系统托盘。如果托盘图标未显示，需要启用支持 StatusNotifierItem 的扩展，或使用主窗口操作。

## 注意

- 应用只修改 rEFInd 的 `PreviousBoot` 嗅探状态，不会直接调用系统重启 API。
- 正式发布前建议替换品牌图标并补充签名配置。
