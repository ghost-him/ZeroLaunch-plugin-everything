// Everything 沉浸式面板 — 宿主内嵌执行（Shadow DOM 容器，全信任模型）。
//
// 数据流：
// - 面板内输入 → host.query(内容) → 宿主 bridge_query 直调插件
//   → 插件返回 CustomPanel（panelData = 自描述结果 JSON，含大小/修改时间/
//     目录/扩展名），面板按需自渲染，不依赖宿主 List 管线；
// - 唤醒/触发词进入时宿主重放 panelData（onDataUpdate），有 query 与
//   items 时直接展示，无需重复查询；
// - Enter 打开选中项 → host.executeAction('open', { path }) → pluginAction 通道；
// - Esc / Ctrl+U 不在此拦截：声明在插件的 interaction_policy bindings 中，
//   由宿主键盘状态机统一解释（Shadow DOM 内事件冒泡到宿主窗口）；
// - 文本经 host.t(key) 查插件语言包（与 Rust t_key 同键，key-or-literal）。

export default function mount(rootEl, host) {
  const state = {
    items: [],
    selectedIndex: 0,
    enablePathMatch: false,
    sortSkipped: false,
    querySeq: 0,
    inFlight: false,
  }

  rootEl.innerHTML = `
    <style>
      /* 面板样式使用宿主 CSS 变量（variables.css / applyAppearanceSettings 权威），
         Shadow DOM 内继承宿主自定义属性——宿主切换主题时自动跟随，无需 IPC。 */
      .ev-panel {
        display: flex; flex-direction: column; height: 100%;
        background: var(--bg-primary); color: var(--text-primary);
        font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
        user-select: none;
      }
      .ev-search {
        display: flex; align-items: center; gap: 12px;
        padding: 14px 18px; border-bottom: 1px solid var(--border-color);
        flex-shrink: 0;
      }
      .ev-search input {
        flex: 1; background: transparent; border: none; outline: none;
        color: var(--text-primary); font-size: 18px; padding: 4px 0;
      }
      .ev-search input::placeholder { color: var(--text-secondary); }
      .ev-count { color: var(--text-secondary); font-size: 12px; white-space: nowrap; }
      .ev-badge {
        color: var(--text-secondary); font-size: 11px; white-space: nowrap;
        border: 1px solid var(--border-color); border-radius: 10px; padding: 1px 8px;
      }
      .ev-badge.highlight { color: var(--accent-color); border-color: var(--accent-color); }
      .ev-list { flex: 1; min-height: 0; overflow-y: auto; padding: 6px 0; }
      .ev-item {
        display: grid; grid-template-columns: 36px 1fr auto; align-items: center;
        gap: 10px; padding: 5px 18px 5px 14px; cursor: default;
        border-left: 3px solid transparent;
      }
      .ev-item:hover { background: var(--hover-color); }
      .ev-item.selected {
        background: var(--primary-color-alpha); border-left-color: var(--accent-color);
      }
      .ev-icon {
        width: 36px; height: 36px; display: flex; align-items: center; justify-content: center;
        font-size: 22px;
      }
      .ev-icon img { max-width: 30px; max-height: 30px; }
      .ev-texts { min-width: 0; display: flex; flex-direction: column; gap: 2px; }
      .ev-name {
        font-size: 14px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ev-dir {
        font-size: 11px; color: var(--text-secondary);
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      }
      .ev-meta {
        display: flex; align-items: center; gap: 12px; min-width: 0;
        padding-left: 12px;
      }
      .ev-size {
        font-size: 12px; color: var(--text-secondary); white-space: nowrap;
        font-variant-numeric: tabular-nums; min-width: 64px; text-align: right;
      }
      .ev-date {
        font-size: 12px; color: var(--text-secondary); white-space: nowrap;
        font-variant-numeric: tabular-nums;
      }
      .ev-ext {
        font-size: 10px; color: var(--text-secondary); white-space: nowrap;
        border: 1px solid var(--border-color); border-radius: 3px;
        padding: 1px 5px;
        background: var(--bg-secondary);
      }
      .ev-empty, .ev-error {
        padding: 40px 18px; text-align: center; color: var(--text-secondary); font-size: 13px;
      }
      .ev-error { color: var(--text-error); }
      .ev-status {
        display: flex; justify-content: space-between; align-items: center;
        padding: 8px 18px; border-top: 1px solid var(--border-color);
        font-size: 12px; color: var(--text-secondary); flex-shrink: 0;
      }
      .ev-status .ev-pathmatch.on { color: var(--accent-color); }
      .ev-hints { white-space: nowrap; }
      .ev-hints kbd {
        background: var(--bg-secondary); border: 1px solid var(--border-color);
        border-radius: 3px; padding: 0 5px; font-size: 11px; color: var(--text-secondary);
      }
    </style>
    <div class="ev-panel">
      <div class="ev-search">
        <input id="ev-input" placeholder="${host.t('panelPlaceholder')}" spellcheck="false" />
        <span class="ev-count" id="ev-count"></span>
        <span class="ev-badge highlight" id="ev-sortbadge" hidden></span>
      </div>
      <div class="ev-list" id="ev-list"></div>
      <div class="ev-status">
        <span class="ev-pathmatch" id="ev-pathmatch"></span>
        <span class="ev-hints"><kbd>↑↓</kbd> ${host.t('panelNavHint')} · <kbd>Enter</kbd> ${host.t('panelOpenHint')} · <kbd>Ctrl+U</kbd> ${host.t('panelPathMatchHint')} · <kbd>Esc</kbd> ${host.t('panelExitHint')}</span>
      </div>
    </div>
  `

  const input = rootEl.querySelector('#ev-input')
  const listEl = rootEl.querySelector('#ev-list')
  const countEl = rootEl.querySelector('#ev-count')
  const sortBadgeEl = rootEl.querySelector('#ev-sortbadge')
  const pathMatchEl = rootEl.querySelector('#ev-pathmatch')

  // 文件类型 → emoji 图标（Everything 面板自渲染，不依赖宿主图标链路）
  const TYPE_ICONS = {
    exe: '\u2699\uFE0F', msi: '\u2699\uFE0F', bat: '\u2699\uFE0F', cmd: '\u2699\uFE0F', ps1: '\u2699\uFE0F',
    jpg: '\uD83D\uDDBC\uFE0F', jpeg: '\uD83D\uDDBC\uFE0F', png: '\uD83D\uDDBC\uFE0F', gif: '\uD83D\uDDBC\uFE0F',
    webp: '\uD83D\uDDBC\uFE0F', bmp: '\uD83D\uDDBC\uFE0F', svg: '\uD83D\uDDBC\uFE0F', ico: '\uD83D\uDDBC\uFE0F',
    mp4: '\uD83C\uDFAC', mkv: '\uD83C\uDFAC', avi: '\uD83C\uDFAC', mov: '\uD83C\uDFAC', webm: '\uD83C\uDFAC',
    mp3: '\uD83C\uDFB5', wav: '\uD83C\uDFB5', flac: '\uD83C\uDFB5', aac: '\uD83C\uDFB5', ogg: '\uD83C\uDFB5',
    zip: '\uD83D\uDDD2\uFE0F', rar: '\uD83D\uDDD2\uFE0F', '7z': '\uD83D\uDDD2\uFE0F', gz: '\uD83D\uDDD2\uFE0F',
    pdf: '\uD83D\uDCC4', doc: '\uD83D\uDCC4', docx: '\uD83D\uDCC4', txt: '\uD83D\uDCC4', md: '\uD83D\uDCC4',
    xls: '\uD83D\uDCCA', xlsx: '\uD83D\uDCCA', csv: '\uD83D\uDCCA',
    ppt: '\uD83D\uDCFD\uFE0F', pptx: '\uD83D\uDCFD\uFE0F',
    html: '\uD83C\uDF10', htm: '\uD83C\uDF10',
    rs: '\uD83E\uDD80', py: '\uD83D\uDC0D', json: '\uD83E\uDDFE', xml: '\uD83E\uDDFE',
  }
  const ICON_FOLDER = '\uD83D\uDCC1'
  const ICON_FILE = '\uD83D\uDCC4'

  function iconFor(item) {
    if (item.iconUrl) return null // 宿主解析图标（data URL，List 回退路径）
    if (item.isFolder) return ICON_FOLDER
    return TYPE_ICONS[item.extension] ?? ICON_FILE
  }

  function formatSize(bytes) {
    if (bytes == null) return '\u2014'
    if (bytes < 1024) return `${bytes} B`
    const units = ['KB', 'MB', 'GB', 'TB']
    let v = bytes
    for (const u of units) {
      v /= 1024
      if (v < 1024) return `${v.toFixed(1)} ${u}`
    }
    return `${v.toFixed(1)} PB`
  }

  function formatDate(unixSec) {
    if (!unixSec) return '\u2014'
    const d = new Date(unixSec * 1000)
    const pad = (n) => String(n).padStart(2, '0')
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
  }

  function normalizeItem(item) {
    // List 回退路径的 item（title/subtitle/icon）与自定义 JSON 统一为内部形状
    if (typeof item.name !== 'string') {
      return {
        path: item.subtitle ?? '',
        name: item.title ?? item.subtitle ?? '',
        dir: '',
        isFolder: false,
        size: null,
        modified: null,
        extension: '',
        iconUrl: item.icon ?? null,
      }
    }
    return {
      path: item.path ?? '',
      name: item.name ?? '',
      dir: item.dir ?? '',
      isFolder: !!item.isFolder,
      size: item.size ?? null,
      modified: item.modified ?? null,
      extension: item.extension ?? '',
      iconUrl: null,
    }
  }

  function applyPanelData(data) {
    state.items = Array.isArray(data?.items) ? data.items.map(normalizeItem) : []
    state.sortSkipped = !!data?.sortSkipped
    if (typeof data?.enablePathMatch === 'boolean') {
      state.enablePathMatch = data.enablePathMatch
      renderPathMatch()
    }
    state.selectedIndex = 0
    render()
  }

  function renderPathMatch() {
    pathMatchEl.textContent = state.enablePathMatch
      ? host.t('panelPathMatchOn')
      : host.t('panelPathMatchOff')
    pathMatchEl.classList.toggle('on', state.enablePathMatch)
  }

  function render() {
    countEl.textContent = state.inFlight
      ? host.t('panelSearching')
      : host.t('panelResultCount', { count: state.items.length })
    sortBadgeEl.hidden = !state.sortSkipped
    if (state.sortSkipped) sortBadgeEl.textContent = host.t('panelSortSkipped')

    if (state.inFlight) {
      listEl.innerHTML = `<div class="ev-empty">${host.t('panelSearching')}</div>`
      return
    }
    if (state.items.length === 0) {
      listEl.innerHTML = `<div class="ev-empty">${host.t('panelEmpty')}</div>`
      return
    }

    listEl.innerHTML = ''
    state.items.forEach((item, i) => {
      const el = document.createElement('div')
      el.className = 'ev-item' + (i === state.selectedIndex ? ' selected' : '')
      el.title = item.path

      const iconGlyph = iconFor(item)
      const metaParts = []
      if (!item.isFolder) {
        metaParts.push(`<span class="ev-size">${formatSize(item.size)}</span>`)
      }
      metaParts.push(`<span class="ev-date">${formatDate(item.modified)}</span>`)
      metaParts.push(`<span class="ev-ext">${item.isFolder ? host.t('panelFolder') : (item.extension || 'FILE').toUpperCase()}</span>`)

      el.innerHTML = `
        <div class="ev-icon">${item.iconUrl ? `<img src="${item.iconUrl}" alt="" />` : iconGlyph}</div>
        <div class="ev-texts">
          <div class="ev-name"></div>
          <div class="ev-dir"></div>
        </div>
        <div class="ev-meta">${metaParts.join('')}</div>`
      el.querySelector('.ev-name').textContent = item.name
      el.querySelector('.ev-dir').textContent = item.dir || '\u00A0'
      el.addEventListener('click', () => {
        state.selectedIndex = i
        render()
      })
      el.addEventListener('dblclick', () => openSelected())
      listEl.appendChild(el)
    })
    const selected = listEl.querySelector('.selected')
    if (selected) selected.scrollIntoView({ block: 'nearest' })
  }

  function setError(message) {
    countEl.textContent = ''
    listEl.innerHTML = `<div class="ev-error">${message}</div>`
  }

  // 响应适配：CustomPanel（面板原生）与 List 回退（宿主旧管线）统一为 items
  function normalizeResponse(resp) {
    if (!resp) return { items: [], sortSkipped: false }
    if (resp.mode === 'plugin_immersive' || resp.mode === 'plugin_panel') {
      return {
        items: Array.isArray(resp.panelData?.items) ? resp.panelData.items : [],
        sortSkipped: !!resp.panelData?.sortSkipped,
        enablePathMatch: resp.panelData?.enablePathMatch,
      }
    }
    if (resp.mode === 'search') {
      return {
        items: (resp.results ?? []).map((r) => normalizeItem(r)),
        sortSkipped: false,
        enablePathMatch: undefined,
      }
    }
    return { items: [], sortSkipped: false, enablePathMatch: undefined }
  }

  async function search() {
    const text = input.value.trim()
    const seq = ++state.querySeq
    if (text.length === 0) {
      state.items = []
      state.selectedIndex = 0
      state.sortSkipped = false
      state.inFlight = false
      render()
      return
    }
    state.inFlight = true
    state.items = []
    state.selectedIndex = 0
    render()
    try {
      const resp = await host.query(text)
      if (seq !== state.querySeq) return // 过期响应丢弃
      const normalized = normalizeResponse(resp)
      state.items = normalized.items
      state.sortSkipped = normalized.sortSkipped
      if (normalized.enablePathMatch !== undefined) {
        state.enablePathMatch = normalized.enablePathMatch
        renderPathMatch()
      }
      state.inFlight = false
      render()
    } catch (e) {
      if (seq !== state.querySeq) return
      state.inFlight = false
      setError(host.t('panelError') + (e?.message ?? ''))
    }
  }

  function openSelected() {
    const item = state.items[state.selectedIndex]
    if (!item) return
    host.executeAction('open', { path: item.path }).catch((e) => {
      console.error('[everything] 打开失败:', e)
    })
  }

  // 宿主重放的面板数据（唤醒时下发；触发词带查询进入时直接展示结果，
  // 并将查询词预填到输入框，用户可继续编辑）。
  host.onDataUpdate((data) => {
    if (!data) return
    applyPanelData(data)
    if (typeof data.query === 'string' && data.query && input.value === '') {
      input.value = data.query
    }
  })

  // 输入防抖 200ms：Everything 查询为阻塞调用，避免每键触发
  let debounceTimer = null
  input.addEventListener('input', () => {
    clearTimeout(debounceTimer)
    debounceTimer = setTimeout(search, 200)
  })

  // Esc / Ctrl+U 由插件 bindings 声明、宿主键盘状态机处理（此处不拦截，事件冒泡到宿主）。
  input.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      state.selectedIndex = Math.min(state.selectedIndex + 1, Math.max(state.items.length - 1, 0))
      render()
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      state.selectedIndex = Math.max(state.selectedIndex - 1, 0)
      render()
    } else if (e.key === 'Enter') {
      e.preventDefault()
      openSelected()
    }
  })

  input.focus()
}
