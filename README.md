# rEFInd Switcher

一个基于 Tauri 2 的跨平台系统托盘工具，用于覆盖 rEFInd 的 `EFI/refind/vars/PreviousBoot` 文件，从托盘菜单直接选择下次启动 Linux、Windows 或 macOS。

## 功能

- Windows / Linux / macOS 系统托盘菜单直接切换下次启动系统。
- Linux 提供 `deb`、`rpm` 和 `AppImage`；macOS 支持 Intel、Apple Silicon 和通用二进制。
- 自动扫描已挂载的 `EFI/refind/vars` 目录。
- Linux 支持通过 `lsblk` 查找未挂载的 FAT/ESP 分区并临时只读挂载；写入前会重新挂载为读写，写完再恢复只读。
- 主窗口可手动指定变量目录，用于处理多个 ESP 或非常规挂载路径。
- 写入使用同目录临时文件、原子替换和 SHA-256 校验。
- `PreviousBoot-linux/windows/mac` 仅作为模板读取，应用不会修改这三个模板文件。
- 内置 Windows/macOS/Linux 打包图标。

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

Linux 包必须在 Linux 环境生成；macOS 包必须在 macOS 环境生成。

## 权限说明

写入 ESP 中的 rEFInd 变量文件需要管理员/root 权限。

- Windows：请以管理员身份运行应用；开发时也需要在管理员终端执行 `npm run tauri dev`。
- Linux：如 ESP 未挂载，应用需要挂载权限，请以 root 或授权用户运行；已挂载且可写时不要求 root。
- macOS：如果 ESP 未挂载，需要先用系统工具挂载，或在主窗口指定已挂载的变量目录。

> Linux GNOME 默认没有传统系统托盘。如果托盘图标未显示，需要启用支持 StatusNotifierItem 的扩展，或使用主窗口操作。

## 注意

- 应用只修改 rEFInd 的 `PreviousBoot` 嗅探状态，不会直接调用系统重启 API。
- 正式发布前建议替换品牌图标并补充签名配置。
