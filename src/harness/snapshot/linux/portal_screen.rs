//! Wayland xdg-desktop-portal 截屏（feature `linux-portal`；upgrade.md
//! §7.1，P3）。
//!
//! 纯 Wayland 会话没有全局截屏协议（无 X11 GetImage 等价物），走
//! xdg-desktop-portal 的 **Screenshot** 接口（`ashpd` 0.12，zbus 纯 Rust
//! D-Bus）：portal 后端（gnome/kde/wlr 等）弹授权对话框 → 用户同意后把
//! 整屏 PNG 写入临时文件、以 `file://` URI 回给调用方 → 本模块读字节解析
//! IHDR 尺寸 → [`ScreenCapture`]。
//!
//! - **授权语义**：不可用/被拒/取消一律 [`Error::WaylandPortalUnavailable`]
//!   （code `screen-unavailable-wayland-portal`，指引宿主提示授权或换
//!   X11 路径）；
//! - **async 红线**：ashpd 是 async（zbus），在函数内建**一次性
//!   current-thread tokio runtime** `block_on`——runtime 是本 feature 的
//!   内部实现细节，默认构建零 async runtime 不受影响（Cargo.toml 注释）；
//! - **坐标语义**：portal Screenshot 只出**整屏单帧**（无区域参数），区域
//!   调用方需自行裁剪位图（todo 清单）；`logical:true` 标注（§7.1）——
//!   ashpd 0.12 的 Screenshot 响应只有 `uri` 字段，**无 API 可传该标记**
//!   （已核对 crate 源码），Wayland 无全局物理坐标协议的事实由模块文档
//!   记录在案，宿主侧解释截图尺寸时须知；
//! - **真实测试**：需要真实 Wayland 会话 + 人工点授权，全部 `#[ignore]`
//!   （todo 清单）；单测只覆盖 PNG 头解析/URI 转路径等纯逻辑。

use ashpd::desktop::screenshot::Screenshot;

use crate::harness::snapshot::screen::ScreenCapture;
use crate::harness::Error;

/// portal 截屏入口（由 [`super::capture_screen_linux`] 在纯 Wayland 时选路
/// 到此）。区域参数不适用（portal Screenshot 只出整屏，见模块文档）。
pub(crate) fn capture_screen_via_portal() -> Result<ScreenCapture, Error> {
    // 一次性 current-thread runtime（block_on 语义即可；portal D-Bus 的
    // unix socket IO/timer 由 enable_all 打开）。
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        // 实践不可达（runtime 构造不依赖环境）；按 portal 不可用归类。
        .map_err(|error| {
            tracing::warn!(%error, "portal runtime 构造失败（实践不可达路径）");
            Error::WaylandPortalUnavailable
        })?;
    runtime.block_on(async {
        // interactive=false：不弹自定义选项对话框（只要授权确认）；
        // modal=true：授权对话框模态（不与应用其他交互混流）。
        let request = Screenshot::request()
            .interactive(false)
            .modal(true)
            .send()
            .await
            .map_err(|error| portal_unavailable("Screenshot 请求失败", error))?;
        let screenshot = request
            .response()
            .map_err(|error| portal_unavailable("Screenshot 响应失败", error))?;
        let uri = screenshot.uri();
        // portal 契约：file:// URI 指向临时 PNG；非 file scheme（如门户实现
        // 差异）按不可用处理。
        let path = uri.to_file_path().map_err(|_| {
            tracing::warn!(uri = %uri, "portal 返回非 file URI");
            Error::WaylandPortalUnavailable
        })?;
        let png = std::fs::read(&path).map_err(|error| {
            tracing::warn!(path = %path.display(), %error, "portal 截图文件读取失败");
            Error::WaylandPortalUnavailable
        })?;
        let (width, height) = png_ihdr_dims(&png).ok_or_else(|| {
            tracing::warn!(path = %path.display(), "portal 产物不是合法 PNG（IHDR 缺失）");
            Error::WaylandPortalUnavailable
        })?;
        Ok(ScreenCapture { png, width, height })
    })
}

/// portal 链路错误统一收敛（细节进 warn 日志，wire 面只有既定错误码——
/// ashpd 错误类型不进公共错误枚举，避免错误面耦合 portal 版本）。
fn portal_unavailable(stage: &str, error: ashpd::Error) -> Error {
    tracing::warn!(stage, %error, "xdg-desktop-portal 截屏失败");
    Error::WaylandPortalUnavailable
}

/// PNG IHDR 尺寸解析（纯函数，单测锁定）：魔数 + 首块 IHDR 的大端宽高。
/// portal 产物与 X11 路径的 png crate 编码同规格（8 字节魔数 + IHDR 起始
/// 偏移 8+4+4）。
pub(crate) fn png_ihdr_dims(png: &[u8]) -> Option<(i32, i32)> {
    if png.len() < 24 || !png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return None;
    }
    if &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?) as i32;
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?) as i32;
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 PNG 头（魔数 + IHDR 长度/类型 + 宽高字节；不追 CRC——
    /// 解析器只看头部）。
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn ihdr_dims_parses_width_and_height() {
        assert_eq!(png_ihdr_dims(&png_header(1920, 1080)), Some((1920, 1080)));
        assert_eq!(png_ihdr_dims(&png_header(1, 65535)), Some((1, 65535)));
    }

    #[test]
    fn ihdr_dims_rejects_malformed_input() {
        assert_eq!(png_ihdr_dims(&[]), None, "空");
        assert_eq!(png_ihdr_dims(b"not a png".as_slice()), None, "无魔数");
        assert_eq!(png_ihdr_dims(&png_header(0, 100)[..20]), None, "截断");
        // 魔数后非 IHDR 首块。
        let mut wrong = png_header(10, 10);
        wrong[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_ihdr_dims(&wrong), None);
        // 非正尺寸。
        assert_eq!(png_ihdr_dims(&png_header(0, 0)), None);
    }

    /* --- 真实 portal 链路（#[ignore]：需真实 Wayland 会话 + 人工授权） ------ */
    //
    // 运行方式（Wayland 桌面会话内执行；授权对话框需人工点击允许）：
    //   WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 \
    //   CARGO_TARGET_DIR=… cargo test --features linux-portal -- \
    //       portal_real --ignored --nocapture

    #[test]
    #[ignore = "需真实 Wayland 会话 + xdg-desktop-portal 后端 + 人工点击授权对话框"]
    fn portal_real_screenshot_on_wayland_session() {
        let wayland = std::env::var("WAYLAND_DISPLAY")
            .is_ok_and(|value| !value.trim().is_empty());
        if !wayland {
            eprintln!("跳过：非 Wayland 会话");
            return;
        }
        let shot = capture_screen_via_portal().expect("授权后应出整屏 PNG");
        assert_eq!(
            png_ihdr_dims(&shot.png),
            Some((shot.width, shot.height)),
            "IHDR 与出参尺寸一致"
        );
    }
}
