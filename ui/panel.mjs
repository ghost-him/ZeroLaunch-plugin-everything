// Hello World plugin panel
export default function mount(rootEl, host) {
  rootEl.innerHTML = `
    <div style="padding: 16px; font-family: system-ui;">
      <h2>Hello World</h2>
      <p>这是一個第三方插件面板示例。</p>
      <div id="data-display"></div>
    </div>
  `

  host.onDataUpdate((data, actions) => {
    const display = rootEl.querySelector('#data-display')
    if (display) {
      display.textContent = JSON.stringify({ data, actions }, null, 2)
    }
  })

  // 销毁契约：面板卸载时宿主调用该回调，清理定时器 / window 级监听器等资源。
  // 也可改为让 mount 直接返回 cleanup 函数（等价，二选一）。
  host.onDestroy(() => {
    rootEl.innerHTML = ''
  })
}
