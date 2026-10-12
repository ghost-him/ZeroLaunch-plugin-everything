# ZeroLaunch Everything 搜索插件

ZeroLaunch 第三方插件：通过 Everything SDK 实时搜索本机文件，以沉浸式面板呈现结果。触发词 `ev` / `every`（在候选项里选中进入），热键 `Ctrl+E`（搜索窗打开时生效）。

## 功能

- **Everything 实时检索**：完整路径匹配查询，支持 Everything 原生查询语法（`ext:`、`folder:`、`!` 等，见 Everything 文档）。
- **沉浸式面板**：面板自渲染结果列表——文件类型 emoji 图标、大小、修改时间、扩展名徽标、所在目录；不依赖宿主 List 管线与图标链路。
- **打开与定位**：`Enter` 打开文件；`Ctrl+Enter` 打开选中项所在文件夹；双击或动作菜单打开文件；`open_folder` 动作打开所在文件夹。
- **结果项右键菜单**：右键结果项弹出「打开文件 / 用记事本打开文件 / 打开文件位置 / 复制文件路径」四项。打开文件走宿主 Shell；记事本、资源管理器定位（`explorer /select` 选中文件）、复制路径（写系统剪贴板）由插件进程内的 Windows 调用完成——宿主协议未向插件暴露剪贴板与任意程序启动能力，这三项属纯平台调用。
- **路径匹配开关**：`Ctrl+U` 切换"匹配完整路径而非仅文件名"（等价 Everything 的 Ctrl+U），即时生效，状态回显在面板底栏。
- **短查询优化**：查询长度低于排序阈值时跳过排序直接返回，降低短查询（命中项极多）的延迟。

## 环境要求

- Windows x86_64（Everything SDK 仅提供 64 位 DLL，插件二进制同样仅构建该目标）。
- 需要 Everything 本体（`Everything.exe`）在后台运行——SDK 经 IPC 与本体通信，仅保留 Everything 服务不够（服务只做 NTFS 索引代理，不响应查询）。
- 本体缺失时插件可自动拉起（默认开启）：探测顺序为设置页自定义路径 → 官方默认安装路径 → `PATH` → 随包附带的 `everything.exe`（1.4.1.1032 完整版，非 Lite，裸机兜底）；以后台 `-startup` 无窗口启动，复用系统服务热索引（有服务时 1~2s 就绪）。关闭自启动后仅返回手动启动指引。随包实例由预置 `Everything.ini`（`app_data=0` + `show_tray_icon=0`）实现配置隔离与无托盘图标静默运行；用户自装版沿用其自身配置。
- **随包本体在插件安装目录内运行**（自包含：装上即用，卸载后目录里不留任何运行时产物）——`everything.exe`（1.4.1.1032）、便携语义 `Everything.ini`（`app_data=0` + `show_tray_icon=0`）与它写出的 `Everything.db` 都在插件根。宿主卸载/升级的顺序是「先停插件 → 再删插件目录」，所以**插件进程退出前会把随包本体停掉**：`BundledBodyguard` 发 `-exit` 并等到 `everything.exe` 进程消失（运行中的镜像会让宿主的目录删除报「拒绝访问」）；插件被强杀时另由 Job（`KILL_ON_JOB_CLOSE`）兜底带走随包子进程。用户自装版（自定义路径/安装路径/PATH 命中）绝不被停止。注意宿主的「禁用」只是停止路由、不结束插件进程，因此禁用状态下本体仍在后台运行。
- **本体是「默认实例」共享语义**：插件拉起时不用 `-instance`（1.4 版 SDK 只能连默认实例），所以随包实例占的就是系统默认实例位——它先跑起来时，用户之后双击自己的 Everything 会附到这个默认实例（看到的是随包的便携配置与库）；反之用户版已在跑时，插件只往既有实例发 `-startup`，不会另起进程。想要彻底隔离请在设置里指定用户版 exe 路径（探测优先命中它）。

## 使用

| 操作 | 效果 |
| --- | --- |
| `Ctrl+E` | 唤起沉浸式面板 |
| 搜索栏输入 `ev` / `every`，选中候选项后 `Enter` | 进入沉浸式面板（触发词后跟查询词会让候选项从列表里消失；查询在面板输入框里输入） |
| `↑` / `↓` | 选择结果 |
| `Enter` / 双击 | 打开选中项 |
| `Ctrl+Enter` | 打开选中项所在文件夹 |
| 右键结果项 | 更多操作：打开文件 / 用记事本打开文件 / 打开文件位置 / 复制文件路径（复制的是原样路径，不做分隔符转换） |
| `Ctrl+U` | 切换路径匹配 |
| 空库提示里的「以管理员重建索引」 | 仅随包实例索引确实为空时出现；点两次（第二次为确认）后弹 UAC 提权建库 |
| `Esc` | 返回宿主默认面板（右键菜单打开时，`Esc` 只关闭菜单） |

面板内输入 200ms 防抖后查询（Everything 查询为阻塞调用，避免每键触发）；`Esc` / `Ctrl+U` 由插件 `interaction_policy` bindings 声明，宿主键盘状态机统一解释执行；方向键 / `Enter` / `Ctrl+Enter` 由面板挂在宿主窗口层监听（鼠标点击结果项后焦点落到 body 也不失效），面板卸载时解绑。右键菜单为面板自绘（宿主对第三方面板不提供菜单组件）：面板内右键命中结果项即选中并弹出，点击别处 / 按方向键 / 输入内容 / 窗口失焦均收起。

## 配置

设置页可配置项（`src/plugin.rs` `setting_schema()`）：

| 配置 | 默认 | 说明 |
| --- | --- | --- |
| 排序阈值 | `4` | 查询长度达到该字符数才排序（1~3 字符短查询跳过） |
| 排序方式 | 名称升序 | 26 种排序（名称/路径/大小/扩展名/类型/创建/修改/访问日期等，升序或降序） |
| 结果数量上限 | `10` | 单次查询最大返回数（1–100） |
| 路径匹配 | 关 | 匹配完整路径而非仅文件名（等价 Everything 的 Ctrl+U） |
| 本体缺失时自动启动 | 开 | 本体未运行时后台 `-startup` 拉起并等待就绪；关闭后仅提示手动启动 |
| Everything 路径 | 空（自动探测） | 自定义 exe 路径；仅自启动开启时可见 |

## 架构

```
搜索栏输入 → host.query() → 宿主 bridge_query → 插件 query()
    → Everything SDK（阻塞线程池 + search_guard 串行化）
    → QueryResponse::CustomPanel { panel_type: "everything", data: 自描述 JSON }
    → 面板 onDataUpdate / host.query 响应 → 自渲染列表
Enter → host.executeAction("open", { path }) → 宿主 shell_open
Ctrl+Enter → host.executeAction("open_folder", { path }) → 插件 open_folder → 宿主 shell_open_folder（打开父目录）
右键菜单 → 面板自绘菜单 → host.executeAction(<菜单动作>, { path }) → 插件 execute_action 统一分发：
    打开文件         → 宿主 shell_open
    用记事本打开     → 插件 spawn notepad.exe <path>（参数数组传路径，子进程 stdio 置空）
    打开文件位置     → 插件 spawn explorer.exe /select,"<path>"（raw_arg 保留引号语法）
    复制文件路径     → 插件经 clipboard-win 写系统剪贴板（CF_UNICODETEXT）
空库按钮 → host.executeAction("rebuild_index_elevated") → 插件 launch_everything_elevated
    → runas 提权启动随包 exe 建库（UAC 弹窗；先 `-exit` 退掉同目录普通实例）
```

- **CustomPanel 数据契约**：`panelData` 为自描述 JSON——`query`、`items`（`path`/`name`/`dir`/`isFolder`/`size`/`modified`(unix 秒)/`extension`）、`sortSkipped`、`enablePathMatch`、`resultLimit`、`emptyBundledIndex`（探针确认的「随包实例索引为空」标记）。面板按需自渲染，`keep_search_bar = false`。
- **唤醒重放**：宿主唤起面板时以空查询调用 `query()`，返回的面板数据经 `onDataUpdate` 下发（首次为空列表）；查询词由面板输入框输入后触发。
- **List 回退**：面板 `normalizeResponse` 兼容宿主旧管线返回的 `mode: "search"` 列表形状（title/subtitle/icon），统一为内部 item。
- **SDK 并发约束**：Everything SDK 的 set_search/query/迭代作用于进程内全局状态，全部查询经 `search_guard` 互斥串行化，并在 `tokio::spawn_blocking` 中执行，避免阻塞 RPC 循环。
- **单实例复用**：进程内复用首个 `Everything` 实例（第二个实例的数据库等待会挂死）；每次查询前 `reset()` 清空上次结果状态；`Everything_SetMatchPath` 为 SDK 全局状态，每次查询显式设置保证确定性。
- **随包本体生命周期**：`locate_everything_exe` 按 自定义 → 安装路径 → `PATH` → 随包副本 探测并返回 `BackendExe::{User,Bundled}`（Job 管控与提权入口的唯一判据）；随包命中时把子进程挂进 `KILL_ON_JOB_CLOSE` 的 Job，并在插件退出时由 `BundledBodyguard` 显式 `-exit` + 等 exe 释放。两者叠加保证宿主随后的 `remove_dir_all` 不会撞上"运行中镜像"（否则卸载会因「拒绝访问」失败并把插件留成半删状态）。
- **运行时依赖**：`extra/Everything64.dll`、`extra/everything.exe`、`extra/Everything.lng`、`extra/Everything.ini` 由打包脚本并入 zip 根（插件 exe 在其 `bin/` 子目录，因此 `../` 即插件根），安装后必须保持该布局。

## 项目结构

```
├── Cargo.toml          # 依赖 zerolaunch-plugin-sdk-rust / plugin-api（0.2.2）+ win32job / clipboard-win（Windows）
├── manifest.toml       # 插件清单（必填 [plugin] 元数据 + 运行时命令、热键、面板入口），打包时位于 zip 根
├── icon.svg            # 插件图标（清单 [icon] 声明，随包分发并作为 Release 附件供市场卡片展示）
├── src/main.rs         # 启动骨架（init() + app().run()，随模板同步，尽量别改）
├── src/plugin.rs       # EverythingPlugin（Plugin + Configurable trait 实现）+ app() 装配
├── ui/panel.mjs        # 沉浸式面板（Shadow DOM 内挂载，宿主 CSS 变量自动跟随主题）
├── i18n/               # 语言包（zh-Hans / en，host 加载时合并进翻译目录）
├── extra/              # 随包二进制（并入 zip 根）：Everything64.dll + everything.exe + Everything.lng + Everything.ini
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

产物 `dist/zerolaunch-plugin-everything-v<版本号>.zip`（插件短id = manifest `[plugin].id` 末段，如 `com.ghost-him.everything` → `everything`），zip 布局：`manifest.toml` 位于根、`bin/zerolaunch-plugin-everything.exe`、`ui/`、`i18n/`、`Everything64.dll` + `everything.exe` + `Everything.lng` + `Everything.ini`（extra/ 内容并入根，与插件 exe 同级）。
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
- 本体未运行且自启动关闭（或找不到 exe）时查询返回中文指引并在面板显示，不再 panic（Issue #62）。
- 裸机首次拉起需全盘建库（几十秒到几分钟），15s 内未就绪会提示稍后重试；有服务热索引时 1~2s 即可。
- 「索引为空」提示的判据是一次独立探针查询（`*` 通配命中数为 0），不是「本次搜索没结果」：随包实例索引正常时，普通无匹配不会误提示去提权建库。
- 提权建库启动的实例同样会被停掉：`BundledBodyguard` 的 `-exit` 实测跨完整性级别可用（无需 UAC；退出时会把已建索引落盘），所以卸载/升级/宿主关闭都不受它影响。要单独结束它：普通权限命令行执行插件根里的 `everything.exe -exit`，或管理员 `taskkill /F /IM everything.exe`。它无托盘图标，运行期间会占住插件目录（属预期，插件退出前会送走它）。
- 宿主「禁用」插件只切启用位与路由、不结束插件进程（`plugin_set_enabled` 实现如此），所以禁用状态不会触发停止逻辑，随包本体仍在后台运行；要停它就卸载，或普通权限执行插件根里的 `everything.exe -exit`。
- 索引库 `Everything.db`（提权建库后近百 MB）随包落在插件根，是「自包含」设计的代价：**升级插件会连它一起被替换 → 需要重新提权建库一次**。
- 随包分发 `everything.exe`（voidtools，1.4.1.1032）与 `Everything.lng`、`Everything64.dll`：本插件只做集成，二进制版权与许可归 voidtools，再分发条款以其官网为准。

## 排障

**卸载/升级报「拒绝访问」（os error 5），之后连暂停都报 `COMPONENT_NOT_FOUND`**

宿主卸载的顺序是「先停插件 → 再删插件目录」；插件目录里躺着运行中的 `everything.exe`（镜像文件）时删除返回「拒绝访问」，而卸载在删目录**之前**就已注销组件，于是留下"半删"状态——目录还在（`bin/`、`ui/` 可能已删掉一部分）、组件已从注册表消失（暂停因此报"组件不存在"），`install_from_zip` 见目录已存在还会报 `AlreadyInstalled` 挡住重装。**0.1.2 起插件停止时会把随包本体一起停掉**（普通权限走 Job，提权实例走 `-exit`），正常不会出现。若在 0.1.1 及更早版本撞上，按下面清理残留（**无需 UAC**）：

```powershell
# 让占用插件目录的随包本体退出（-exit 只作用于当前默认实例；退出时会把索引库落盘）
& "$env:USERPROFILE\.ZeroLaunch-rs\plugins\com.ghost-him.everything\everything.exe" -exit
# 删掉残留插件目录（目录存在时重装会被 AlreadyInstalled 挡住）
Remove-Item -Recurse -Force "$env:USERPROFILE\.ZeroLaunch-rs\plugins\com.ghost-him.everything"
```

之后重启 ZeroLaunch 再重新安装插件。
