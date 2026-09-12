#![cfg(feature = "harness")]
//! 真实抓屏冒烟（#[ignore]：需真实交互桌面会话；照 tests/uia_smoke.rs
//! 惯例，环境受限时 #[ignore] 降级）。
//!
//! 运行：`cargo test -p harness --test screen_smoke -- --ignored`
//!
//! 验证：`capture_screen` 只读 op（经 serve 同款 [`ToolRegistry`]——GDI
//! 不依赖 UIA 探测，`without_probe` 注册表即可路由）产出合法 PNG：
//! - 协议字段 `pngBase64/width/height` 齐备，宽高 >0；
//! - base64（标准字母表）解码后以 PNG magic bytes `\x89PNG\r\n\x1a\n` 开头；
//! - IHDR 内的图像尺寸与协议 `width/height` 一致，解码长度合理；
//! - 区域参数（P3-12）：`x/y/width/height` 只抓局部（PNG 尺寸 = 请求
//!   尺寸），大范围负偏移请求与虚拟屏求交后 ≈ 整屏尺寸（钳制语义）。
//!
//! 冒烟不注入任何输入（只读感知），不落盘。

use serde_json::{json, Value};
use channel::harness::tools::ToolRegistry;

/// 测试内联 base64 解码（标准字母表 + `=` 填充；与 crate 侧
/// `base64_encode` 对偶——集成测试仅见公共 API，解码器就地实现）。
fn base64_decode(s: &str) -> Vec<u8> {
    fn value_of(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|b| value_of(*b).is_some())
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 3);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value_of(c).expect("标准字母表字符") << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

#[test]
#[ignore = "真实桌面抓屏冒烟：需交互会话；无头/受限环境跳过"]
fn real_capture_screen_produces_valid_png_of_virtual_screen() {
    let reg = ToolRegistry::without_probe();
    let result = reg
        .call("capture_screen", &json!({}))
        .expect("GDI 抓屏应成功（交互桌面会话）");
    let Value::Object(ref fields) = result else {
        panic!("capture_screen 应返回对象: {result}");
    };
    assert!(
        fields.contains_key("pngBase64") && fields.contains_key("width") && fields.contains_key("height"),
        "协议字段 pngBase64/width/height 齐备: {fields:?}"
    );
    let b64 = result["pngBase64"].as_str().expect("pngBase64 为字符串");
    assert!(!b64.is_empty(), "pngBase64 非空");
    let width = result["width"].as_i64().expect("width 为数值");
    let height = result["height"].as_i64().expect("height 为数值");
    assert!(width > 0 && height > 0, "虚拟屏尺寸必须 >0: {width}x{height}");
    println!("虚拟屏: {width}x{height}");

    // 解码并验证 PNG 结构：magic bytes + IHDR 尺寸一致 + 长度合理。
    let png = base64_decode(b64);
    assert!(png.len() >= 24, "PNG 至少含签名+IHDR 头（{}B）", png.len());
    assert_eq!(
        &png[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "PNG magic bytes"
    );
    let ihdr_w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let ihdr_h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    assert_eq!(i64::from(ihdr_w), width, "IHDR 宽 = 协议 width");
    assert_eq!(i64::from(ihdr_h), height, "IHDR 高 = 协议 height");
    // 长度合理：压缩后不小于纯结构开销，也不超过未压缩 BGRA 位图的两倍。
    let raw_bgra = width as usize * height as usize * 4;
    assert!(png.len() > 100, "PNG 结构应完整（{}B）", png.len());
    assert!(
        png.len() <= raw_bgra * 2 + 1024,
        "PNG 不应显著大于原始位图: {} vs {raw_bgra}",
        png.len()
    );
    println!("PNG: {} 字节（未压缩 BGRA {raw_bgra} 字节）", png.len());
}

/// 区域截图冒烟（P3-12）：合法区域只抓局部（PNG 尺寸 = 请求尺寸）；
/// 大范围负偏移请求经虚拟屏钳制后 ≈ 整屏尺寸（越界部分自然裁掉）。
/// 参数校验的纯逻辑断言见 `tools` 模块单测，此处验证 Windows 实现侧。
#[test]
#[ignore = "真实桌面抓屏冒烟：需交互会话；无头/受限环境跳过"]
fn real_capture_screen_region_returns_clamped_valid_png() {
    let reg = ToolRegistry::without_probe();
    // 基准：整屏（无参 = 虚拟屏尺寸）。
    let full = reg.call("capture_screen", &json!({})).expect("整屏抓屏应成功");
    let vw = full["width"].as_i64().expect("width 为数值");
    let vh = full["height"].as_i64().expect("height 为数值");

    // 局部区域：主屏左上 200x100（主屏原点 (0,0) 属虚拟屏），钳制为恒等。
    let region = reg
        .call("capture_screen", &json!({ "x": 0, "y": 0, "width": 200, "height": 100 }))
        .expect("区域抓屏应成功");
    let width = region["width"].as_i64().expect("width 为数值");
    let height = region["height"].as_i64().expect("height");
    assert_eq!((width, height), (200, 100), "屏内区域不被裁: {region:?}");
    let b64 = region["pngBase64"].as_str().expect("pngBase64 为字符串");
    let png = base64_decode(b64);
    assert!(png.len() < base64_decode(full["pngBase64"].as_str().expect("整屏 base64")).len(),
        "区域 PNG 应显著小于整屏");
    let ihdr_w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let ihdr_h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    assert_eq!((i64::from(ihdr_w), i64::from(ihdr_h)), (width, height), "IHDR = 协议宽高");

    // 大范围负偏移（宽/高受 10000 上限约束）：请求覆盖虚拟屏左上大半，
    // 钳制后尺寸 ≤ 请求尺寸且 ≤ 虚拟屏尺寸、>0，PNG 结构一致。
    let clamped = reg
        .call("capture_screen", &json!({ "x": -5000, "y": -5000, "width": 10000, "height": 10000 }))
        .expect("钳制区域抓屏应成功（与虚拟屏有交集）");
    let cw = clamped["width"].as_i64().expect("width");
    let ch = clamped["height"].as_i64().expect("height");
    assert!(cw > 0 && ch > 0, "钳制后尺寸 >0: {cw}x{ch}");
    assert!(cw <= 10_000 && cw <= vw && ch <= 10_000 && ch <= vh,
        "钳制后不超请求/虚拟屏: {cw}x{ch}（请求 10000x10000，虚拟屏 {vw}x{vh}）");
    let png = base64_decode(clamped["pngBase64"].as_str().expect("pngBase64"));
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], "PNG magic bytes");
    let ihdr_w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let ihdr_h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    assert_eq!((i64::from(ihdr_w), i64::from(ihdr_h)), (cw, ch), "IHDR = 钳制后协议宽高");
    println!("钳制区域: {cw}x{ch}（请求 -5000,-5000 10000x10000；虚拟屏 {vw}x{vh}）");
}
