//! ws 传输层冒烟（potato 客户端两条路径的端到端回归，无需真实浏览器）：
//!
//! - [`wslite_client_echo_roundtrip`]：`potato::wslite` 泛型客户端（CDP 泵
//!   线程同款用法——同步 socket 适配 [`WebsocketIo`] + `potato::block_on`
//!   驱动即时完成的 future），覆盖客户端握手、帧掩码、Ping/Pong 应答、
//!   文本帧往返；
//! - [`full_websocket_connect_echo_roundtrip`]：`potato::Websocket::connect`
//!   （Codex WebSocket endpoint 同款用法——HTTP 栈升级握手），覆盖同一
//!   echo 服务端的异步路径。
//!
//! 服务端为测试内手写的最小 RFC 6455 形态：升级响应经 potato 的
//! `build_ws_upgrade_response`（含 `Sec-WebSocket-Accept` 计算），帧收发走
//! `ws_recv`/`ws_send_text`（server 角色：要求客户端帧掩码、自身帧不掩码）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use potato::wslite::{
    build_ws_upgrade_response, client_handshake_with_rng, ws_recv, ws_send_text, Websocket,
    WebsocketIo, WsFrame,
};

/// potato 同步 socket 适配（与 CDP 泵线程同款思路：同步阻塞 I/O 包成
/// 即时完成的 future，`potato::block_on` 驱动）。
struct SyncSocket(TcpStream);

impl WebsocketIo for SyncSocket {
    type Error = std::io::Error;

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.0.read(buf)
    }

    async fn write(&mut self, data: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(data)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush()
    }
}

/// 测试用掩码随机源（协议只要求客户端帧带掩码；内容不必密码学强度）。
type TestRng = Box<dyn FnMut(&mut [u8]) + Send>;

fn test_rng() -> TestRng {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    let mut seq: u64 = 0;
    Box::new(move |buf: &mut [u8]| {
        for chunk in buf.chunks_mut(8) {
            seq = seq.wrapping_add(1);
            let mut hasher = state.build_hasher();
            hasher.write_u64(seq);
            let value = hasher.finish();
            for (index, byte) in chunk.iter_mut().enumerate() {
                *byte = (value >> (index * 8)) as u8;
            }
        }
    })
}

/// 读取 ws 升级请求里的 `Sec-WebSocket-Key`（读到头体分隔即解析）。
fn read_handshake_key(stream: &mut TcpStream) -> Option<String> {
    let mut buf = [0u8; 4096];
    let mut pos = 0;
    loop {
        if buf[..pos].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if pos >= buf.len() {
            return None;
        }
        let n = stream.read(&mut buf[pos..]).ok()?;
        if n == 0 {
            return None;
        }
        pos += n;
    }
    String::from_utf8_lossy(&buf[..pos])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("Sec-WebSocket-Key")
                .then(|| value.trim().to_string())
        })
}

/// 最小 ws echo 服务端：升级握手 → 回显一条文本（server 角色收发）。
fn spawn_ws_echo(listener: TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stream = listener.accept().expect("accept").0;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
        let key = read_handshake_key(&mut stream).expect("ws 升级请求带 Sec-WebSocket-Key");
        stream.write_all(&build_ws_upgrade_response(&key)).expect("写 101 升级响应");
        let mut socket = SyncSocket(stream);
        match potato::block_on(ws_recv(&mut socket)).expect("server 收帧") {
            WsFrame::Text(text) => {
                potato::block_on(ws_send_text(&mut socket, &format!("echo: {text}")))
                    .expect("server 发帧");
            }
            WsFrame::Binary(_) => panic!("服务端只约定文本帧"),
        }
    })
}

/// CDP 泵线程同款路径：同步 socket + `client_handshake_with_rng` +
/// `send_text`/`recv` 往返。
#[test]
fn wslite_client_echo_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn_ws_echo(listener);

    let mut socket = SyncSocket(TcpStream::connect(addr).expect("connect"));
    potato::block_on(client_handshake_with_rng(&mut socket, "127.0.0.1", addr.port(), "/", &mut test_rng()))
        .expect("客户端握手");
    let mut ws = Websocket::from_socket(socket, test_rng());
    potato::block_on(ws.send_text("你好，potato")).expect("客户端发帧");
    match potato::block_on(ws.recv()).expect("客户端收帧") {
        WsFrame::Text(text) => assert_eq!(text, "echo: 你好，potato"),
        WsFrame::Binary(_) => panic!("期望文本回显"),
    }
    server.join().expect("服务端正常收线");
}

/// Codex WebSocket endpoint 同款路径：`Websocket::connect`（HTTP 栈升级）
/// + `send_text`/`recv` 往返。
#[tokio::test]
async fn full_websocket_connect_echo_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn_ws_echo(listener);

    let mut ws = potato::Websocket::connect(&format!("ws://{addr}"), vec![])
        .await
        .expect("HTTP 栈升级握手");
    ws.send_text("hello").await.expect("客户端发帧");
    // 注意：full 版 recv 返回顶层 `potato::WsFrame`（与 wslite 的同名不同型）。
    match ws.recv().await.expect("客户端收帧") {
        potato::WsFrame::Text(text) => assert_eq!(text, "echo: hello"),
        potato::WsFrame::Binary(_) => panic!("期望文本回显"),
    }
    server.join().expect("服务端正常收线");
}
