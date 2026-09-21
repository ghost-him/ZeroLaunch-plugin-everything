#!/usr/bin/env python3
"""ZeroLaunch 插件打包脚本：构建 Rust 插件并打包为宿主可直接安装的 zip。

用法:
    python package.py                 # cargo build --release 后打包
    python package.py --no-build      # 复用现有构建产物，直接打包
    python package.py --target <triple>   # 交叉编译（产物在 target/<triple>/release/）
    python package.py --out <目录>    # 指定输出目录（默认 ./dist）

无系统 Python 时可用 uv 运行（uv 自动下载托管 Python，无需手动安装）:
    uv run --python 3.12 python package.py

产物: <输出目录>/zerolaunch-plugin-<插件短id>-v<版本号>.zip

插件短id = manifest [plugin].id 去掉域名前缀后的末段
（如 com.ghost-him.everything → everything，com.example.hello-world → hello-world）；
产物名与插件二进制/仓库命名族 zerolaunch-plugin-* 对齐，便于按插件+版本归档分发。

zip 根目录结构（与宿主安装器约定一致，manifest.toml 必须位于 zip 根）:
    manifest.toml
    bin/<可执行文件>        # 文件名取自 manifest [runtime].command
    ui/...                  # 若存在
    i18n/...                # 若存在
    <icon 文件>             # 若 manifest [icon] path 声明
    extra/ 目录内容         # 若存在：内容并入 zip 根（如 extra/Everything64.dll →
                            # 根/Everything64.dll，与 exe 同级；子目录保持相对结构），
                            # 用于随插件分发需与 exe 同目录的运行时文件

安装方式: 设置 → 插件管理 → 安装本地插件，选择该 zip；
或手动解压到 %USERPROFILE%/.ZeroLaunch-rs/plugins/<plugin-id>/。
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import zipfile
from pathlib import Path

# CI/部分 Windows 控制台默认编码非 UTF-8（如 cp1252），
# 直接 print 中文/全角字符会抛 UnicodeEncodeError；统一重配置为 UTF-8。
for _stream in (sys.stdout, sys.stderr):
    if _stream is not None and hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8")

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    sys.exit("需要 Python 3.11+（tomllib 为标准库），请升级 Python 后重试。")

ROOT = Path(__file__).resolve().parent

# 宿主 manifest schema 字段名（serde rename 后的 camelCase 键）。
# toml 中写成 snake_case 不会导致解析报错，但字段会被 serde 静默丢弃
# （如 [ui] panel_entry → 前端拿不到 panelEntry → 面板不加载），
# 因此对已知易错字段做显式预检。
SNAKE_TO_CAMEL = {
    "panel_entry": "panelEntry",
    "settings_entry": "settingsEntry",
    "result_item_entry": "resultItemEntry",
}


def check_manifest_field_names(manifest: dict) -> None:
    """预检 manifest 可选段字段名，发现 snake_case 键时给出显式警告。

    宿主 schema 使用 camelCase（panelEntry/settingsEntry/resultItemEntry）；
    snake_case 键会被 serde 静默忽略，属无声失效，必须提前暴露。
    """
    ui = manifest.get("ui") or {}
    for snake, camel in SNAKE_TO_CAMEL.items():
        if snake in ui and camel not in ui:
            print(
                f"警告: manifest [ui] 使用了 {snake!r}，"
                f"宿主 schema 要求 {camel!r}，该字段会被忽略。"
            )


def load_toml(path: Path) -> dict:
    """读取 toml 文件并返回解析结果；文件缺失或格式错误时直接报错退出。"""
    with open(path, "rb") as f:
        return tomllib.load(f)


def short_plugin_id(plugin_id: str) -> str:
    """从 manifest 插件 id 提取末段作产物名：com.ghost-him.everything → everything。

    同时校验 id 是合法的『域.短名』形式——若短名缺失（裸名/纯域名）则按字面使用
    整个 id，避免产物名退化成无区分度的 zerolaunch-plugin-v1.0.0.zip。
    """
    parts = plugin_id.split(".")
    last = parts[-1]
    if not last:
        print(
            f"警告: 插件 id {plugin_id!r} 无法提取末段短名，"
            "产物名将使用完整 id 并保留点号。"
        )
        return plugin_id
    return last


def check_manifest_short_id(plugin_id: str) -> None:
    """校验插件 id 末段短名与 Cargo 包名命名族 zerolaunch-plugin-* 的一致性。

    产物名取自 manifest id 末段而非 Cargo 包名（Cargo 包名无法从 id 推导）；
    仅当 Cargo 包名已是 zerolaunch-plugin-X 形式而 X 与短 id 不符（fork 时
    只改了 manifest id、漏改包名）才告警。模板原装包名 zerolaunch-hello-world-plugin
    属 fork 前状态，不触发。
    """
    short = plugin_id.split(".")[-1]
    cargo_toml = load_toml(ROOT / "Cargo.toml")
    package_name = cargo_toml["package"]["name"]
    if short and package_name.startswith("zerolaunch-plugin-") and not package_name.endswith(short):
        print(
            f"警告: manifest 插件短 id {short!r} 与 Cargo 包名 {package_name!r} 不一致，"
            "产物将按短 id 命名。fork 插件时建议包名取 zerolaunch-plugin-<短id>。"
        )


def build_release(target: str | None) -> None:
    """在插件目录执行 cargo build --release；--target 指定时做交叉编译。"""
    cmd = ["cargo", "build", "--release"]
    if target:
        cmd += ["--target", target]
    print(f"$ {' '.join(cmd)}")
    subprocess.run(cmd, cwd=ROOT, check=True)


def locate_binary(package_name: str, target: str | None) -> Path:
    """返回构建产物可执行文件路径；不存在时给出排查提示并退出。

    本机构建产物在 target/release/，交叉编译在 target/<triple>/release/；
    Windows 目标（本机 Windows 或 cross 目标含 windows）带 .exe 后缀。
    """
    exe_suffix = ".exe" if (os.name == "nt" or (target and "windows" in target)) else ""
    base = (
        ROOT / "target" / "release"
        if target is None
        else ROOT / "target" / target / "release"
    )
    cand = base / (package_name + exe_suffix)
    if cand.is_file():
        return cand
    sys.exit(
        f"未找到构建产物 {cand}，请先执行 `cargo build --release`"
        "（或确认 --target 与实际构建产物一致）。"
    )


def collect_entries(manifest: dict, binary: Path) -> list[tuple[Path, str]]:
    """收集打包条目 (磁盘路径, zip 内相对路径)，全部位于 zip 根，避免公共前缀歧义。

    打包内容：manifest.toml（必需）、bin/<command 文件名>、ui/、i18n/、
    以及 manifest [icon] path 声明的图标文件（若存在）。
    另有约定目录 extra/（可选）：目录内容原样并入 zip 根，与 manifest.toml、
    bin/ 同级——用于分发需与 exe 同目录的运行时文件（如 Everything64.dll）；
    子目录保持相对结构（extra/sub/x → zip 根 sub/x）。
    """
    command = manifest.get("runtime", {}).get("command")
    if not command:
        sys.exit("manifest.toml 缺少 [runtime].command 字段，无法确定可执行文件位置。")
    entries: list[tuple[Path, str]] = [(ROOT / "manifest.toml", "manifest.toml")]
    entries.append((binary, f"bin/{Path(command).name}"))
    for sub in ("ui", "i18n"):
        src = ROOT / sub
        if src.is_dir():
            for p in sorted(src.rglob("*")):
                if p.is_file():
                    entries.append((p, f"{sub}/{p.relative_to(src).as_posix()}"))
    extra = ROOT / "extra"
    if extra.is_dir():
        for p in sorted(extra.rglob("*")):
            if p.is_file():
                entries.append((p, p.relative_to(extra).as_posix()))
    icon = (manifest.get("icon") or {}).get("path")
    if icon:
        icon_rel = Path(icon)
        if icon_rel.is_absolute() or icon_rel.name in ("", "..") or icon.startswith(".."):
            print(f"警告: manifest [icon].path 必须是插件目录内的相对路径（当前: {icon!r}），已跳过。")
        elif (ROOT / icon).is_file():
            entries.append((ROOT / icon, icon_rel.as_posix()))
        else:
            print(f"警告: manifest 声明了 icon {icon!r} 但文件不存在，已跳过。")
    return entries


def write_zip(entries: list[tuple[Path, str]], out_path: Path) -> None:
    """将条目写入 zip，统一使用正斜杠路径（zip 规范要求）。"""
    with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as zf:
        for disk_path, arcname in entries:
            zf.write(disk_path, arcname)


def main() -> int:
    """解析命令行参数并执行 构建 → 定位产物 → 打包 → 输出安装提示。"""
    parser = argparse.ArgumentParser(description="ZeroLaunch 插件打包脚本")
    parser.add_argument(
        "--no-build",
        action="store_true",
        help="跳过 cargo build --release，直接打包现有产物",
    )
    parser.add_argument(
        "--target",
        default=None,
        help="cargo 交叉编译目标 triple（如 x86_64-pc-windows-gnu）",
    )
    parser.add_argument("--out", default="dist", help="输出目录（默认 dist/）")
    args = parser.parse_args()

    cargo_toml = load_toml(ROOT / "Cargo.toml")
    manifest = load_toml(ROOT / "manifest.toml")
    check_manifest_field_names(manifest)
    package_name = cargo_toml["package"]["name"]
    plugin_id = manifest["plugin"]["id"]
    plugin_version = manifest["plugin"]["version"]
    if "version" in cargo_toml["package"] and cargo_toml["package"]["version"] != plugin_version:
        print(
            f"警告: Cargo.toml 版本 {cargo_toml['package']['version']!r} 与 "
            f"manifest 版本 {plugin_version!r} 不一致，产物按 manifest 版本命名。"
        )
    check_manifest_short_id(plugin_id)
    short_id = short_plugin_id(plugin_id)

    if not args.no_build:
        build_release(args.target)

    binary = locate_binary(package_name, args.target)

    command_name = Path(manifest["runtime"]["command"]).name
    if command_name != binary.name:
        print(
            f"警告: manifest [runtime].command 文件名为 {command_name!r}，"
            f"实际构建产物为 {binary.name!r}，将按 command 名打包。"
        )

    entries = collect_entries(manifest, binary)
    out_dir = ROOT / args.out
    out_dir.mkdir(parents=True, exist_ok=True)
    out_path = out_dir / f"zerolaunch-plugin-{short_id}-v{plugin_version}.zip"
    write_zip(entries, out_path)
    print(f"打包完成: {out_path}（共 {len(entries)} 个文件）")
    print("安装: 设置 → 插件管理 → 安装本地插件，选择该 zip；")
    print(f"      或手动解压到 %USERPROFILE%/.ZeroLaunch-rs/plugins/{plugin_id}/。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
