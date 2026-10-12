//! Everything 搜索插件实现——**业务文件**：不参与模板同步（`.templatesyncignore` 排除 `src/plugin.rs`）。
//!
//! 照常实现宿主的 `Plugin` / `Configurable` trait；启动骨架（`src/main.rs`）只负责 `init()` + `app().run()`。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tracing::{debug, info};
use zerolaunch_plugin_api::config::{
    ComponentCore, ComponentType, ConfigError, Configurable, FieldUiMetadata, PathMode, SchemaKind,
    SchemaNode, SettingDefinition, VisibleWhen, WidgetHint,
};
use zerolaunch_plugin_api::services::icon_request::IconRequest;
use zerolaunch_plugin_api::{
    PanelInteraction, PanelKeyAction, PanelKeyBinding, Plugin, PluginContext, PluginError,
    PluginHandle, Query, QueryResponse, ResultAction,
};
use zerolaunch_plugin_sdk_rust::{host, PluginApp, t_key};

/// Everything 搜索插件 — 触发词 ev / every 唤起，查询 Everything 服务并返回文件列表。
///
/// 平台注意：Everything SDK 的 set_search/query/迭代作用于进程内全局状态，
/// 并发调用会互相污染，故全部查询经 `search_guard` 串行化并在阻塞线程池执行
/// （Everything 服务未运行时查询可能短暂阻塞）。Everything64.dll 由打包脚本
/// 从 extra/ 目录并入 zip 根，运行时必须与插件 exe 同目录。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use everything_rs::{Everything, EverythingError, EverythingRequestFlags, EverythingSDKError, EverythingSort};

/// 全部排序方式（与 everything-rs `EverythingSort` 枚举一一对应；
/// 配置值字符串序列化与旧版 EverythingConfig 兼容）。
const SORT_METHODS: [&str; 26] = [
    "NameAscending",
    "NameDescending",
    "PathAscending",
    "PathDescending",
    "SizeAscending",
    "SizeDescending",
    "ExtensionAscending",
    "ExtensionDescending",
    "TypeNameAscending",
    "TypeNameDescending",
    "DateCreatedAscending",
    "DateCreatedDescending",
    "DateModifiedAscending",
    "DateModifiedDescending",
    "AttributesAscending",
    "AttributesDescending",
    "FileListFilenameAscending",
    "FileListFilenameDescending",
    "RunCountAscending",
    "RunCountDescending",
    "DateRecentlyChangedAscending",
    "DateRecentlyChangedDescending",
    "DateAccessedAscending",
    "DateAccessedDescending",
    "DateRunAscending",
    "DateRunDescending",
];

/// Everything 查询参数快照——承载排序/数量/路径匹配与本体自启动开关的生效值。
///
/// 仅本插件进程内使用：`query()` 加读锁克隆后传入 `search_everything`（spawn_blocking 闭包）；
/// 宿主侧仅经 `setting_schema` / `get_settings` / `apply_settings` 读写，不直接持有本类型。
#[derive(Clone)]
struct EverythingState {
    /// 查询长度达到该值时才设置排序（字符数；短查询跳过排序以降低延迟）。
    sort_threshold: usize,
    /// 排序方式（SORT_METHODS 中的值）。
    sort_method: String,
    /// 单次查询返回的最大结果数。
    result_limit: usize,
    /// 路径匹配：匹配完整路径而非仅文件名（等价 Everything 的 Ctrl+U）。
    enable_path_match: bool,
    /// 本体未运行时是否自动启动 Everything.exe（无感拉起 IPC→exe→服务链路）。
    auto_launch_everything: bool,
    /// Everything.exe 自定义路径（空串表示自动探测：随包附带→常见安装路径→PATH）。
    everything_exe_path: String,
}

impl Default for EverythingState {
    fn default() -> Self {
        Self {
            // 默认 4：1~3 字符的短查询匹配项极多，跳过排序直接返回原始结果
            sort_threshold: 4,
            sort_method: "NameAscending".to_string(),
            result_limit: 10,
            enable_path_match: false,
            // 默认开启：Issue #62 的用户本体缺失时可无感恢复，无需手动干预。
            auto_launch_everything: true,
            // 空串 = 自动探测，不写死路径以适配安装版/便携版混用。
            everything_exe_path: String::new(),
        }
    }
}

/// Everything SDK 实例槽（进程内复用同一实例）。
/// dll 的数据库等待在进程内第二个实例上挂死，连续查询必须复用首个实例。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
struct EverythingSlot(Mutex<Option<Everything>>);

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
struct EverythingSlot;

impl EverythingSlot {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    fn new() -> Self {
        Self
    }
}
/// Windows Job 守卫——插件拉起的随包本体跟随插件退出。
///
/// 仅限本文件内使用：由 `EverythingPlugin` 持有（与插件进程同寿命）。
/// `KILL_ON_JOB_CLOSE` 语义下，最后一个 Job 句柄关闭即杀光 Job 内进程——正常路径是
/// 守卫 `Drop`（插件 stdin EOF 退出），强杀路径由内核回收被终止进程的句柄触发。注意：
/// - 仅随包 exe（第 4 级探测命中）挂入本 Job；用户自装版绝不碰；
/// - 提权（runas）进程挂不进普通权限 Job（完整性隔离），提权建库路径不管它；
/// - 本体若已运行则拉起调用秒退（无新进程），此时挂 Job 是空操作。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
struct JobGuard {
    /// KILL_ON_JOB_CLOSE 的 Job；`None` = 创建失败，本次运行不管控随包本体（降级不报错）。
    /// 句柄所有权与关闭由 `win32job::Job` 的 `Drop` 承担。
    job: Option<win32job::Job>,
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
impl JobGuard {
    /// 创建 KILL_ON_JOB_CLOSE 的 Job；失败时 job 为 None（调用方降级为不管控）。
    /// 注意 `win32job::Job::create()` 不带任何 limit，必须走 `create_with_limit_info`。
    fn new() -> Self {
        let mut info = win32job::ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        Self { job: win32job::Job::create_with_limit_info(&info).ok() }
    }

    /// 把已启动的子进程挂入 Job；Job 创建失败时空操作。
    /// 调用方需在 `Command::spawn` 成功后尽早调用——本体若转交既有实例会秒退，
    /// 此时进程已消失，Assign 必然失败，属预期（只告警不报错）。
    fn assign(&self, child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        let Some(job) = &self.job else { return };
        if let Err(e) = job.assign_process(child.as_raw_handle() as isize) {
            debug!(error = %e, "随包本体挂入 Job 失败（进程可能已退出）");
        }
    }
}

/// Everything 查询结果项（文件/目录元信息快照，面板按此自渲染）。
#[derive(Debug, Clone, Serialize)]
struct SearchResult {
    /// 完整路径（动作执行时经 payload 直传）。
    #[serde(rename = "path")]
    path: String,
    /// 文件名（含扩展名）。
    #[serde(rename = "name")]
    name: String,
    /// 父目录路径（无父目录时为空，如卷根）。
    #[serde(rename = "dir")]
    dir: String,
    /// 是否目录（含卷）。
    #[serde(rename = "isFolder")]
    is_folder: bool,
    /// 文件大小（字节；目录无值）。
    #[serde(rename = "size")]
    size: Option<u64>,
    /// 修改时间（unix 秒；SDK 未返回时无值）。
    #[serde(rename = "modified")]
    modified: Option<u64>,
    /// 扩展名（小写、不带点；目录为空）。
    #[serde(rename = "extension")]
    extension: String,
}

/// FILETIME（100ns 间隔，1601-01-01 纪元）转为 unix 秒。
fn file_time_to_unix(file_time: u64) -> u64 {
    (file_time / 10_000_000).saturating_sub(11_644_473_600)
}

/// 拆分完整路径为文件名与父目录；无分隔符时父目录为空。
fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('\\') {
        Some((dir, name)) => (name.to_string(), dir.to_string()),
        None => (path.to_string(), String::new()),
    }
}

/// 随包附带的 Everything.exe 文件名（`extra/everything.exe` 打包后位于插件根）。
/// 同目录的预置 `Everything.ini`（`extra/Everything.ini`）使其以便携语义启动：
/// `app_data=0`（配置隔离，不碰 %APPDATA%）、`show_tray_icon=0`（无托盘图标静默运行）。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const BUNDLED_EVERYTHING_EXE: &str = "everything.exe";

/// 拉起后等待就绪的最长时长：有服务热索引时本体 1~2s 即可 `IsDBLoaded`。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const LAUNCH_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 让随包本体退出时的等待上限：必须明显短于宿主关停插件的 5s 超时
/// （`PluginProcess::shutdown(Duration::from_secs(5))`），否则插件会被强杀。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const STOP_EVERYTHING_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// 插件安装根目录（插件 exe 位于 `<root>/bin/`，随包本体与 dll/ini 位于 `<root>/`）。
///
/// 仅限本文件内使用：随包本体的准备入口与提权围栏共用这一处根目录判定。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn plugin_root() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|bin| bin.parent()).map(|r| r.to_path_buf()))
}

/// 探测命中的本体来源——Job 管控与「提权建库」入口的唯一判据（不再由路径反推）。
///
/// 仅限本文件内使用：`ensure_backend_ready()` 产出，据此决定是否挂 Job、
/// 是否写 `bundled_backend` 标志。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
enum BackendExe {
    /// 用户自己的 Everything（设置页路径 / 官方安装路径 / PATH）——绝不挂 Job、绝不提权。
    User(std::path::PathBuf),
    /// 随包本体在运行目录（插件安装目录之外）的副本——挂 Job 跟随插件退出，可提权建库。
    Bundled(std::path::PathBuf),
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
impl BackendExe {
    /// 命中路径（动作/日志使用）。
    fn path(&self) -> &std::path::Path {
        match self {
            BackendExe::User(p) | BackendExe::Bundled(p) => p,
        }
    }

    /// 是否随包本体（Job 管控 + 提权建库的唯一判据）。
    fn is_bundled(&self) -> bool {
        matches!(self, BackendExe::Bundled(_))
    }
}

/// 定位可用的 Everything.exe：用户自装版优先（自定义路径 → 官方安装路径 → PATH），
/// 都没有时才用插件安装目录里随包附带的副本（裸机兜底，与默认实例同配置，首次需建库）。
///
/// 顺序含义：用户自己的 exe 带用户配置与已有索引，优先级最高；随包副本只作兜底。
/// 注意旧 SDK（1.4 时代绑定）只能连默认实例，故不使用 `-instance` 命名实例隔离。
/// 仅限本文件内使用：`ensure_backend_ready()` 调用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn locate_everything_exe(configured: &str) -> Option<BackendExe> {
    use std::path::PathBuf;
    // 1. 用户在设置页手动指定的路径（最高优先级）。
    let custom = configured.trim().trim_matches('"');
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(BackendExe::User(p));
        }
        debug!(path = %custom, "配置的 Everything 路径不存在，继续自动探测");
    }
    // 2. 官方默认安装路径（64 位 / 32 位 Program Files）。
    for dir in [
        r"C:\Program Files\Everything\Everything.exe",
        r"C:\Program Files (x86)\Everything\Everything.exe",
    ] {
        let p = PathBuf::from(dir);
        if p.is_file() {
            return Some(BackendExe::User(p));
        }
    }
    // 3. PATH 中的 Everything.exe（便携版用户常加 PATH）。
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("Everything.exe");
            if p.is_file() {
                return Some(BackendExe::User(p));
            }
        }
    }
    // 4. 随包副本（插件安装根，与插件 exe 的 `bin/` 同级）。
    let bundled = plugin_root()?.join(BUNDLED_EVERYTHING_EXE);
    if bundled.is_file() {
        return Some(BackendExe::Bundled(bundled));
    }
    None
}

/// 本体缺失时的中文指引（未开启自启动 / 自启动失败 / 压根找不到 exe 时展示）。
///
/// 仅限本文件内使用：`ensure_backend_ready()` 构造、`search_everything()` 经 Err 透出、
/// 最终由 `query()` 转为 `PluginError::QueryFailed` 在面板展示。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn everything_missing_hint() -> String {
    "未检测到 Everything 本体（Everything.exe 未运行），SDK 无法查询。".to_string()
        + "仅保留 Everything 服务不够——索引服务不响应查询，本体必须在后台运行。"
        + "解决办法：启动 Everything（托盘常驻即可）；或在插件设置中开启「本体缺失时自动启动」。"
}

/// 检查 Everything IPC 是否就绪：数据库已加载即返回 true。
///
/// 仅限本文件内使用：`ensure_backend_ready()` 与拉起后轮询复用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn is_ipc_ready() -> bool {
    // SAFETY: 纯查询调用，无回调、无写入，与 SDK 单线程约束不冲突（调用方已持串行锁）。
    unsafe { everything_sys_bindgen::Everything_IsDBLoaded() != 0 }
}

/// 后台静默启动 Everything 本体：`-startup` 无窗口，仅后台常驻并创建 IPC 窗口。
///
/// 托盘图标由 exe 同目录 `Everything.ini` 的 `show_tray_icon` 决定：随包实例因预置
/// `show_tray_icon=0` 无图标静默运行；用户自装版沿用其自身配置，插件不干预。
/// 仅限本文件内使用：`ensure_backend_ready()` 在确认 IPC 缺失且允许自启动时调用。
/// 本体若已运行则该调用秒退（转交参数给既有实例），天然幂等、无需预检进程。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn launch_everything_client(exe: &std::path::Path) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    // CREATE_NO_WINDOW: 不闪控制台。刻意不加 DETACHED_PROCESS：随包本体由插件进程的
    // Job（JobGuard，KILL_ON_JOB_CLOSE）管控跟随退出，脱离父进程的会话没有意义
    // （Windows 本来就不因父进程退出而终止子进程，父子关系与 Job 是两件事）。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(exe)
        .arg("-startup")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动 Everything 失败（{}）：{e}", exe.display()))
}

/// 以管理员权限启动插件安装目录里的随包本体建库（弹 UAC，需用户点“是”）。
///
/// 背景：裸机（无服务）普通权限读不到 NTFS USN，随包实例的索引永远建不起来；
/// 静默提权不存在，必须走 `runas` 过 UAC。
/// 提权目标只可能是 `BundledBodyguard` 记录的随包 exe（安装根的编译期子路径），
/// 动作载荷无法影响它，故没有"诱导提权任意程序"的入口——`debug_assert!` 守住该不变量。
/// 该实例不在 Job 内（跨完整性级别挂不进），由插件退出时的 `BundledBodyguard`
/// （`-exit` + 等 exe 释放）负责送走，保证宿主随后的目录删除不会撞上运行中镜像。
/// 实测 UIPI 放行普通权限 SDK 查询提权本体，建库中也可查（结果随建库增长）。
/// 仅限本文件内使用：`execute_action(rebuild_index_elevated)` 调用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn launch_everything_elevated(exe: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    debug_assert_eq!(
        plugin_root().as_deref(),
        exe.parent(),
        "提权目标必须是插件安装根里的随包 exe"
    );
    // 提权前先让当前默认实例退出：Everything 单实例语义下，已运行的实例会吞掉
    // runas 的 `-startup`（转交参数后秒退），导致建库根本没发生。
    stop_everything_process(exe, STOP_EVERYTHING_WAIT);
    // 返回值 >32 成功；最常见失败 ERROR_CANCELLED(1223)=用户点“否”，直接转中文。
    let verb: Vec<u16> = std::ffi::OsStr::new("runas").encode_wide().chain([0]).collect();
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let params: Vec<u16> = std::ffi::OsStr::new("-startup").encode_wide().chain([0]).collect();
    // SAFETY: 宽串以 NUL 结尾且在调用期间存活；ShellExecuteW 同步返回，无回调。
    let ret = unsafe {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        ShellExecuteW(
            std::ptr::null_mut::<std::ffi::c_void>() as HWND,
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        )
    };
    if (ret as usize) <= 32 {
        return Err("提权启动被取消（UAC 未确认），索引仍为空".to_string());
    }
    info!(exe = %exe.display(), "已提权启动随包本体建库（UAC 已确认）");
    Ok(())
}

/// 让正在运行的随包本体退出，并等它真正退出（插件目录才删得掉）。
///
/// `-exit` 只作用于**当前默认实例**（与发送方是不是同一个 exe 无关），且实测可跨完整性
/// 级别生效：普通权限进程也能结束提权实例，因此不需要管理员。
/// 前置判断：没有 `everything.exe` 进程在跑就什么都不做——此时发 `-exit` 反而会另起一个。
/// 等待判据是进程消失：进程终止时内核会释放它的镜像映射与文件句柄，那正是宿主
/// `remove_dir_all` 需要的条件（运行中的镜像会让删除返回「拒绝访问」，插件目录便删不掉）。
/// 超时只告警：`-exit` 已发出，实例最终仍会退出。
/// 仅限本文件内使用：`BundledBodyguard::drop` 与 `launch_everything_elevated()` 调用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn stop_everything_process(exe: &Path, wait: std::time::Duration) {
    use std::process::{Command, Stdio};
    if !exe.is_file() || !everything_process_running() {
        return;
    }
    info!(exe = %exe.display(), "随包本体仍在运行，请求退出并等待其结束");
    let _ = Command::new(exe)
        .arg("-exit")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = std::time::Instant::now() + wait;
    while everything_process_running() {
        if std::time::Instant::now() >= deadline {
            tracing::warn!(exe = %exe.display(), "等待随包本体退出超时，插件目录可能仍被占用");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // 进程刚消失时宿主可能立刻来删目录：给内核收尾留一点余量。
    std::thread::sleep(std::time::Duration::from_millis(100));
}

/// 是否仍有 `everything.exe` 进程在运行（Toolhelp 按镜像名枚举，提权实例同样可见）。
///
/// 不用 SDK：本函数在插件退出路径上工作，此处 SDK 全局状态可能已被清理（见
/// `BundledBodyguard::drop`）。也不用"以 DELETE 权限打开 exe"这类探测——实测运行中的
/// 镜像允许被以 DELETE 权限打开（映射阻止的是删除本身，不是打开），判不出占用。
/// 只按镜像名匹配，故调用方须用 `owned` 标志保证"只在随包实例是后端时才停止"。
/// 仅限本文件内使用：`stop_everything_process()` 的前置判断与等待判据。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn everything_process_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: 快照句柄用完即关；PROCESSENTRY32W 按 API 契约先零初始化并填 dwSize。
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if name.eq_ignore_ascii_case(BUNDLED_EVERYTHING_EXE) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

/// 插件退出时把随包本体一起停掉——「先停插件、再删插件目录」这一约定里，插件负责前者。
///
/// 宿主卸载/升级的顺序是：关闭 stdin 等插件退出（5s 超时后强杀）→ `remove_dir_all`。
/// 插件目录里躺着运行中的 `everything.exe`（镜像文件）时，删除会返回「拒绝访问」并把
/// 插件留成半删状态（组件已注销、目录还在），因此必须在**插件退出前**把本体送走：
/// - 普通权限那份另有 `JobGuard` 的 Job 兜底（关闭 Job 句柄即被杀，插件被强杀也生效）；
/// - 提权实例跨完整性级别挂不进 Job，只能在这里显式 `-exit` 并等它释放 exe。
/// 仅由 `EverythingPlugin` 持有（与插件同寿命，声明在 `job_guard` 之前 → 先优雅退出）。
struct BundledBodyguard {
    /// 插件安装根里的随包 exe：`-exit` 的发送方，也是"是否还在跑"的等待判据；
    /// `None` = 拿不到插件安装根（异常环境），此时不做任何动作。
    exe: Option<std::path::PathBuf>,
    /// 后端是否由本插件拉起的随包实例（与 `EverythingPlugin::bundled_backend` 同一份标志）：
    /// 为假说明默认实例可能是用户自装版，插件停止绝不能碰它。
    owned: Arc<AtomicBool>,
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
impl BundledBodyguard {
    /// 插件安装根里的随包 exe 路径（`None` = 拿不到插件安装根）。
    /// 「提权建库」入口直接用它——路径只来自插件自身构造的安装根子路径，不含用户输入。
    fn exe(&self) -> Option<&Path> {
        self.exe.as_deref()
    }
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
impl Drop for BundledBodyguard {
    fn drop(&mut self) {
        if !self.owned.load(Ordering::SeqCst) {
            return;
        }
        let Some(exe) = &self.exe else { return };
        // 刻意不调用任何 SDK 函数：`everything_slot` 字段先于本守卫析构
        // （`Everything::drop` → SDK `CleanUp`），此时再调 `Everything_IsDBLoaded` 会挂住，
        // 插件进程被宿主 5s 超时强杀（退出码 1），本体也就没被停掉。占用判断交给文件系统。
        stop_everything_process(exe, STOP_EVERYTHING_WAIT);
    }
}

/// 确保后端就绪：就绪直返；IPC 缺失且允许自启动则拉起后等待；否则返回中文指引。
///
/// 仅限本文件内使用：在 `spawn_blocking` 内、持串行锁后调用（SDK 全局状态要求串行）。
/// 成功返回 `Ok(())`（可继续查询）；失败返回 `Err(中文原因)`，调用方直接透出面板。
/// `bundled_backend` 由本函数写入（拉起时按探测命中写一次），供 `query()` 判是否提示提权建库。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn ensure_backend_ready(
    state: &EverythingState,
    job: &JobGuard,
    bundled_backend: &AtomicBool,
) -> Result<(), String> {
    use std::time::Instant;
    if is_ipc_ready() {
        return Ok(());
    }
    // SAFETY: 仅读取错误码，与查询路径互斥（调用方持锁）。
    let last_err = unsafe { everything_sys_bindgen::Everything_GetLastError() };
    // 非 IPC 错误（如数据库仍在加载 OK=0）：不拉新实例，直接短轮询等本体就绪。
    if last_err != everything_sys_bindgen::EVERYTHING_ERROR_IPC {
        debug!(last_err, "IPC 通道暂未就绪（非缺失），短轮询等待本体加载");
        let deadline = Instant::now() + LAUNCH_READY_TIMEOUT;
        while Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(300));
            if is_ipc_ready() {
                return Ok(());
            }
        }
        return Err("Everything 数据库仍在加载，稍后重试。".to_string());
    }
    // IPC 缺失 = 本体未运行。
    if !state.auto_launch_everything {
        info!("检测到 Everything 本体未运行，但自启动开关关闭，返回指引");
        return Err(everything_missing_hint());
    }
    let Some(hit) = locate_everything_exe(&state.everything_exe_path) else {
        info!("检测到 Everything 本体未运行，且未找到可用的 Everything.exe，返回指引");
        return Err(everything_missing_hint() + "当前未找到 Everything.exe：请安装官方版或在设置中指定路径。");
    };
    // 是否随包命中：仅随包本体挂入插件 Job 跟随退出；用户自装版绝不管控。
    let bundled = hit.is_bundled();
    let exe = hit.path();
    // 记下本次生效的后端归属：面板的「提权建库」入口只对随包实例显示。
    bundled_backend.store(bundled, Ordering::SeqCst);
    info!(exe = %exe.display(), bundled, "检测到本体未运行，尝试后台拉起");
    let child = launch_everything_client(exe)?;
    if bundled {
        // 尽早挂入 Job：本体若转交既有实例秒退，assign 是空操作（进程已退出则 Assign 失败）。
        job.assign(&child);
    }
    // 拉起后轮询：有服务热索引时 1~2s 就绪；裸机首次建库可能更久，15s 兜底。
    let deadline = Instant::now() + LAUNCH_READY_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if is_ipc_ready() {
            info!("Everything 本体拉起成功，IPC 就绪");
            return Ok(());
        }
    }
    Err("已尝试启动 Everything，但 IPC 仍未就绪（首次索引可能较慢），请稍后重试。".to_string())
}

/// Everything 插件运行时状态与宿主交互能力。
///
/// 仅由本插件进程使用，保存查询配置、结果缓存、串行查询锁和宿主主题状态。
struct EverythingPlugin {
    /// 组件 ID、名称、类型等基础元数据（`Configurable` trait 默认实现委托于此）。
    core: ComponentCore,
    /// 可配置状态。
    state: RwLock<EverythingState>,
    /// Everything SDK 实例（进程内复用一份）。
    everything_slot: Arc<EverythingSlot>,
    /// Everything SDK 全局状态串行化锁（跨 spawn_blocking 共享）。
    search_guard: Arc<Mutex<()>>,
    /// 随包本体停止守卫（与插件同寿命）：插件退出时把随包本体一起送走，宿主随后的
    /// 插件目录删除才不会撞上"运行中镜像"（拒绝访问）。声明在 `job_guard` 之前，
    /// 使其 Drop（优雅 `-exit` + 等 exe 释放）先于 Job 关闭（强杀兜底）执行。
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    bodyguard: Arc<BundledBodyguard>,
    /// 随包本体进程归属守卫（与插件同寿命；Drop 关闭 Job 即杀光 Job 内本体）。
    /// 仅随包命中时挂入；用户自装版/提权进程不管控。
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    job_guard: Arc<JobGuard>,
    /// 当前后端是否为插件拉起的随包实例（`ensure_backend_ready` 是唯一写入者，
    /// 拉起时按探测命中写一次）。`query()` 据此决定是否提示「提权建库」，
    /// 免去每次查询重复探测 exe 位置（同步文件系统 I/O）；`BundledBodyguard`
    /// 也读它来决定插件停止时可否让默认实例退出（为假 = 默认实例可能是用户自装版）。
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    bundled_backend: Arc<AtomicBool>,
}

impl EverythingPlugin {
    fn new() -> Self {
        // 随包后端归属标志：`ensure_backend_ready` 写、`query()` 与停止守卫读，共用一份。
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        let bundled_backend = Arc::new(AtomicBool::new(false));
        Self {
            core: ComponentCore::new(
                "com.ghost-him.everything".to_string(),
                "Everything 搜索".to_string(),
                "Everything 文件搜索集成".to_string(),
                ComponentType::Plugin,
                100,
            ),
            state: RwLock::new(EverythingState::default()),
            everything_slot: Arc::new(EverythingSlot::new()),
            search_guard: Arc::new(Mutex::new(())),
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            bodyguard: Arc::new(BundledBodyguard {
                exe: plugin_root().map(|root| root.join(BUNDLED_EVERYTHING_EXE)),
                owned: Arc::clone(&bundled_backend),
            }),
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            job_guard: Arc::new(JobGuard::new()),
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            bundled_backend,
        }
    }
}

/// 一次查询的结果与面板所需的附加标记（同一次探测内产生，调用方不再二次探测）。
///
/// 仅限本文件内使用：`search_everything()` 产出、`query()` 消费并序列化进 CustomPanel.data。
struct SearchOutcome {
    /// 结果项列表（面板渲染主体）；为空表示本次查询无匹配。
    items: Vec<SearchResult>,
    /// 是否「随包实例 + 索引里一个条目都没有」——面板据此渲染「以管理员重建索引」入口。
    /// 判据来自独立探针查询（`*` 命中数为 0），不是「本次查询无结果」。
    bundled_index_empty: bool,
}

/// 探针：当前索引内是否一个条目都没有（区分「本次查询无匹配」与「随包实例没建库」）。
///
/// 以 `*` 通配查询（`max=1` 只物化一条，总命中数由应答头给出），调用方需持串行锁。
/// 查询失败按「非空」处理——宁可少提示一次，也不把无匹配误报成「索引为空」。
/// 仅限本文件内使用：`search_everything()` 主查询无结果时调用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn index_is_empty(everything: &Everything) -> bool {
    everything.reset();
    everything.set_search("*");
    everything.set_request_flags(EverythingRequestFlags::FullPathAndFileName);
    everything.set_max_results(1);
    if everything.query().is_err() {
        return false;
    }
    // SAFETY: 只读上次查询的应答统计；调用方持串行锁，与其它 SDK 调用互斥。
    unsafe { everything_sys_bindgen::Everything_GetTotResults() == 0 }
}

/// 执行一次 Everything 查询，返回结果项与面板标记。
/// 查询长度低于排序阈值时跳过排序（短查询匹配项极多，排序开销大）。
/// 在 spawn_blocking 中调用；本体未运行时按配置尝试拉起，失败返回中文原因。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn search_everything(
    guard: &Mutex<()>,
    slot: &EverythingSlot,
    search_term: &str,
    state: &EverythingState,
    job: &JobGuard,
    bundled_backend: &AtomicBool,
) -> Result<SearchOutcome, String> {
    let query_started = std::time::Instant::now();
    let _guard = guard.lock();
    let query_length = search_term.chars().count();
    let should_sort = query_length >= state.sort_threshold;

    debug!(
        query_length,
        result_limit = state.result_limit,
        should_sort,
        "开始 Everything 查询"
    );

    // 先确保后端就绪：本体缺失时按配置拉起（IPC→exe→服务），失败直接返回中文原因。
    // 注意：`Everything` 是零字段标记类型，但带有 `Drop::CleanUp`——多建多 Drop 会
    // 反复清理 SDK 全局状态，故仍经 `slot` 复用单例（与原有复用语义一致）。
    // `everything-rs` 的 `new()` 内含 `.expect()`（Issue #62 的 panic 来源），此处绝不调用。
    ensure_backend_ready(state, job, bundled_backend)?;
    let mut slot_guard = slot.0.lock();
    if slot_guard.is_none() {
        *slot_guard = Some(Everything);
    }
    let everything = slot_guard.as_ref().expect("Everything 实例初始化失败");
    everything.reset();

    everything.set_search(search_term);
    // FullPathAndFileName 已包含完整路径和文件名（包括扩展名）；
    // Size/DateModified/Extension 供面板渲染元信息列。
    everything.set_request_flags(
        EverythingRequestFlags::FullPathAndFileName
            | EverythingRequestFlags::Size
            | EverythingRequestFlags::DateModified
            | EverythingRequestFlags::Extension,
    );
    everything.set_max_results(state.result_limit as u32);

    // 路径匹配是 SDK 全局状态，每次查询显式设置保证确定性（1 = 匹配路径）
    unsafe {
        everything_sys_bindgen::Everything_SetMatchPath(if state.enable_path_match {
            1
        } else {
            0
        });
    }

    if should_sort {
        let sort = parse_sort(&state.sort_method);
        everything.set_sort(sort);
    }

    let query_started_at = query_started.elapsed();
    everything.query().map_err(|e| match &e {
        // 查询中途本体退出：Ipc 错误转中文指引，其余保留原文便于排查。
        EverythingError::SDKError(EverythingSDKError::Ipc) => everything_missing_hint(),
        other => format!("Everything 查询失败: {other:?}"),
    })?;
    let query_elapsed = query_started.elapsed();
    let mut out = Vec::with_capacity(everything.get_num_results() as usize);
    for i in 0..everything.get_num_results() {
        let Ok(path) = everything.get_result_full_path(i) else {
            continue;
        };
        let is_folder = everything.is_result_folder(i) || everything.is_result_volume(i);
        let extension = if is_folder {
            String::new()
        } else {
            everything.get_result_extension(i).unwrap_or_default()
        };
        let (name, dir) = split_path(&path);
        out.push(SearchResult {
            path,
            name,
            dir,
            is_folder,
            size: if is_folder {
                None
            } else {
                everything.get_result_size(i).ok()
            },
            modified: everything
                .get_result_count_modified_date(i)
                .ok()
                .map(file_time_to_unix),
            extension,
        });
    }
    info!(
        query_length,
        should_sort,
        result_count = out.len(),
        setup_ms = query_started_at.as_millis() as u64,
        query_ms = query_elapsed.as_millis() as u64,
        total_ms = query_started.elapsed().as_millis() as u64,
        "Everything 查询完成"
    );
    // 零结果本身不构成「索引为空」的证据：只在后端确为随包实例且本次无匹配时，
    // 才补一次索引探针，区分「无匹配」与「随包实例还没建库」。
    let bundled_index_empty =
        out.is_empty() && bundled_backend.load(Ordering::SeqCst) && index_is_empty(everything);
    if bundled_index_empty {
        info!("随包实例索引为空（探针查询 0 命中），面板将提示提权建库");
    }
    Ok(SearchOutcome { items: out, bundled_index_empty })
}

/// 非 Windows x86_64 平台：Everything SDK 不可用，返回空结果。
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn search_everything(
    _guard: &Mutex<()>,
    _slot: &EverythingSlot,
    _search_term: &str,
    _state: &EverythingState,
    _bundled_backend: &AtomicBool,
) -> Result<SearchOutcome, String> {
    Ok(SearchOutcome { items: Vec::new(), bundled_index_empty: false })
}

/// 排序方式配置字符串 → EverythingSort 映射；未知值回退到名称升序。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn parse_sort(kind: &str) -> EverythingSort {
    match kind {
        "NameAscending" => EverythingSort::NameAscending,
        "NameDescending" => EverythingSort::NameDescending,
        "PathAscending" => EverythingSort::PathAscending,
        "PathDescending" => EverythingSort::PathDescending,
        "SizeAscending" => EverythingSort::SizeAscending,
        "SizeDescending" => EverythingSort::SizeDescending,
        "ExtensionAscending" => EverythingSort::ExtensionAscending,
        "ExtensionDescending" => EverythingSort::ExtensionDescending,
        "TypeNameAscending" => EverythingSort::TypeNameAscending,
        "TypeNameDescending" => EverythingSort::TypeNameDescending,
        "DateCreatedAscending" => EverythingSort::DateCreatedAscending,
        "DateCreatedDescending" => EverythingSort::DateCreatedDescending,
        "DateModifiedAscending" => EverythingSort::DateModifiedAscending,
        "DateModifiedDescending" => EverythingSort::DateModifiedDescending,
        "AttributesAscending" => EverythingSort::AttributesAscending,
        "AttributesDescending" => EverythingSort::AttributesDescending,
        "FileListFilenameAscending" => EverythingSort::FileListFilenameAscending,
        "FileListFilenameDescending" => EverythingSort::FileListFilenameDescending,
        "RunCountAscending" => EverythingSort::RunCountAscending,
        "RunCountDescending" => EverythingSort::RunCountDescending,
        "DateRecentlyChangedAscending" => EverythingSort::DateRecentlyChangedAscending,
        "DateRecentlyChangedDescending" => EverythingSort::DateRecentlyChangedDescending,
        "DateAccessedAscending" => EverythingSort::DateAccessedAscending,
        "DateAccessedDescending" => EverythingSort::DateAccessedDescending,
        "DateRunAscending" => EverythingSort::DateRunAscending,
        "DateRunDescending" => EverythingSort::DateRunDescending,
        _ => EverythingSort::NameAscending,
    }
}

/// 用记事本打开指定文件（Windows）。
///
/// 直接 spawn `notepad.exe` 并以参数数组传路径，不经 shell 拼接（避免路径中的
/// 空格/`&`/`(` 等字符被解释）。子进程 stdio 全部置空：插件 stdout 是 JSON-RPC
/// 管道，GUI 进程继承写端会拖住宿主侧的管道关闭。
#[cfg(target_os = "windows")]
fn open_with_notepad(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if !Path::new(path).exists() {
        return Err(format!("文件不存在: {path}"));
    }

    Command::new("notepad.exe")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 在资源管理器中打开文件所在目录并选中该文件（Windows）。
///
/// 用 `raw_arg` 传原始片段：`explorer` 的 `/select,` 后必须紧跟带引号的路径，
/// 而 Rust 默认的参数转义会再包一层引号破坏该语法。explorer 成功时也返回
/// 退出码 1，故只判 spawn 是否成功、不等待子进程。
#[cfg(target_os = "windows")]
fn reveal_in_explorer(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    if !Path::new(path).exists() {
        return Err(format!("文件不存在: {path}"));
    }

    // Everything 返回的是 `\` 分隔路径；兼容上游可能带 `/` 的情况
    let windows_path = path.replace('/', "\\");

    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{windows_path}\""))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 把文本写入系统剪贴板（Windows，`CF_UNICODETEXT`）。
///
/// 宿主协议未向第三方插件暴露剪贴板能力（`host/` 方法表里没有剪贴板端点，
/// `PluginHandle::set_clipboard_text` 只在宿主进程内可用），故由插件进程经
/// `clipboard-win` 直写 Win32。
#[cfg(target_os = "windows")]
fn set_clipboard_text(text: &str) -> Result<(), String> {
    // clipboard-win 负责 OpenClipboard/GlobalAlloc/SetClipboardData/CloseClipboard
    // 全套（含占用重试与失败路径释放），比手写 Win32 少一整类资源泄漏。
    clipboard_win::set_clipboard_string(text).map_err(|e| format!("写入剪贴板失败: {e}"))
}

/// 非 Windows 平台：Everything 插件本身依赖 Windows SDK，三项平台动作直接拒绝。
#[cfg(not(target_os = "windows"))]
fn open_with_notepad(_path: &str) -> Result<(), String> {
    Err("仅 Windows 支持".to_string())
}

#[cfg(not(target_os = "windows"))]
fn reveal_in_explorer(_path: &str) -> Result<(), String> {
    Err("仅 Windows 支持".to_string())
}

#[cfg(not(target_os = "windows"))]
fn set_clipboard_text(_text: &str) -> Result<(), String> {
    Err("仅 Windows 支持".to_string())
}

/// 路径类动作统一入口：`open` / `open_folder` 交宿主 Shell 打开，其余三项由插件
/// 进程内的 Windows 调用完成（记事本、资源管理器选中、剪贴板）。
async fn run_path_action(action_id: &str, path: &str) -> Result<(), PluginError> {
    match action_id {
        "open" => host()
            .shell_open(path)
            .await
            .map_err(|e| PluginError::ActionFailed(format!("打开失败: {e}, 路径: {path}")))?,
        "open_folder" => {
            let parent = Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string());
            host().shell_open_folder(&parent).await.map_err(|e| {
                PluginError::ActionFailed(format!("打开文件夹失败: {e}, 路径: {parent}"))
            })?;
        }
        "open_with_notepad" => open_with_notepad(path).map_err(|e| {
            PluginError::ActionFailed(format!("用记事本打开失败: {e}, 路径: {path}"))
        })?,
        "open_file_location" => reveal_in_explorer(path).map_err(|e| {
            PluginError::ActionFailed(format!("打开文件位置失败: {e}, 路径: {path}"))
        })?,
        "copy_path" => {
            set_clipboard_text(path)
                .map_err(|e| PluginError::ActionFailed(format!("复制路径失败: {e}")))?;
        }
        other => return Err(PluginError::ActionFailed(format!("未知路径动作: {other}"))),
    }
    Ok(())
}

#[async_trait]
impl Configurable for EverythingPlugin {
    fn core(&self) -> &ComponentCore {
        &self.core
    }

    /// 声明六项配置：排序阈值、排序方式、结果上限、路径匹配（默认关）、
    /// 本体自启动（默认开）、exe 自定义路径（仅自启动开启时可见）。
    fn setting_schema(&self) -> Vec<SettingDefinition> {
        let sort_values: Vec<String> = SORT_METHODS.iter().map(|s| s.to_string()).collect();
        let sort_labels: Vec<String> = SORT_METHODS
            .iter()
            .map(|s| t_key(&format!("sort.{s}")))
            .collect();
        vec![
            SettingDefinition {
                key: "sort_threshold".to_string(),
                schema: SchemaNode {
                    kind: SchemaKind::Integer {
                        minimum: Some(0),
                        maximum: Some(100),
                        multiple_of: None,
                    },
                    default: Some(serde_json::json!(4)),
                },
                ui: FieldUiMetadata {
                    pointer: "/sort_threshold".to_string(),
                    label: t_key("sortThreshold"),
                    description: t_key("sortThresholdDesc"),
                    group: None,
                    order: 0,
                    visible: true,
                    read_only: false,
                    visible_when: None,
                    widget: None,
                    action: None,
                    detail_action: None,
                },
            },
            SettingDefinition {
                key: "sort_method".to_string(),
                schema: SchemaNode {
                    kind: SchemaKind::String {
                        enum_values: sort_values,
                        enum_labels: sort_labels,
                        min_length: None,
                        max_length: None,
                        pattern: None,
                    },
                    default: Some(serde_json::json!("NameAscending")),
                },
                ui: FieldUiMetadata {
                    pointer: "/sort_method".to_string(),
                    label: t_key("sortMethod"),
                    description: t_key("sortMethodDesc"),
                    group: None,
                    order: 1,
                    visible: true,
                    read_only: false,
                    visible_when: None,
                    widget: Some(WidgetHint::Select),
                    action: None,
                    detail_action: None,
                },
            },
            SettingDefinition {
                key: "result_limit".to_string(),
                schema: SchemaNode {
                    kind: SchemaKind::Integer {
                        minimum: Some(1),
                        maximum: Some(100),
                        multiple_of: None,
                    },
                    default: Some(serde_json::json!(10)),
                },
                ui: FieldUiMetadata {
                    pointer: "/result_limit".to_string(),
                    label: t_key("resultLimit"),
                    description: t_key("resultLimitDesc"),
                    group: None,
                    order: 2,
                    visible: true,
                    read_only: false,
                    visible_when: None,
                    widget: None,
                    action: None,
                    detail_action: None,
                },
            },
            SettingDefinition {
                key: "enable_path_match".to_string(),
                schema: SchemaNode {
                    kind: SchemaKind::Boolean,
                    default: Some(serde_json::json!(false)),
                },
                ui: FieldUiMetadata {
                    pointer: "/enable_path_match".to_string(),
                    label: t_key("enablePathMatch"),
                    description: t_key("enablePathMatchDesc"),
                    group: None,
                    order: 3,
                    visible: true,
                    read_only: false,
                    visible_when: None,
                    widget: Some(WidgetHint::Toggle),
                    action: None,
                    detail_action: None,
                },
            },
            SettingDefinition {
                key: "auto_launch_everything".to_string(),
                schema: SchemaNode {
                    kind: SchemaKind::Boolean,
                    default: Some(serde_json::json!(true)),
                },
                ui: FieldUiMetadata {
                    pointer: "/auto_launch_everything".to_string(),
                    label: t_key("autoLaunch"),
                    description: t_key("autoLaunchDesc"),
                    group: None,
                    order: 4,
                    visible: true,
                    read_only: false,
                    visible_when: None,
                    widget: Some(WidgetHint::Toggle),
                    action: None,
                    detail_action: None,
                },
            },
            SettingDefinition {
                key: "everything_exe_path".to_string(),
                schema: SchemaNode {
                    kind: SchemaKind::String {
                        enum_values: Vec::new(),
                        enum_labels: Vec::new(),
                        min_length: None,
                        max_length: None,
                        pattern: None,
                    },
                    default: Some(serde_json::json!("")),
                },
                ui: FieldUiMetadata {
                    pointer: "/everything_exe_path".to_string(),
                    label: t_key("exePath"),
                    description: t_key("exePathDesc"),
                    group: None,
                    order: 5,
                    visible: true,
                    read_only: false,
                    visible_when: Some(VisibleWhen {
                        field: "auto_launch_everything".to_string(),
                        value: serde_json::json!(true),
                    }),
                    widget: Some(WidgetHint::Path { mode: PathMode::File }),
                    action: None,
                    detail_action: None,
                },
            },
        ]
    }

    fn get_settings(&self) -> serde_json::Value {
        let s = self.state.read();
        serde_json::json!({
            "sort_threshold": s.sort_threshold,
            "sort_method": s.sort_method,
            "result_limit": s.result_limit,
            "enable_path_match": s.enable_path_match,
            "auto_launch_everything": s.auto_launch_everything,
            "everything_exe_path": s.everything_exe_path,
        })
    }

    /// 应用宿主下发的配置（宿主已按 schema 校验，此处只更新存在的键）。
    async fn apply_settings(&self, settings: serde_json::Value) -> Result<(), ConfigError> {
        let mut s = self.state.write();
        if let Some(v) = settings.get("sort_threshold").and_then(|v| v.as_u64()) {
            s.sort_threshold = v as usize;
        }
        if let Some(v) = settings.get("sort_method").and_then(|v| v.as_str()) {
            s.sort_method = v.to_string();
        }
        if let Some(v) = settings.get("result_limit").and_then(|v| v.as_u64()) {
            s.result_limit = v as usize;
        }
        if let Some(v) = settings.get("enable_path_match").and_then(|v| v.as_bool()) {
            s.enable_path_match = v;
        }
        if let Some(v) = settings.get("auto_launch_everything").and_then(|v| v.as_bool()) {
            s.auto_launch_everything = v;
        }
        if let Some(v) = settings.get("everything_exe_path").and_then(|v| v.as_str()) {
            s.everything_exe_path = v.trim().to_string();
        }
        Ok(())
    }
}

#[async_trait]
impl Plugin for EverythingPlugin {
    async fn init(
        &self,
        _ctx: &PluginContext,
        _handle: Option<Arc<PluginHandle>>,
    ) -> Result<(), PluginError> {
        tracing::info!("Everything 插件初始化完成");
        Ok(())
    }

    /// 查询统一返回沉浸式面板响应：空查询（热键唤醒）返回面板骨架；
    /// 非空查询执行 Everything 搜索，结果经 CustomPanel.data 自描述 JSON
    /// 承载（含大小/修改时间/目录/扩展名），面板按需自渲染。
    async fn query(
        &self,
        _ctx: &PluginContext,
        query: &Query,
    ) -> Result<QueryResponse, PluginError> {
        let search_term = query.search_term.trim();

        // 读取配置快照：克隆后立即释放读锁（guard 不 Send，不可跨 await）
        let state = self.state.read().clone();
        let should_sort = search_term.chars().count() >= state.sort_threshold;

        let outcome = if search_term.is_empty() {
            // 面板唤醒（Ctrl+E）：返回沉浸式面板骨架
            SearchOutcome { items: Vec::new(), bundled_index_empty: false }
        } else {
            let guard = Arc::clone(&self.search_guard);
            let slot = Arc::clone(&self.everything_slot);
            let search_term = search_term.to_string();
            let search_state = state.clone();
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            let job = Arc::clone(&self.job_guard);
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            let bundled_backend = Arc::clone(&self.bundled_backend);

            // Everything 查询为阻塞调用（等待服务响应），放阻塞线程池执行，避免卡住 RPC 循环
            tokio::task::spawn_blocking(move || {
                #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
                return search_everything(&guard, &slot, &search_term, &search_state, &job, &bundled_backend);
                #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
                return search_everything(&guard, &slot, &search_term, &search_state, &bundled_backend);
            })
            .await
            .map_err(|e| PluginError::QueryFailed(format!("Everything 搜索任务失败: {e}")))?
            .map_err(|e| PluginError::QueryFailed(e))?
        };
        let should_sort = !search_term.is_empty() && should_sort;

        // 沉浸式面板统一响应：结果以自定义 JSON 经 CustomPanel.data 承载
        // （含大小/修改时间/目录/扩展名，面板按需自渲染）。GUI 触发词与
        // CLI 查询同样获得该形状（宿主对 CustomPanel 透传，CLI 有面板输出）。
        // 空库标记由 search_everything 的探针给出：仅当后端确为随包实例、且索引里
        // 确实一个条目都没有时才为真（用户自装版的零结果不提示）。
        Ok(QueryResponse::CustomPanel {
            panel_type: "everything".to_string(),
            data: serde_json::json!({
                "query": search_term,
                "items": outcome.items,
                "sortSkipped": !should_sort,
                "enablePathMatch": state.enable_path_match,
                "resultLimit": state.result_limit,
                "emptyBundledIndex": outcome.bundled_index_empty,
            }),
            actions: vec![
                ResultAction {
                    id: "open".to_string(),
                    label: t_key("open"),
                    icon: IconRequest::Path(String::new()),
                    is_default: true,
                    shortcut_key: "Enter".to_string(),
                },
                ResultAction {
                    id: "toggle_path_match".to_string(),
                    label: if state.enable_path_match {
                        t_key("togglePathMatchOff")
                    } else {
                        t_key("togglePathMatchOn")
                    },
                    icon: IconRequest::Path(String::new()),
                    is_default: false,
                    // 仅作展示提示：前端当前不自动分发 action 快捷键，
                    // 实际通过 Tab / 点击 / Ctrl+数字 触发
                    shortcut_key: "Ctrl+U".to_string(),
                },
                ResultAction {
                    id: "open_folder".to_string(),
                    label: t_key("openFolder"),
                    icon: IconRequest::Path(String::new()),
                    is_default: false,
                    shortcut_key: String::new(),
                },
            ],
            keep_search_bar: false,
        })
    }

    /// 沉浸式面板按键契约（宿主键盘状态机解释执行；Shadow DOM 内事件冒泡到宿主窗口）：
    /// - Esc → 返回默认面板（与内置面板语义一致）；
    /// - Ctrl+U → 切换路径匹配（custom 动作，宿主经 pluginAction 通道转发）。
    /// 面板按钮类的动作不走按键通道：面板的 `host.executeAction` 与按键转发是同一条
    /// pluginAction RPC（同一会话代际与归属校验），无需为按钮额外声明按键。
    fn interaction_policy(&self) -> PanelInteraction {
        PanelInteraction {
            bindings: vec![
                PanelKeyBinding {
                    key: "Escape".to_string(),
                    action: PanelKeyAction::GoBack,
                },
                PanelKeyBinding {
                    key: "Ctrl+U".to_string(),
                    action: PanelKeyAction::Custom {
                        action: "toggle_path_match".to_string(),
                        args: serde_json::Value::Null,
                    },
                },
            ],
            ..Default::default()
        }
    }

    /// 执行动作：路径类动作（open / open_folder / open_with_notepad /
    /// open_file_location / copy_path）从载荷取 `{ "path": "..." }` 后统一分发；
    /// toggle_path_match 翻转路径匹配开关（后续查询生效）。
    async fn execute_action(
        &self,
        _ctx: &PluginContext,
        action_id: &str,
        payload: serde_json::Value,
    ) -> Result<(), PluginError> {
        // 沉浸式面板动作载荷为自由 JSON：{ "path": "..." }（面板直传完整路径）
        if matches!(
            action_id,
            "open" | "open_folder" | "open_with_notepad" | "open_file_location" | "copy_path"
        ) {
            let path = payload
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PluginError::ActionFailed("载荷缺少路径（path）".to_string()))?;
            return run_path_action(action_id, path).await;
        }

        match action_id {
            "toggle_path_match" => {
                let mut state = self.state.write();
                state.enable_path_match = !state.enable_path_match;
                tracing::info!(enable_path_match = state.enable_path_match, "切换路径匹配");
                Ok(())
            }
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            "rebuild_index_elevated" => {
                // 面板二次确认后走到这里：提权启动插件安装根里的随包本体建库（弹 UAC）。
                // 退出旧实例（`-exit` + 等 exe 释放）是阻塞调用，放阻塞线程池执行，不占 tokio worker。
                let exe = self.bodyguard.exe().map(Path::to_path_buf);
                tokio::task::spawn_blocking(move || {
                    let Some(exe) = exe else {
                        return Err("无法确定插件安装目录，拒绝提权启动".to_string());
                    };
                    if !exe.is_file() {
                        return Err(format!(
                            "随包本体不存在（安装包缺少 {BUNDLED_EVERYTHING_EXE}）：请在插件设置中指定 Everything.exe 路径"
                        ));
                    }
                    launch_everything_elevated(&exe)
                })
                .await
                .map_err(|e| PluginError::ActionFailed(format!("提权建库任务失败: {e}")))?
                .map_err(PluginError::ActionFailed)?;
                info!("已提权启动随包本体建库，面板提示用户等待索引建成");
                Ok(())
            }
            other => Err(PluginError::ActionFailed(format!("未知动作: {other}"))),
        }
    }
}

/// 装配插件应用（骨架 `main()` 调用 `.run()`）。
pub fn app() -> PluginApp {
    PluginApp::new(EverythingPlugin::new())
}