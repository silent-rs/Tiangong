//! 由 WIT 自动生成的宿主侧绑定。
//!
//! `bindgen!` 宏读取 `wit/` 下的接口定义，为 `tiangong-plugin` world 生成
//! 类型化的 Rust 调用入口。生成代码不在此文件手写，由宏在编译期产出。

wasmtime::component::bindgen!({
    path: "wit/tiangong/plugin.wit",
    world: "tiangong-plugin",
});
