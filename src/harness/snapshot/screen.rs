//! 只读屏幕截图原语（v1.13；design.md §10.2「每步开始前截图存 runs/<ts>/
//! （经 harness 感知面）」＋ §22.4 H3 只读感知工具面）。
//!
//! - 实现路径（仅 Windows 真实抓屏）：GDI `CreateDIBSection`（32bpp 顶朝下
//!   DIB）＋ `BitBlt(SRCCOPY | CAPTUREBLT，个别驱动不兼容时回退 SRCCOPY)`
//!   抓**整个虚拟屏幕**（分层窗口/工具提示入图；多显示器一体成图，
//!   `GetSystemMetrics(SM_*VIRTUALSCREEN)` 定位原点与尺寸），再经 WIC
//!   （Windows Imaging Component）在内存中编码 PNG 字节——纯系统组件，
//!   导出物零第三方依赖（Python 侧纯 stdlib 环境亦有截图能力）；
//! - `capture_screen` 是普通只读 op：不依赖 UIA 探测（GDI 独立可用），
//!   非 Windows 平台分发到 [`super::linux`] 接线层（Linux：X11/portal，
//!   P1/P3 填充；当前骨架诚实降级 `no-display-server`，serve 层转
//!   `ok:false`——该降级路径有合成单测）；
//! - `capture_screen_region`（P3-12）：区域截图——整屏 base64 单行可达
//!   数十 MB，调用方可指定屏幕坐标矩形只抓局部；请求矩形与虚拟屏求交
//!   钳制（越界部分自然裁掉），空交报错。`capture_screen` 即「以虚拟屏
//!   矩形调用区域版」，两入口共用同一条 GDI 路径；
//! - **不落盘**：只回 PNG 字节（协议层 base64），路径由调用方决定；
//! - COM 初始化沿用 [`super::uia`] 收敛的 thread_local 每线程一次模式
//!   （ rationale 详见其模块注释；两处各自持有 thread_local，`CoInitializeEx`
//!   幂等，同线程先后调用互不冲突）。

use crate::harness::Error;

/// `capture_screen` / `capture_screen_region` op 的产物（PNG 字节 + 捕获
/// 区域尺寸；不落盘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCapture {
    /// PNG 编码字节（协议层经标准字母表 base64 下发为 `pngBase64`）。
    pub png: Vec<u8>,
    /// 捕获区域宽（像素；>0；整屏入口 = 虚拟屏宽）。
    pub width: i32,
    /// 捕获区域高（像素；>0；整屏入口 = 虚拟屏高）。
    pub height: i32,
}

/// Linux 整屏入口：分发到 Linux 接线层 [`crate::harness::snapshot::linux::capture_screen_linux`]
/// （X11/portal 由 P1/P3 填充，见其模块文档）。骨架期（无显示服务实现）
/// 诚实降级 `Err(NoDisplayServer)`——`code: no-display-server` 供宿主
/// 程序化分支。参数 `(0,0,0,0)` 为「矩形未探测」哨兵：真实全屏矩形由
/// Linux 实现层自查（X11 根窗口几何），不在此臆造。
#[cfg(target_os = "linux")]
#[inline]
pub fn capture_screen() -> Result<ScreenCapture, Error> {
    crate::harness::snapshot::linux::capture_screen_linux(0, 0, 0, 0)
}

/// 区域截图（P3-12，Linux 分发版；语义详见 Windows 分支的同名文档）：
/// 委托 [`crate::harness::snapshot::linux::capture_screen_linux`]（求交钳制/空交
/// 报错在实现层，P1 填充；参数校验已在工具层完成，此处不再重复判定）。
#[cfg(target_os = "linux")]
#[inline]
pub fn capture_screen_region(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<ScreenCapture, Error> {
    crate::harness::snapshot::linux::capture_screen_linux(x, y, width, height)
}

/// 其余平台（非 Windows、非 Linux）整屏桩：无 GDI/WIC 也无 Linux 接线层，
/// ok:false 降级。
#[cfg(not(any(windows, target_os = "linux")))]
#[inline]
pub fn capture_screen() -> Result<ScreenCapture, Error> {
    Err(Error::ScreenNonWindows)
}

/// 区域入口的「其余平台」桩（与整屏桩同语义）。
#[cfg(not(any(windows, target_os = "linux")))]
#[inline]
pub fn capture_screen_region(
    _x: i32,
    _y: i32,
    _width: i32,
    _height: i32,
) -> Result<ScreenCapture, Error> {
    Err(Error::ScreenNonWindows)
}

#[cfg(windows)]
pub fn capture_screen() -> Result<ScreenCapture, Error> {
    // 整屏 = 以虚拟屏矩形调用区域版（同一 GDI 路径；请求矩形即虚拟屏
    // 本身，钳制为恒等——无重复实现，行为与区域版严格一致）。
    let (x, y, width, height) = virtual_screen_rect()?;
    capture_screen_region(x, y, width, height)
}

/// 区域截图（P3-12，Windows 实现；语义详见非 Windows 分支的同名文档）：
/// 请求矩形与虚拟屏求交钳制，空交报错。
#[cfg(windows)]
pub fn capture_screen_region(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<ScreenCapture, Error> {
    let raw = capture_region_bgra(x, y, width, height)?;
    let png = encode_png(&raw)?;
    Ok(ScreenCapture { png, width: raw.width, height: raw.height })
}

/* --- Windows 实现（GDI BitBlt + WIC PNG 编码） --------------------------------- */

#[cfg(windows)]
mod imp {
    use std::cell::Cell;

    use windows::core::Interface;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
        IWICBitmapFrameEncode, IWICBitmapSource, IWICImagingFactory, WICBitmapEncoderNoCache,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        CreateStreamOnHGlobal, GetHGlobalFromStream,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    use crate::harness::Error;

    /// GDI/WIC 阶段的中间产物：整屏 BGRA 像素 + 尺寸（顶朝下行序）。
    pub(super) struct RawScreen {
        pub pixels: Vec<u8>,
        pub width: i32,
        pub height: i32,
    }

    thread_local! {
        /// 本线程 COM 已完成（或已尝试）MTA 初始化（每线程一次；模式与
        /// [`super::super::uia`] 的 `ensure_com_initialized` 一致）。
        static COM_INITIALIZED: Cell<bool> = const { Cell::new(false) };
    }

    /// 本线程 COM MTA 就绪（每线程首次调用时初始化一次，此后跳过；不配对
    /// `CoUninitialize`——MTA 生命周期进程级，见 uia.rs 的同款论证）。
    pub(super) fn ensure_com_initialized() {
        COM_INITIALIZED.with(|init| {
            if !init.get() {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                // 即便返回 RPC_E_CHANGED_MODE 也标记为已尝试（apartment 不再变）。
                init.set(true);
            }
        });
    }

    /// 虚拟屏矩形 `(x, y, width, height)`（多显示器一体；原点可为负）。
    /// 无交互桌面会话（宽/高 ≤0）→ Err。
    pub(super) fn virtual_screen_rect() -> Result<(i32, i32, i32, i32), Error> {
        let (x, y, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if width <= 0 || height <= 0 {
            return Err(Error::VirtualScreenInvalid { width, height });
        }
        Ok((x, y, width, height))
    }

    /// GDI 抓屏幕区域到内存 DIB（32bpp 顶朝下），返回 BGRA 像素副本 +
    /// **钳制后**的实际尺寸。请求矩形先与虚拟屏求交（坐标不设限——越界
    /// 部分自然裁掉；空交/钳制后宽高 <1 报错），再走 BitBlt（CAPTUREBLT
    /// 优先 + 失败回退 SRCCOPY，P2-5 语义在区域路径同样保留）。
    pub(super) fn capture_region_bgra(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<RawScreen, Error> {
        // 钳制：请求矩形 ∩ 虚拟屏（饱和加法防溢出；多显示器坐标可为负）。
        let (vx, vy, vw, vh) = virtual_screen_rect()?;
        let left = x.max(vx);
        let top = y.max(vy);
        let right = x.saturating_add(width).min(vx.saturating_add(vw));
        let bottom = y.saturating_add(height).min(vy.saturating_add(vh));
        let (clamped_w, clamped_h) = (right - left, bottom - top);
        if clamped_w < 1 || clamped_h < 1 {
            return Err(Error::ScreenRegionDisjoint {
                x,
                y,
                width,
                height,
                vx,
                vy,
                vw,
                vh,
            });
        }
        let (width, height) = (clamped_w, clamped_h);
        // 32bpp 顶朝下 DIB（biHeight 为负 = 首行在顶；与 WIC 行序一致）。
        // biCompression 在 windows crate 里即 u32（BI_RGB.0）。
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe {
            let screen_dc = GetDC(None);
            if screen_dc.is_invalid() {
                return Err(Error::GetScreenDcFailed);
            }
            // GDI 句柄全程手动回收（Rust 无 RAII；逐项映射错误摘要）。
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            if mem_dc.is_invalid() {
                ReleaseDC(None, screen_dc);
                return Err(Error::CreateCompatibleDcFailed);
            }
            let bitmap = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .map_err(|source| {
                    ReleaseDC(None, screen_dc);
                    let _ = DeleteDC(mem_dc);
                    Error::CreateDibSectionFailed(source)
                })?;
            let old = SelectObject(mem_dc, bitmap.into());
            // CAPTUREBLT：分层窗口/工具提示等入图（P2-5）——不带它时悬浮
            // 提示、部分右键菜单实现、拖拽幽灵等「屏幕上看得见、截图里没
            // 有」。个别驱动下该组合会闪屏/失败 → 失败时回退不带
            // CAPTUREBLT 的普通 blit（原行为），仍失败走原错误路径。
            // BitBlt 源坐标 = 钳制后矩形左上角（屏幕坐标系）。
            let mut blit = BitBlt(
                mem_dc,
                0,
                0,
                width,
                height,
                Some(screen_dc),
                left,
                top,
                SRCCOPY | CAPTUREBLT,
            );
            if blit.is_err() {
                blit = BitBlt(mem_dc, 0, 0, width, height, Some(screen_dc), left, top, SRCCOPY);
            }
            // 像素先拷出（blit 结果仍判定，但清理不受影响）。
            let stride = width as usize * 4;
            let pixels = if bits.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(bits as *const u8, stride * height as usize).to_vec()
            };
            SelectObject(mem_dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
            blit.map_err(Error::BitBltFailed)?;
            if pixels.is_empty() {
                return Err(Error::EmptyDibBuffer);
            }
            Ok(RawScreen { pixels, width, height })
        }
    }

    /// WIC 内存编码 BGRA 像素 → PNG 字节（不落盘；HGLOBAL 流随 IStream
    /// 释放自动回收——`CreateStreamOnHGlobal(None, true)`）。
    pub(super) fn encode_png(raw: &RawScreen) -> Result<Vec<u8>, Error> {
        ensure_com_initialized();
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(Error::WicFactoryFailed)?;
            let bitmap = factory
                .CreateBitmapFromMemory(
                    raw.width as u32,
                    raw.height as u32,
                    &GUID_WICPixelFormat32bppBGRA,
                    (raw.width as usize * 4) as u32,
                    &raw.pixels,
                )
                .map_err(Error::WicBitmapFailed)?;
            let source: IWICBitmapSource = bitmap.cast().map_err(Error::WicSourceCastFailed)?;
            let stream = CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true)
                .map_err(Error::MemoryStreamCreateFailed)?;
            let encoder = factory
                .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())
                .map_err(Error::PngEncoderCreateFailed)?;
            encoder
                .Initialize(&stream, WICBitmapEncoderNoCache)
                .map_err(Error::EncoderInitFailed)?;
            let mut frame_opt: Option<IWICBitmapFrameEncode> = None;
            encoder
                .CreateNewFrame(&mut frame_opt, std::ptr::null_mut())
                .map_err(Error::FrameCreateFailed)?;
            let frame = frame_opt.ok_or(Error::FrameEmpty)?;
            frame.Initialize(None).map_err(Error::FrameInitFailed)?;
            frame
                .SetSize(raw.width as u32, raw.height as u32)
                .map_err(Error::FrameSizeSetFailed)?;
            frame
                .WriteSource(&source, std::ptr::null())
                .map_err(Error::PngWriteFailed)?;
            frame.Commit().map_err(Error::FrameCommitFailed)?;
            encoder.Commit().map_err(Error::EncoderCommitFailed)?;
            // 编码完成 → 从后备 HGLOBAL 取出字节（流随后释放并自动回收内存）。
            let hglobal = GetHGlobalFromStream(&stream).map_err(Error::HGlobalFromStreamFailed)?;
            let size = GlobalSize(hglobal);
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return Err(Error::WicLockFailed);
            }
            let png = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
            // GlobalUnlock 特例（Win32 文档）：解锁计数归零时返回 FALSE 且
            // GetLastError()==NO_ERROR——windows crate 包装为空 Err（0x0），
            // 属成功路径，按解锁完成处理。
            if let Err(source) = GlobalUnlock(hglobal) {
                if !source.code().is_ok() {
                    return Err(Error::WicUnlockFailed(source));
                }
            }
            drop(stream); // fDeleteOnRelease=true → HGLOBAL 随流释放回收
            Ok(png)
        }
    }
}

#[cfg(windows)]
use imp::{capture_region_bgra, encode_png, virtual_screen_rect};

/* --- base64（标准字母表；零新依赖的纯逻辑） ------------------------------------- */

/// 标准字母表（RFC 4648 §4）base64 编码，含 `=` 填充。手写实现避免为
/// 一个编码器引入新依赖（harness 依赖面保持 serde/windows 最小集）；
/// RFC 4648 测试向量单测锁定。
pub(crate) fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |&b| u32::from(b));
        let b2 = chunk.get(2).map_or(0, |&b| u32::from(b));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_handles_full_byte_range() {
        let data: Vec<u8> = (0..=255u8).collect();
        let encoded = base64_encode(&data);
        // 256 字节 → 86 组余 1 → 长度 86*4+4、尾部 "=="。
        assert_eq!(encoded.len(), 4 * data.len().div_ceil(3));
        assert!(encoded.ends_with("=="), "余 1 字节补两处填充: {encoded}");
        assert!(
            encoded
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'='),
            "仅标准字母表与填充符"
        );
    }

    /// Linux 降级/真实双路径：无 DISPLAY（纯 TTY/headless CI）→ 诚实降级
    /// `no-display-server`；有 DISPLAY（如本机 :1）→ X11 实现接管（成功
    /// 路径由 x11_screen.rs 的真实测试锁定，此处不重复断言）。
    #[cfg(target_os = "linux")]
    #[test]
    fn capture_screen_fails_closed_without_display_implementation() {
        if std::env::var("DISPLAY").is_ok_and(|value| !value.trim().is_empty()) {
            return; // 本机有 X 会话：走 X11 实现（另有测试），降级分支不适用。
        }
        let err = super::capture_screen().unwrap_err();
        assert_eq!(err.code(), Some("no-display-server"), "{err}");
    }

    /// 区域入口（P3-12）Linux 语义：无 DISPLAY 时参数值不影响结果（校验
    /// 在工具层，坐标钳制在 X11 实现层——后者由 x11_screen.rs 测试覆盖）。
    #[cfg(target_os = "linux")]
    #[test]
    fn capture_screen_region_fails_closed_without_display_implementation() {
        if std::env::var("DISPLAY").is_ok_and(|value| !value.trim().is_empty()) {
            return;
        }
        let err = super::capture_screen_region(0, 0, 800, 600).unwrap_err();
        assert_eq!(err.code(), Some("no-display-server"), "{err}");
        // 越界坐标同样走降级（无显示服务时坐标语义不存在）。
        assert!(super::capture_screen_region(-99_999, -99_999, 10, 10).is_err());
    }
}
