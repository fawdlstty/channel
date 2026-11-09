//! 子进程输出 / 本地文件字节解码：Windows 旧程序（cmd、PowerShell 5.x、
//! reg 等）按系统 OEM/ANSI 码页（zh-CN 为 GBK/936）写管道，直接按 UTF-8
//! lossy 解会产出 U+FFFD 与误读字符（如 GBK 字节里的合法三字节序列被读成
//! "◆"）。这里统一走"显式 BOM → 完整 UTF-8 → 系统码页"的探测顺序，任何
//! 一环命中即返回；码页转换用 kernel32 `MultiByteToWideChar`（零新增依赖）。

/// 解码子进程输出字节。UTF-8/UTF-16 BOM 显式识别（UTF-8 BOM 剥离），完整
/// UTF-8 原样采用，否则按系统 OEM 码页解码（非 Windows 回落 UTF-8 lossy）。
pub fn decode_child_output(bytes: &[u8]) -> String {
    if let Some(text) = decode_utf8_bom_stripped(bytes) {
        return text;
    }
    decode_detected(bytes)
}

/// 解码本地文件字节，语义与 [`decode_child_output`] 相同，唯独保留 UTF-8
/// BOM：Read/Edit 对 PowerShell 脚本等 BOM 文件的原样回写依赖它。
pub fn decode_file_bytes(bytes: &[u8]) -> String {
    decode_detected(bytes)
}

fn decode_utf8_bom_stripped(bytes: &[u8]) -> Option<String> {
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF])?;
    Some(String::from_utf8_lossy(rest).into_owned())
}

fn decode_detected(bytes: &[u8]) -> String {
    // UTF-16 BOM：显式双字节序识别。
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, false);
    }
    // 完整 UTF-8 快路径（node/git/rg 等现代程序的管道输出几乎总命中）。
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    decode_legacy(bytes)
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let (pairs, rest) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    let mut text = String::from_utf16_lossy(&units);
    // 尾部奇数字节不成码元，按替换符补齐（与 from_utf16_lossy 语义一致）。
    if !rest.is_empty() {
        text.push('\u{FFFD}');
    }
    text
}

#[cfg(not(windows))]
fn decode_legacy(bytes: &[u8]) -> String {
    // Unix 侧子进程输出由 locale 决定，主流环境均为 UTF-8；非 UTF-8 时
    // 按替换符降级（原行为）。
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn decode_legacy(bytes: &[u8]) -> String {
    use std::sync::OnceLock;

    fn oem_code_page() -> u32 {
        static PAGE: OnceLock<u32> = OnceLock::new();
        *PAGE.get_or_init(|| unsafe { GetOEMCP() })
    }

    if bytes.is_empty() {
        return String::new();
    }
    // MBCS 码页每字符至多 2 字节、SBCS 1 字节，len 个 u16 必然够放。
    let mut buffer = vec![0_u16; bytes.len()];
    let written = unsafe {
        MultiByteToWideChar(
            oem_code_page(),
            0,
            bytes.as_ptr(),
            bytes.len().min(i32::MAX as usize) as i32,
            buffer.as_mut_ptr(),
            buffer.len() as i32,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    buffer.truncate(written as usize);
    String::from_utf16_lossy(&buffer)
}

#[cfg(windows)]
extern "system" {
    #[link_name = "MultiByteToWideChar"]
    fn MultiByteToWideChar(
        code_page: u32,
        flags: u32,
        source: *const u8,
        source_length: i32,
        destination: *mut u16,
        destination_length: i32,
    ) -> i32;
    #[link_name = "GetOEMCP"]
    fn GetOEMCP() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GBK 字节（"中文" = D6 D0 CE C4）按码页解回原文本。Windows 专属：
    /// Unix 无码表，回落 lossy。
    #[cfg(windows)]
    #[test]
    fn decodes_gbk_child_output() {
        assert_eq!(decode_child_output(&[0xD6, 0xD0, 0xCE, 0xC4]), "中文");
    }

    #[cfg(windows)]
    #[test]
    fn decodes_gbk_command_error() {
        // cmd 的错误文本："不是内部或外部命令" 的 GBK 字节（手算码位）。
        let bytes = [
            0xB2, 0xBB, // 不
            0xCA, 0xC7, // 是
            0xC4, 0xDA, // 内
            0xB2, 0xBF, // 部
            0xBB, 0xF2, // 或
            0xCD, 0xE2, // 外
            0xB2, 0xBF, // 部
            0xC3, 0xFC, // 命
            0xC1, 0xEE, // 令
        ];
        assert_eq!(decode_child_output(&bytes), "不是内部或外部命令");
    }

    #[test]
    fn keeps_valid_utf8() {
        assert_eq!(decode_child_output("中文输出".as_bytes()), "中文输出");
    }

    #[test]
    fn strips_utf8_bom_in_child_output() {
        let bytes = [0xEF, 0xBB, 0xBF].iter().copied().chain("ok".bytes()).collect::<Vec<u8>>();
        assert_eq!(decode_child_output(&bytes), "ok");
    }

    #[test]
    fn keeps_utf8_bom_in_file_bytes() {
        let bytes = [0xEF, 0xBB, 0xBF].iter().copied().chain("ok".bytes()).collect::<Vec<u8>>();
        assert_eq!(decode_file_bytes(&bytes), "\u{FEFF}ok");
    }

    #[test]
    fn decodes_utf16le_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "中文".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_child_output(&bytes), "中文");
    }
}
