# ZeroLaunch Everything 搜索插件

ZeroLaunch 第三方插件：通过 Everything SDK 实时搜索本机文件，以沉浸式面板呈现结果。触发词 `ev` / `every`，全局热键 `Ctrl+E`。

## 功能

- **Everything 实时检索**：完整路径匹配查询，支持 Everything 原生查询语法（`ext:`、`folder:`、`!` 等，见 Everything 文档）。
- **沉浸式面板**：面板自渲染结果列表——文件类型 emoji 图标、大小、修改时间、扩展名徽标、所在目录；不依赖宿主 List 管线与图标链路。
- **打开与定位**：`Enter` 打开文件；`Ctrl+Enter` 打开选中项所在文件夹；双击或动作菜单打开文件；`open_folder` 动作打开所在文件夹。
- **路径匹配开关**：`Ctrl+U` 切换"匹配完整路径而非仅文件名"（等价 Everything 的 Ctrl+U），即时生效，状态回显在面板底栏。
- **短查询优化**：查询长度低于排序阈值时跳过排序直接返回，降低短查询（命中项极多）的延迟。

## 环境要求

- Windows x86_64（Everything SDK 仅提供 64 位 DLL，插件二进制同样仅构建该目标）。
- 本机已安装并运行 [Everything](https://www.voidtools.com/)（SDK 查询依赖 Everything 服务）。

## 使用

| 操作 | 效果 |
| --- | --- |
| `Ctrl+E` | 唤起沉浸式面板 |
| 搜索栏输入 `ev` / `every` + 空格 | 触发词唤起（带查询词时直接展示结果并预填输入框） |
| `↑` / `↓` | 选择结果 |
| `Enter` / 双击 | 打开选中项 |
| `Ctrl+Enter` | 打开选中项所在文件夹 |
| `Ctrl+U` | 切换路径匹配 |
| `Esc` | 返回宿主默认面板 |

面板内输入 200ms 防抖后查询（Everything 查询为阻塞调用，避免每键触发）；`Esc` / `Ctrl+U` 由插件 `interaction_policy` bindings 声明，宿主键盘状态机统一解释执行；方向键 / `Enter` / `Ctrl+Enter` 由面板挂在宿主窗口层监听（鼠标点击结果项后焦点落到 body 也不失效），面板卸载时解绑。

## 配置

设置页可配置项（`src/plugin.rs` `setting_schema()`）：

| 配置 | 默认 | 说明 |
| --- | --- | --- |
| 排序阈值 | `4` | 查询长度达到该字符数才排序（1~3 字符短查询跳过） |
| 排序方式 | 名称升序 | 26 种排序（名称/路径/大小/扩展名/类型/创建/修改/访问日期等，升序或降序） |
| 结果数量上限 | `10` | 单次查询最大返回数（1–100） |
| 路径匹配 | 关 | 匹配完整路径而非仅文件名（等价 Everything 的 Ctrl+U） |

## 架构

```
搜索栏输入 → host.query() → 宿主 bridge_query → 插件 query()
    → Everything SDK（阻塞线程池 + search_guard 串行化）
    → QueryResponse::CustomPanel { panel_type: "everything", data: 自描述 JSON }
    → 面板 onDataUpdate / host.query 响应 → 自渲染列表
Enter → host.executeAction("open", { path }) → 宿主 shell_open
Ctrl+Enter → host.executeAction("open_folder", { path }) → 插件 open_folder → 宿主 shell_open_folder（打开父目录）
```

- **CustomPanel 数据契约**：`panelData` 为自描述 JSON——`query`、`items`（`path`/`name`/`dir`/`isFolder`/`size`/`modified`(unix 秒)/`extension`）、`sortSkipped`、`enablePathMatch`、`resultLimit`。面板按需自渲染，`keep_search_bar = false`。
- **唤醒重放**：宿主唤起面板时经 `onDataUpdate` 重放下次查询数据；触发词带查询词进入时直接展示结果并将查询词预填到输入框。
- **List 回退**：面板 `normalizeResponse` 兼容宿主旧管线返回的 `mode: "search"` 列表形状（title/subtitle/icon），统一为内部 item。
- **SDK 并发约束**：Everything SDK 的 set_search/query/迭代作用于进程内全局状态，全部查询经 `search_guard` 互斥串行化，并在 `tokio::spawn_blocking` 中执行，避免阻塞 RPC 循环。
- **单实例复用**：进程内复用首个 `Everything` 实例（第二个实例的数据库等待会挂死）；每次查询前 `reset()` 清空上次结果状态；`Everything_SetMatchPath` 为 SDK 全局状态，每次查询显式设置保证确定性。
- **运行时依赖**：`extra/Everything64.dll` 由打包脚本并入 zip 根，安装后必须与插件 exe 同目录（`bin/`）。

## 项目结构

```
├── Cargo.toml          # 依赖 zerolaunch-plugin-sdk-rust / plugin-api（0.2）
├── manifest.toml       # 插件清单（必填 [plugin] 元数据 + 运行时命令、热键、面板入口），打包时位于 zip 根
├── icon.svg            # 插件图标（清单 [icon] 声明，随包分发并作为 Release 附件供市场卡片展示）
├── src/main.rs         # 启动骨架（init() + app().run()，随模板同步，尽量别改）
├── src/plugin.rs       # EverythingPlugin（Plugin + Configurable trait 实现）+ app() 装配
├── ui/panel.mjs        # 沉浸式面板（Shadow DOM 内挂载，宿主 CSS 变量自动跟随主题）
├── i18n/               # 语言包（zh-Hans / en，host 加载时合并进翻译目录）
├── extra/              # Everything64.dll（打包时并入 zip 根，与 exe 同目录）
├── package.py          # 打包脚本（Python 3.11+，tomllib 标准库）
└── .github/workflows/  # CI（check/build/打包）、推 tag 自动发 Release、模板同步入口
```

插件只依赖 SDK crates（trait/类型 + `host()`），不依赖 Tauri/宿主源码；独立于宿主 workspace 构建。`src/main.rs` 属同步集合（会被模板版本覆盖），业务逻辑一律写 `src/plugin.rs`。

## 构建与打包

推 tag 自动发布（`.github/workflows/release.yml`，随模板同步下发）：`git tag v0.1.0 && git push origin v0.1.0` —— GitHub Actions 构建 + 打包，把 `dist/zerolaunch-plugin-<短id>-v<版本>.zip` 作为附件发到 Release。要求 tag 形如 `v<major>.<minor>.<patch>`，且与 `manifest.toml [plugin].version`、`Cargo.toml version` 三处一致（不一致直接失败）。补发：Actions → 「发布插件」→ Run workflow，填该 tag。

本地打包（同一套流程）：

```bash
cargo check                 # 零错误冒烟
python package.py           # cargo build --release 后打包
python package.py --no-build    # 复用现有产物直接打包
python package.py --target <triple>   # 交叉编译
python package.py --out <目录>       # 指定输出目录（默认 ./dist）
```

无系统 Python 时：`uv run package.py`。

产物 `dist/zerolaunch-plugin-everything-v<版本号>.zip`（插件短id = manifest `[plugin].id` 末段，如 `com.ghost-him.everything` → `everything`），zip 布局：`manifest.toml` 位于根、`bin/zerolaunch-plugin-everything.exe`、`ui/`、`i18n/`、`Everything64.dll`（extra/ 内容并入根）。
插件市场按 `/releases/latest` 的这个 zip 附件自动安装，别改产物名与 zip 布局。

## 安装

- 设置 → 插件管理 → 安装本地插件，选择 zip；或
- 手动解压到 `%USERPROFILE%/.ZeroLaunch-rs/plugins/com.ghost-him.everything/` 后重新加载。

## 调试与验证

- **插件日志**：`%USERPROFILE%/.ZeroLaunch-rs/plugin-logs/com.ghost-him.everything.log`（含每次查询的耗时埋点：setup/query/total ms）。
- **CLI 查询**：宿主运行时 `zerolaunch-cli.exe query "ev xxx"` 直查插件响应（`--json` 输出原始 JSON，用于验证 CustomPanel 载荷）。
- **面板改动**：修改 `ui/panel.mjs` 后需重新打包安装/重新加载插件（宿主按 URL 缓存 ESM 模块，重载时旧模块可能残留，必要时重启宿主）。

## 已知限制

- 仅 Windows x86_64：其他平台 `search_everything` 编译为返回空结果（SDK 不可用）。
- Everything 服务未运行时查询返回错误并在面板显示（Everything SDK 查询可能短暂阻塞等待服务）。
