#!/usr/bin/env python3

import argparse
import atexit
import ctypes
import hashlib
import json
import os
import shutil
import string
import subprocess
import sys
import tempfile
from pathlib import Path


SOURCE_FILES = {
    "linux": "PreviousBoot-linux",
    "windows": "PreviousBoot-windows",
    "win": "PreviousBoot-windows",
    "macos": "PreviousBoot-mac",
    "mac": "PreviousBoot-mac",
}

FAT_FILESYSTEMS = {"vfat", "fat", "fat12", "fat16", "fat32", "exfat"}
ESP_PARTTYPE = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
_ACTIVE_MOUNT = None


class TemporaryRefindMount:
    def __init__(self, device, mountpoint):
        self.device = device
        self.mountpoint = mountpoint

    def make_writable(self):
        subprocess.run(
            ["mount", "-o", "remount,rw", self.device, str(self.mountpoint)],
            check=True,
        )

    def close(self):
        subprocess.run(["umount", str(self.mountpoint)], check=True)
        self.mountpoint.rmdir()


def close_active_mount():
    global _ACTIVE_MOUNT
    if _ACTIVE_MOUNT is not None:
        _ACTIVE_MOUNT.close()
        _ACTIVE_MOUNT = None


atexit.register(close_active_mount)

DISPLAY_NAMES = {
    "linux": "Linux",
    "windows": "Windows",
    "win": "Windows",
    "macos": "macOS",
    "mac": "macOS",
}


def is_admin():
    if os.name == "nt":
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    return os.geteuid() == 0


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def candidate_directories():
    candidates = []

    if os.name == "nt":
        roots = [Path(f"{drive}:\\") for drive in string.ascii_uppercase]
        root_patterns = []
    else:
        roots = [
            Path("/Volumes/REFIND"),
            Path("/boot/efi"),
            Path("/efi"),
            Path("/boot"),
            Path("/mnt/REFIND"),
            Path("/media/REFIND"),
        ]
        root_patterns = [
            "/Volumes/*",
            "/mnt/*",
            "/mnt/*/REFIND",
            "/media/*",
            "/media/*/*",
            "/run/media/*",
            "/run/media/*/REFIND",
            "/run/media/*/*",
    ]
    for pattern in root_patterns:
        roots.extend(Path("/").glob(pattern.lstrip("/")))

    for root in roots:
        candidates.append(root / "EFI" / "refind" / "vars")

    unique = []
    seen = set()
    for candidate in candidates:
        try:
            resolved = candidate.resolve()
        except OSError:
            continue
        if resolved not in seen and resolved.is_dir():
            seen.add(resolved)
            unique.append(resolved)
    return unique


def unmounted_refind_candidates():
    try:
        result = subprocess.run(
            [
                "lsblk",
                "--json",
                "--output",
                "PATH,TYPE,FSTYPE,LABEL,PARTLABEL,PARTTYPE,MOUNTPOINTS",
            ],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as error:
        raise OSError(f"无法枚举磁盘分区: {error}") from error

    try:
        devices = json.loads(result.stdout)["blockdevices"]
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise OSError(f"无法解析 lsblk 输出: {error}") from error

    candidates = []
    for device in devices:
        filesystem = device.get("fstype")
        mountpoints = device.get("mountpoints") or []
        if filesystem not in FAT_FILESYSTEMS or mountpoints:
            continue

        device_path = device.get("path")
        if not device_path:
            continue

        label = str(device.get("label") or "").lower()
        partlabel = str(device.get("partlabel") or "").lower()
        parttype = str(device.get("parttype") or "").lower()
        hints = f"{label} {partlabel}"

        if "refind" in hints:
            priority = 0
        elif parttype == ESP_PARTTYPE or "efi system partition" in hints:
            priority = 1
        else:
            priority = 2
        candidates.append((priority, device_path, filesystem))

    candidates.sort(key=lambda item: (item[0], item[1]))
    return [(device_path, filesystem) for _, device_path, filesystem in candidates]


def scan_unmounted_partitions():
    global _ACTIVE_MOUNT
    mount_errors = []

    for device, filesystem in unmounted_refind_candidates():
        mountpoint = Path(tempfile.mkdtemp(prefix="switch-refind-"))
        result = subprocess.run(
            [
                "mount",
                "--read-only",
                "--types",
                filesystem,
                device,
                str(mountpoint),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        if result.returncode != 0:
            mountpoint.rmdir()
            mount_errors.append(f"{device}: {result.stderr.strip()}")
            continue

        candidate = mountpoint / "EFI" / "refind" / "vars"
        if candidate.is_dir():
            _ACTIVE_MOUNT = TemporaryRefindMount(device, mountpoint)
            return candidate.resolve()

        subprocess.run(["umount", str(mountpoint)], check=True)
        mountpoint.rmdir()

    hint = ""
    if os.name != "nt" and os.geteuid() != 0:
        hint = "；挂载分区需要 root 权限，请使用 sudo 运行"
    details = ""
    if mount_errors:
        details = f"；挂载失败: {mount_errors[0]}"
    raise FileNotFoundError(
        f"已扫描未挂载的 FAT/ESP 分区，但未找到 EFI/refind/vars 目录{details}{hint}"
    )


def find_vars_directory(explicit=None, writable=False):
    if explicit:
        directory = explicit.expanduser().resolve()
        if not directory.is_dir():
            raise FileNotFoundError(f"rEFInd 变量目录不存在: {directory}")
        return directory

    candidates = candidate_directories()
    if not candidates:
        if os.name == "nt":
            raise FileNotFoundError(
                "未找到 EFI/refind/vars 目录；请使用 --vars-dir 明确指定路径"
            )
        candidates = [scan_unmounted_partitions()]
    if len(candidates) > 1:
        print("找到多个 rEFInd 变量目录，请使用 --vars-dir 指定:", file=sys.stderr)
        for candidate in candidates:
            print(f"  {candidate}", file=sys.stderr)
        raise SystemExit(2)
    directory = candidates[0]
    if writable and _ACTIVE_MOUNT is not None:
        _ACTIVE_MOUNT.make_writable()
    return directory


def atomic_copy(source, target):
    temporary = None
    try:
        with source.open("rb") as source_file:
            with tempfile.NamedTemporaryFile(
                mode="wb",
                prefix=".PreviousBoot.",
                dir=target.parent,
                delete=False,
            ) as target_file:
                temporary = Path(target_file.name)
                shutil.copyfileobj(source_file, target_file, 1024 * 1024)
                target_file.flush()
                os.fsync(target_file.fileno())
        os.replace(temporary, target)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def switch_system(system, vars_directory):
    source_name = SOURCE_FILES[system]
    source = vars_directory / source_name
    target = vars_directory / "PreviousBoot"

    if not source.is_file():
        raise FileNotFoundError(f"变量文件不存在: {source}")
    if not target.parent.is_dir():
        raise FileNotFoundError(f"目标目录不存在: {target.parent}")

    try:
        atomic_copy(source, target)
    except PermissionError:
        if os.name == "nt":
            hint = "请以管理员身份运行"
        else:
            hint = "请使用 sudo 运行"
        raise PermissionError(f"没有权限覆盖 {target}，{hint}")

    if file_hash(source) != file_hash(target):
        raise OSError(f"写入校验失败: {target}")

    print(f"已设置下次启动系统: {DISPLAY_NAMES[system]}")
    print(f"来源: {source}")
    print(f"目标: {target}")


def print_found_directories():
    candidates = candidate_directories()
    if not candidates:
        try:
            candidates = [find_vars_directory()]
        except OSError as error:
            print(f"未找到 EFI/refind/vars 目录: {error}")
            return
        return
    print("找到以下 rEFInd 变量目录:")
    for candidate in candidates:
        print(f"  {candidate}")


def print_variables(vars_directory):
    target = vars_directory / "PreviousBoot"
    print("rEFInd 嗅探状态")
    print("----------------")
    print(f"变量目录: {vars_directory}")
    print("可用系统:")
    for system in ("linux", "windows", "macos"):
        source = vars_directory / SOURCE_FILES[system]
        exists = "存在" if source.is_file() else "不存在"
        marker = ""
        if source.is_file() and target.is_file() and file_hash(source) == file_hash(target):
            marker = "，当前生效"
        print(f"  {system:<8} {source.name:<22} {exists}{marker}")


def choose_system_interactively():
    print("请选择下次启动系统:")
    print("  1) Linux")
    print("  2) Windows")
    print("  3) macOS")
    try:
        choice = input("输入编号: ").strip()
    except (KeyboardInterrupt, EOFError):
        print()
        raise SystemExit(130)
    return {
        "1": "linux",
        "2": "windows",
        "3": "macos",
    }.get(choice)


def parse_args():
    parser = argparse.ArgumentParser(
        description="通过覆盖 rEFInd 的 PreviousBoot 二进制变量设置下次启动系统"
    )
    parser.add_argument(
        "system",
        nargs="?",
        choices=sorted(SOURCE_FILES),
        help="下次启动系统: linux、windows/win、macos/mac",
    )
    parser.add_argument(
        "--vars-dir",
        type=Path,
        help="明确指定 EFI/refind/vars 目录",
    )
    parser.add_argument(
        "--find",
        action="store_true",
        help="查找所有可用的 rEFInd 变量目录",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="列出变量文件和当前生效系统",
    )
    return parser.parse_args()


def main():
    args = parse_args()

    if args.find:
        print_found_directories()
        return

    try:
        writable = not args.list
        vars_directory = find_vars_directory(args.vars_dir, writable=writable)
        if args.list:
            print_variables(vars_directory)
            return

        if not args.system:
            print_variables(vars_directory)
            print()
            system = choose_system_interactively()
        else:
            system = args.system
        if system is None:
            print("无效选择", file=sys.stderr)
            raise SystemExit(2)
        switch_system(system, vars_directory)
    except (OSError, ValueError) as error:
        print(f"错误: {error}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
