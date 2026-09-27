//! 启动骨架：进程入口 + SDK 启动。
//!
//! 随模板同步（`.github/workflows/sync-template.yml`）——这里只放启动模板代码，尽量别改；
//! 插件实现全部写在 `src/plugin.rs`（不参与同步）。确有必要改动本文件时照常改，
//! 同步 PR 里人工合并即可（同步带的 `cargo check` 会挡住编译不过的情况）。

mod plugin;

fn main() {
    // 预置插件 id（宿主 spawn 时注入 ZEROLAUNCH_PLUGIN_ID），使组件构造阶段的
    // t_key() 可用——组件元数据构造早于 run() 握手。
    zerolaunch_plugin_sdk_rust::init();
    // 装配（可挂载额外组件）与运行交给插件侧
    plugin::app().run();
}
