use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tracing::{debug, info};
use zerolaunch_plugin_api::config::{
    ComponentCore, ComponentType, ConfigError, Configurable, FieldUiMetadata, SchemaKind,
    SchemaNode, SettingDefinition, WidgetHint,
};
use zerolaunch_plugin_api::services::icon_request::IconRequest;
use zerolaunch_plugin_api::{
    PanelInteraction, PanelKeyAction, PanelKeyBinding, Plugin, PluginContext, PluginError,
    PluginHandle, PluginKind, PluginMetadata, PluginMode, Query, QueryResponse, ResultAction,
};
use zerolaunch_plugin_sdk_rust::{host, run, t_key};

/// Everything 搜索插件 — 触发词 ev / every 唤起，查询 Everything 服务并返回文件列表。
///
/// 平台注意：Everything SDK 的 set_search/query/迭代作用于进程内全局状态，
/// 并发调用会互相污染，故全部查询经 `search_guard` 串行化并在阻塞线程池执行
/// （Everything 服务未运行时查询可能短暂阻塞）。Everything64.dll 由打包脚本
/// 从 extra/ 目录并入 zip 根，运行时必须与插件 exe 同目录。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use everything_rs::{Everything, EverythingRequestFlags, EverythingSort};

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

/// 可配置状态（查询参数快照，apply_settings 时整体更新）。
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
}

impl Default for EverythingState {
    fn default() -> Self {
        Self {
            // 默认 4：1~3 字符的短查询匹配项极多，跳过排序直接返回原始结果
            sort_threshold: 4,
            sort_method: "NameAscending".to_string(),
            result_limit: 10,
            enable_path_match: false,
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

/// Everything 插件运行时状态与宿主交互能力。
///
/// 仅由本插件进程使用，保存查询配置、结果缓存、串行查询锁和宿主主题状态。

struct EverythingPlugin {
    /// 组件 ID、名称、类型等基础元数据（`Configurable` trait 默认实现委托于此）。
    core: ComponentCore,
    /// 插件静态元数据：id、触发关键词、优先级等。
    metadata: PluginMetadata,
    /// 可配置状态。
    state: RwLock<EverythingState>,
    /// Everything SDK 实例（进程内复用一份）。
    everything_slot: Arc<EverythingSlot>,
    /// Everything SDK 全局状态串行化锁（跨 spawn_blocking 共享）。
    search_guard: Arc<Mutex<()>>,
}

impl EverythingPlugin {
    fn new() -> Self {
        Self {
            core: ComponentCore::new(
                "com.ghost-him.everything".to_string(),
                "Everything 搜索".to_string(),
                "Everything 文件搜索集成".to_string(),
                ComponentType::Plugin,
                100,
            ),
            metadata: PluginMetadata {
                id: "com.ghost-him.everything".to_string(),
                name: "Everything 搜索".to_string(),
                version: "0.1.0".to_string(),
                description: "Everything 文件搜索集成".to_string(),
                author: "ghost-him".to_string(),
                // 触发词双语义：Panel 形态下不参与路由（宿主注册时过滤），
                // 而是作为默认搜索候选项的匹配关键字（用户输入 ev/every 显示该插件）
                trigger_keywords: vec!["ev".to_string(), "every".to_string()],
                supported_os: vec!["windows".to_string()],
                priority: 100,
                // 第三方插件种类（宿主加载时强制覆盖为 ThirdParty，此处显式声明保持语义一致）
                kind: PluginKind::ThirdParty,
                // 沉浸式面板热键唤醒（panel 形态才注册热键表）
                hotkey: Some("Ctrl+E".to_string()),
                icon: None,
                // 完全插件模式：Ctrl+E 唤醒后接管整个搜索窗口
                mode: PluginMode::Panel,
            },
            state: RwLock::new(EverythingState::default()),
            everything_slot: Arc::new(EverythingSlot::new()),
            search_guard: Arc::new(Mutex::new(())),
        }
    }
}

/// 执行一次 Everything 查询，返回结果项列表。
/// 查询长度低于排序阈值时跳过排序（短查询匹配项极多，排序开销大）。
/// 在 spawn_blocking 中调用；Everything 服务不可用时返回错误信息。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn search_everything(
    guard: &Mutex<()>,
    slot: &EverythingSlot,
    search_term: &str,
    state: &EverythingState,
) -> Result<Vec<SearchResult>, String> {
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

    // 懒初始化单实例；复用实例每查询前清空上次结果状态（避免跨查询污染）
    let mut slot_guard = slot.0.lock();
    if slot_guard.is_none() {
        *slot_guard = Some(Everything::new());
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
    everything
        .query()
        .map_err(|e| format!("Everything 查询失败: {e:?}"))?;
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
    Ok(out)
}

/// 非 Windows x86_64 平台：Everything SDK 不可用，返回空结果。
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn search_everything(
    _guard: &Mutex<()>,
    _slot: &EverythingSlot,
    _search_term: &str,
    _state: &EverythingState,
) -> Result<Vec<SearchResult>, String> {
    Ok(Vec::new())
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

#[async_trait]
impl Configurable for EverythingPlugin {
    fn core(&self) -> &ComponentCore {
        &self.core
    }

    /// 声明四项配置：排序阈值、排序方式、结果上限、路径匹配（默认关）。
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
        ]
    }

    fn get_settings(&self) -> serde_json::Value {
        let s = self.state.read();
        serde_json::json!({
            "sort_threshold": s.sort_threshold,
            "sort_method": s.sort_method,
            "result_limit": s.result_limit,
            "enable_path_match": s.enable_path_match,
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
        Ok(())
    }
}

#[async_trait]
impl Plugin for EverythingPlugin {
    fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

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

        let results = if search_term.is_empty() {
            // 面板唤醒（Ctrl+E）：返回沉浸式面板骨架
            Vec::new()
        } else {
            let guard = Arc::clone(&self.search_guard);
            let slot = Arc::clone(&self.everything_slot);
            let search_term = search_term.to_string();
            let search_state = state.clone();

            // Everything 查询为阻塞调用（等待服务响应），放阻塞线程池执行，避免卡住 RPC 循环
            tokio::task::spawn_blocking(move || {
                search_everything(&guard, &slot, &search_term, &search_state)
            })
            .await
            .map_err(|e| PluginError::QueryFailed(format!("Everything 搜索任务失败: {e}")))?
            .map_err(|e| PluginError::QueryFailed(e))?
        };
        let should_sort = !search_term.is_empty() && should_sort;

        // 沉浸式面板统一响应：结果以自定义 JSON 经 CustomPanel.data 承载
        // （含大小/修改时间/目录/扩展名，面板按需自渲染）。GUI 触发词与
        // CLI 查询同样获得该形状（宿主对 CustomPanel 透传，CLI 有面板输出）。
        Ok(QueryResponse::CustomPanel {
            panel_type: "everything".to_string(),
            data: serde_json::json!({
                "query": search_term,
                "items": results,
                "sortSkipped": !should_sort,
                "enablePathMatch": state.enable_path_match,
                "resultLimit": state.result_limit,
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

    /// 执行动作：open / open_folder 按候选 id 还原路径并交由宿主打开；
    /// toggle_path_match 翻转路径匹配开关（后续查询生效）。
    async fn execute_action(
        &self,
        _ctx: &PluginContext,
        action_id: &str,
        payload: serde_json::Value,
    ) -> Result<(), PluginError> {
        match action_id {
            "open" | "open_folder" => {
                // 沉浸式面板动作载荷为自由 JSON：{ "path": "..." }（面板直传完整路径）
                let path = payload
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| PluginError::ActionFailed("载荷缺少路径（path）".to_string()))?;
                if action_id == "open" {
                    host().shell_open(&path).await.map_err(|e| {
                        PluginError::ActionFailed(format!("打开失败: {e}, 路径: {path}"))
                    })?;
                } else {
                    let parent = Path::new(&path)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    host().shell_open_folder(&parent).await.map_err(|e| {
                        PluginError::ActionFailed(format!("打开文件夹失败: {e}, 路径: {parent}"))
                    })?;
                }
                Ok(())
            }
            "toggle_path_match" => {
                let mut state = self.state.write();
                state.enable_path_match = !state.enable_path_match;
                tracing::info!(enable_path_match = state.enable_path_match, "切换路径匹配");
                Ok(())
            }
            other => Err(PluginError::ActionFailed(format!("未知动作: {other}"))),
        }
    }
}

fn main() {
    run(EverythingPlugin::new())
}
