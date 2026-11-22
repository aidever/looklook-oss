//! 电脑浏览器里的终端渲染（设计稿 §7）：wgpu 画 termd 的屏幕和回滚区。浏览器有 WebGPU 用 WebGPU，没有退到 WebGL2。
//!
//! 和 term-web 编进同一个 wasm：JS 端用这里导出的 `TermClient`（拆包、解码、回滚行留在 WASM 里，`keep_history`），
//! `GpuTerm` 直接读它的屏幕和回滚行来画，解码完的状态不用再搬到 JS。
//!
//! - scene.rs：一帧画什么（实例列表）；select.rs：选字、搜索、链接；state.rs：滚动等状态；theme.rs：配色；
//!   boxdraw.rs：框线方块自己画——这几个不碰 GPU，本机能测；
//! - atlas.rs：字交给浏览器 Canvas2D 光栅化进图集；gpu.rs：wgpu 管线。

pub mod boxdraw;
pub mod scene;
pub mod select;
pub mod state;
pub mod theme;

#[cfg(target_arch = "wasm32")]
mod atlas;
#[cfg(target_arch = "wasm32")]
mod gpu;
#[cfg(target_arch = "wasm32")]
mod web;

pub use term_web::TermClient;

#[cfg(test)]
mod tests;
