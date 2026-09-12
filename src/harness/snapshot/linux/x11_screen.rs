//! X11 截屏（feature `linux-x11`；upgrade.md §7.1/§7.4，P1 实现）。
//!
//! 路径：`x11rb` 纯 Rust 直连 `DISPLAY`（无 libX11 硬链）→ 根窗口
//! `GetImage(ZPixmap)` 抓 BGRA → `png` crate 内存编码 →
//! [`ScreenCapture`]。覆盖 X11 原生 / XWayland / WSLg / VcXsrv（同一 X 协议）。
//!
//! - **坐标语义（物理像素）**：X11 全局坐标 = 根窗口坐标，单位是物理像素
//!   （与 Windows DPI-aware 虚拟屏语义对齐，§7.1）——**不是** Web/浏览器
//!   场景的 CSS 逻辑像素；跨层传坐标（如 CDP 的 CSS 坐标）必须先按设备
//!   缩放换算。XWayland HiDPI 下根窗口尺寸与 Wayland 逻辑尺寸可能不一致
//!   （§7.5-5 风险，todo 清单跟踪）；
//! - **整屏哨兵**：非正宽/高（如整屏入口传的 `(0,0,0,0)`）=「矩形未探测」
//!   → 以根窗口几何自查整屏，替换哨兵语义；
//! - **区域语义（对齐 screen.rs 的 Windows 实现）**：请求矩形与根窗口求交
//!   钳制（越界自然裁掉，多显示器根窗口可含负坐标——X11 下根窗口原点恒
//!   `(0,0)`，RandR 扩展屏向正方向延展），空交报
//!   [`Error::ScreenRegionDisjointX11`]；
//! - **只读安全**：GetImage 无输入副作用，对真实桌面可安全实测（本文件
//!   的真实测试即如此；注入类测试才需要 Xvfb/专用会话）。

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use crate::harness::snapshot::screen::ScreenCapture;
use crate::harness::Error;

/// 请求矩形与根窗口求交钳制（纯函数，单测锁定；语义对齐 screen.rs 的
/// Windows 实现 `capture_region_bgra`）。返回钳制后的 `(left, top, w, h)`；
/// 空交（钳制后宽/高 <1）→ `None`。饱和加法防溢出（i32 坐标上界来自
/// 工具层 10000px 钳制 + i32 范围）。
fn clamp_to_root(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    root_w: i32,
    root_h: i32,
) -> Option<(i32, i32, i32, i32)> {
    // X11 全局坐标原点 = 根窗口左上 (0,0)（RandR 多屏向右/下扩展；
    // 与 Windows 虚拟屏可为负原点不同，钳制下界取 0）。
    let left = x.max(0);
    let top = y.max(0);
    let right = x.saturating_add(width).min(root_w);
    let bottom = y.saturating_add(height).min(root_h);
    let (w, h) = (right - left, bottom - top);
    (w >= 1 && h >= 1).then_some((left, top, w, h))
}

/// 连接 X server（`DISPLAY` 缺失/连不上 → [`Error::X11ConnectFailed`]；
/// 调用方已按 `DISPLAY` 在场选路，此处的失败是「环境变量在而服务不可达」
/// ——socket 权限、拼写错误、XWayland 缺席等）。
fn connect() -> Result<(RustConnection, usize), Error> {
    x11rb::connect(None).map_err(|error| Error::X11ConnectFailed(error.to_string()))
}

/// X11 请求阶段错误统一收敛（op 标注阶段名，source 为 x11rb 错误字符串）。
fn x11_err(op: &'static str) -> impl Fn(x11rb::errors::ConnectionError) -> Error + 'static {
    move |error| Error::X11Protocol { op, detail: error.to_string() }
}

/// X11 截屏入口（由 [`super::capture_screen_linux`] 在 `DISPLAY` 在场时选路
/// 到此）。`width <= 0 || height <= 0` = 整屏哨兵（根窗口几何自查）。
pub(crate) fn capture_screen(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<ScreenCapture, Error> {
    let (conn, screen_num) = connect()?;
    let screen = &conn.setup().roots[screen_num];
    let (root_w, root_h) = (i32::from(screen.width_in_pixels), i32::from(screen.height_in_pixels));
    // 哨兵：非正尺寸 = 整屏（根窗口几何；screen.rs 的 Linux 整屏入口传
    // (0,0,0,0) 即走此分支，不在此臆造尺寸）。
    let (left, top, w, h) = if width <= 0 || height <= 0 {
        (0, 0, root_w, root_h)
    } else {
        clamp_to_root(x, y, width, height, root_w, root_h).ok_or(Error::ScreenRegionDisjointX11 {
            x,
            y,
            width,
            height,
            vw: root_w,
            vh: root_h,
        })?
    };
    // GetImage(ZPixmap)：根窗口全平面、32bpp 行序返回（depth 24/32 的
    // ZPixmap 数据按像素 4 字节打包）。坐标/尺寸经钳制后必在 u16 范围内
    // （根窗口尺寸本身即 u16）。
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            left as i16,
            top as i16,
            w as u16,
            h as u16,
            u32::MAX,
        )
        .map_err(x11_err("GetImage"))?
        .reply()
        .map_err(|error| Error::X11Protocol { op: "GetImage", detail: error.to_string() })?;
    if reply.depth != 24 && reply.depth != 32 {
        return Err(Error::X11Protocol {
            op: "GetImage",
            detail: format!("不支持的根窗口深度 {}（仅支持 24/32bpp ZPixmap）", reply.depth),
        });
    }
    let expected = w as usize * h as usize * 4;
    if reply.data.len() < expected {
        return Err(Error::X11Protocol {
            op: "GetImage",
            detail: format!("像素数据不足（{} < {expected} 字节）", reply.data.len()),
        });
    }
    // 字节序：ZPixmap 像素内字节序随 server 的 image_byte_order——LSBFirst
    // 为 [B,G,R,X]、MSBFirst 为 [X,R,G,B]（大端 X server 罕见但存在）。
    let little_endian = matches!(conn.setup().image_byte_order, ImageOrder::LSB_FIRST);
    let rgb = bgra_to_rgb(&reply.data[..expected], little_endian);
    let png = encode_png(&rgb, w as u32, h as u32)
        .map_err(|error| Error::X11Protocol { op: "PngEncode", detail: error.to_string() })?;
    Ok(ScreenCapture { png, width: w, height: h })
}

/// 4 字节/像素 → 3 字节 RGB 去重（行内逐像素；ZPixmap 32bpp 行距恰为
/// w*4，无行尾填充需要跳过）。
fn bgra_to_rgb(data: &[u8], little_endian: bool) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(data.len() / 4 * 3);
    for px in data.chunks_exact(4) {
        let (r, g, b) = if little_endian {
            (px[2], px[1], px[0])
        } else {
            (px[1], px[2], px[3])
        };
        rgb.extend_from_slice(&[r, g, b]);
    }
    rgb
}

/// RGB888 → PNG 字节（内存编码，不落盘；Windows 侧走 WIC，两平台互不引入）。
fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, png::EncodingError> {
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgb)?;
    // 显式先 drop writer（其借用 buf）再返回——借用检查器无法推断
    // writer 的生命周期在此结束。
    drop(writer);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本环境是否有真实 X 会话（无 DISPLAY 的 headless CI 跳过真实抓屏）。
    fn display_present() -> bool {
        std::env::var("DISPLAY").is_ok_and(|value| !value.trim().is_empty())
    }

    /// PNG 魔数 + IHDR 尺寸核对（真实测试共用断言底座）。
    fn assert_png_with_dims(png: &[u8], width: i32, height: i32) {
        assert!(
            png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            "PNG 魔数不符"
        );
        assert_eq!(&png[12..16], b"IHDR", "首块应为 IHDR");
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((w, h), (width as u32, height as u32), "IHDR 尺寸与出参一致");
    }

    /* --- 纯逻辑（钳制/像素换序：无 X 依赖） -------------------------------- */

    #[test]
    fn clamp_keeps_fully_contained_region_intact() {
        assert_eq!(clamp_to_root(10, 20, 100, 50, 1920, 1080), Some((10, 20, 100, 50)));
    }

    #[test]
    fn clamp_crops_out_of_bounds_request() {
        // 右/下越界：裁到根窗口边缘。
        assert_eq!(clamp_to_root(1900, 1000, 100, 200, 1920, 1080), Some((1900, 1000, 20, 80)));
        // 全部越界但首像素在屏内。
        assert_eq!(clamp_to_root(1919, 1079, 500, 500, 1920, 1080), Some((1919, 1079, 1, 1)));
    }

    #[test]
    fn clamp_rejects_negative_origin_beyond_root() {
        // X11 根窗口原点恒 (0,0)：负起点被抬到 0，请求全在屏外 → 空交。
        assert_eq!(clamp_to_root(-100, -100, 50, 50, 1920, 1080), None);
        // 部分在屏内：从 0 开始裁剪。
        assert_eq!(clamp_to_root(-30, -40, 100, 60, 1920, 1080), Some((0, 0, 70, 20)));
    }

    #[test]
    fn clamp_rejects_disjoint_and_handles_saturation() {
        assert_eq!(clamp_to_root(5000, 5000, 10, 10, 1920, 1080), None, "屏外无交");
        // 饱和加法：width = i32::MAX 不溢出为负。
        assert_eq!(clamp_to_root(0, 0, i32::MAX, i32::MAX, 1920, 1080), Some((0, 0, 1920, 1080)));
        // 请求尺寸为 0/负：钳制后 <1 → None（整屏哨兵在调用方先行分流，
        // 到这里的 0 尺寸 = 显式区域请求的退化形态，按空交处理）。
        assert_eq!(clamp_to_root(10, 10, 0, 0, 1920, 1080), None);
    }

    #[test]
    fn bgra_to_rgb_respects_byte_order() {
        // LSBFirst：[B,G,R,X]。
        assert_eq!(bgra_to_rgb(&[0x11, 0x22, 0x33, 0xFF], true), vec![0x33, 0x22, 0x11]);
        // MSBFirst：[X,R,G,B]。
        assert_eq!(bgra_to_rgb(&[0xFF, 0x33, 0x22, 0x11], false), vec![0x33, 0x22, 0x11]);
        // 多像素逐行换序。
        assert_eq!(
            bgra_to_rgb(&[1, 2, 3, 4, 5, 6, 7, 8], true),
            vec![3, 2, 1, 7, 6, 5]
        );
    }

    /* --- 真实抓屏（DISPLAY 在场才跑；GetImage 只读无副作用） --------------- */

    /// 整屏哨兵 → 根窗口几何自查：真实 X 会话下 `(0,0,0,0)` 出根窗口尺寸
    /// 的合法 PNG。本机 `DISPLAY=:1` 实测（upgrade.md §7 验收）。
    #[test]
    fn captures_full_screen_on_real_display() {
        if !display_present() {
            return; // headless CI：无 X 会话，降级路径另测（linux/mod.rs）。
        }
        let shot = capture_screen(0, 0, 0, 0).expect("DISPLAY 在场 → GetImage 应成功");
        assert!(shot.width > 0 && shot.height > 0, "根窗口几何非正: {}x{}", shot.width, shot.height);
        assert_png_with_dims(&shot.png, shot.width, shot.height);
        // 1920x1080 的 RGB888 约 6MB 上界——尺寸合理性粗检（防巨幅错报）。
        assert!(shot.png.len() < 64 * 1024 * 1024);
    }

    /// 区域截屏：真实会话下请求 64x40（钳制到根窗口内），尺寸逐字节一致。
    #[test]
    fn captures_region_on_real_display() {
        if !display_present() {
            return;
        }
        let shot = capture_screen(0, 0, 64, 40).expect("区域 GetImage 应成功");
        assert_eq!((shot.width, shot.height), (64, 40));
        assert_png_with_dims(&shot.png, 64, 40);
    }

    /// 屏外区域 → 空交错误（真实会话下根窗口几何参与判定）。
    #[test]
    fn rejects_disjoint_region_on_real_display() {
        if !display_present() {
            return;
        }
        let error = capture_screen(1 << 20, 1 << 20, 10, 10).unwrap_err();
        assert!(matches!(error, Error::ScreenRegionDisjointX11 { .. }), "{error}");
    }
}
