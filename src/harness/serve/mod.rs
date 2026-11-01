//! H4 服务与协议（design.md §22.3/§22.4）。
//!
//! JSON-lines 请求-响应 + 事件订阅（风格对齐 app 的 input-hook stdout 协议：
//! UTF-8、每行一对象、`\n` 结尾、逐行 flush；宿主语言无关；一律管道/回环）。
//!
//! # 请求（stdin）
//!
//! ```text
//! {"id":1,"op":"version","args":{}}
//! {"id":2,"op":"get_control","args":{"x":100,"y":200}}
//! {"id":3,"op":"subscribe","args":{"kinds":["focus"]}}
//! {"id":4,"op":"askUser","args":{"questions":[...],"timeoutMs":120000}}
//! {"id":5,"op":"answer","args":{"id":"ask-1","answers":{...}}}
//! stop                                                       （温和退出）
//! ```
//!
//! # 响应 / 事件（stdout）
//!
//! ```text
//! {"id":1,"ok":true,"result":{...}}
//! {"id":2,"ok":false,"error":"..."}
//! {"id":3,"ok":false,"error":"...","code":"actuation-locked"}   code 仅平台/能力级错误携带（加法式）
//! {"event":"focus","data":{...}}          订阅事件行（生产者见 bin --poll-focus-ms）
//! {"id":"ask-1","op":"askUser","args":{...}}                    服务端请求行（反向提问，见下节）
//! ```
//!
//! # 反向提问（askUser/answer）
//!
//! 服务端可向宿主客户端发起反向提问——第三种 wire 形态「服务端请求行」。
//! 三条新行形态（`questions` 元素 = [`crate::protocol::UserQuestion`] 的
//! serde 形状原样嵌入，`multi_select` 为 snake_case）：
//!
//! ```text
//! // ① 宿主发起 askUser op：服务端登记 pending 后，原 id 的响应**延迟**
//! //    到 answer / 超时才写（timeoutMs 缺省 120000；source 可选）。
//! {"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"…","options":[{"label":"甲","description":"…"}],"multi_select":false}],"timeoutMs":120000}}
//! // ② 服务端请求行（stdout）：id 为服务端自增 "ask-N"（独立命名空间）。
//! {"id":"ask-1","op":"askUser","args":{"questions":[…],"timeoutMs":120000}}
//! // ③ 宿主应答 answer op：服务端先写 10 的最终响应行，再写 11 的 ack 行。
//! {"id":11,"op":"answer","args":{"id":"ask-1","answers":{"q1":["甲"]}}}
//! {"id":10,"ok":true,"result":{"answers":{"q1":["甲"]},"cancelled":false}}
//! {"id":11,"ok":true,"result":null}
//! ```
//!
//! 取消与超时：answer 行顶层加法式 `"cancelled":true`（缺省 false）→ 最终
//! 响应 `{"answers":{},"cancelled":true}`；timeoutMs 到点未答 → 原 id 回
//! ok:false（ask 超时），迟到 answer 落 unknown ask 错误、不给原 id 写
//! 第二次响应。pending 非空时主循环的阻塞等待时限取「最早 deadline」——
//! 反向等待绝不冻结主循环对其余 stdin 行/事件的消费；askUser/answer 均
//! 为内联轻量 op（不 spawn 工作线程、不受 30s op_timeout 影响），等待由
//! pending 表 + 等待预算承载。所有退出路径（Eof/stop/写失败）返回前清空
//! pending 表（drop 未写出的延迟响应——进程退出后无意义，仅需等待方不
//! 悬挂）；stop 退出另对每条未答 ask 补一行
//! `{"id":"ask-N","op":"askUser","args":{…},"cancelled":true}` 作废通知
//! （加法式可选键，best-effort）。`version` 结果加法式补 `"askUser":true`
//! 能力键（宿主据此决定是否实现 answer 路径；PROTOCOL_VERSION 不递增）。
//!
//! 生命周期：stdin EOF 或 `stop` 行退出（不留孤儿进程，宿主死亡同收）；
//! 退出时置位调用方传入的进程级关停标志（后台事件生产者轮询检查后收线，
//! P1-1）；stdout 连续写失败（客户端管道已断）同样退出。单行解析失败
//! 不影响后续请求（错误隔离；无效 UTF-8 行 lossy 降级走同一坏行路径，
//! P2-8）；探测不可用时感知 op 返回 `ok:false`（协议层 `ping`/`version`
//! 恒可用）。
//!
//! # 超时隔离（v1.14）
//!
//! 重 op 请求（含 [`crate::harness::tools::ToolRegistry::call`]，UIA 调用可能无限
//! 挂起）在一次性工作线程执行，serve 主循环限时等待结果：超时即回
//! `{"id":N,"ok":false,"error":"操作超时…"}` 并放弃该线程（不 join——泄漏
//! 一个挂起线程远好过整个 serve 冻结；线程结束后 send 到已丢弃的结果通道
//! 自然失败收线）。默认 30s，环境变量 `HARNESS_OP_TIMEOUT_MS`（>0 毫秒）
//! 可覆盖。请求-响应保持严格按序：主循环一次只处理一条消息，当前响应写完
//! 才取下一条（[`run_with_timeout`] 的 `timeout` 参数供测试注入短超时）。
//! 纯协议层轻量 op（ping/version/subscribe/unsubscribe/askUser/answer 与
//! 坏行）内联执行不 spawn 线程；工作线程 panic 经 `catch_unwind` 捕获后按
//! `op panicked`
//! 归因上报（与超时区分）；超时放弃的线程累计计数，每满 10 个经 tracing
//! warn 一次（P3-13：慢性病现场走结构化日志，stderr，不进协议通道）。
//!
//! # 背压与上限（P3-6）
//!
//! 消息通道为有界 [`mpsc::sync_channel`]（容量 [`CHANNEL_CAPACITY`]）：
//! stdin 请求走阻塞 send（背压传导到客户端写端，不丢请求），事件生产者
//! 走 `try_send`（慢客户端时通道满即丢弃该条事件——focus 事件可丢，绝不
//! 反向阻塞生产者线程）。stdin 单行超过 [`MAX_LINE_BYTES`] 时整行丢弃、
//! 回一行 ok:false 错误响应后继续（超限部分的剩余字节分块越过，不读入
//! 内存）。
//!
//! # 事件过滤（双保险）
//!
//! 事件行透传前按 [`ServeState::subscribed_kinds`] 过滤（bin 焦点轮询已自行
//! 过滤，此处兜底）：未订阅/无法解析 `event` 字段的事件静默丢弃。

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::harness::tools::ToolRegistry;
use crate::harness::Error;
use crate::protocol::UserQuestion;

/// 可订阅事件种类（`subscribe` args.kinds 白名单）：
/// - `focus` = 焦点控件观测（bin `--poll-focus-ms` 轮询生产者）；
/// - `scene` = 前台 Scene 快照（步骤 36 异常期用）——**无生产者占位**（Scene
///   走请求-响应 `capture_scene` op），保留白名单位不撤。
///
/// 事件帧 `{"event","data"}` 按订阅表**原样透传**（不感知 data 内部结构），
/// 新事件源只需生产 JSON 行。
///
/// **v1.15 边界（用户要求 U）**：`ui`（原生事件直录）/`browser`（CDP 直录）
/// 两个 kind 随录制/浏览器能力迁至 WorkRecorder 仓的 `wr-harness-ext`
/// crate，白名单不再收录。
pub const KNOWN_EVENT_KINDS: [&str; 2] = ["focus", "scene"];

/// serve 消息通道容量（P3-6a）：请求与事件共用的有界通道——stdin 请求走
/// 阻塞 send（背压到客户端写端，不丢失），事件生产者走 `try_send`（慢
/// 客户端时满即丢弃该条事件，focus 事件可丢）。慢/停客户端叠加高频事件
/// 生产时内存不再无界增长。
pub const CHANNEL_CAPACITY: usize = 256;

/// stdin 单行字节上限（P3-6b，含行尾）：超限行回一行 ok:false 错误响应、
/// 剩余字节分块丢弃（不整行读入内存），服务继续。
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// stdout 连续写失败达到该次数即退出主循环（P1-1：客户端管道已断，
/// 继续只会空转耗 CPU；偶发单次失败不生效）。
const WRITE_FAILURE_LIMIT: u32 = 3;

/// 超时放弃的工作线程每累计该数量 warn 一次（P3-4：UIA 疑似持续挂起的
/// 慢性病可见性；tracing 结构化日志，stderr）。
const ABANDONED_WARN_STEP: u64 = 10;

/// askUser 反向等待的缺省上限（毫秒）：宿主未带 `timeoutMs` 时的兜底，
/// 到点未收到 answer 即回 ask 超时并作废该提问。
pub const DEFAULT_ASK_TIMEOUT_MS: u64 = 120_000;

/// stdin 请求行。
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// 请求 id（响应原样回带；缺失按 null）。
    #[serde(default)]
    pub id: Value,
    pub op: String,
    #[serde(default)]
    pub args: Value,
    /// answer op 的用户取消语义（顶层加法式可选键，缺省 false；非 answer
    /// op 忽略）——与 stop 退出的作废通知行 `{"id":"ask-N",…,"cancelled":true}`
    /// 同一顶层键位，宿主两侧对接同一形状。
    #[serde(default)]
    pub cancelled: bool,
}

/// serve 共享状态：工具注册表 + 订阅表 + 未答 askUser pending 表。
pub struct ServeState {
    pub registry: ToolRegistry,
    subscriptions: Mutex<HashSet<String>>,
    /// 未答 askUser pending 表（ask id → 条目）：answer 命中移除、超时到期
    /// 移除。BTreeMap 保证同批到期/作废行按 ask id 稳定输出。
    asks: Mutex<BTreeMap<String, PendingAsk>>,
    /// 服务端 ask id 计数（`ask-1` 起，独立于宿主请求 id 命名空间）。
    next_ask_id: AtomicU64,
}

impl ServeState {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            subscriptions: Mutex::new(HashSet::new()),
            asks: Mutex::new(BTreeMap::new()),
            next_ask_id: AtomicU64::new(1),
        }
    }

    /// 当前订阅种类（观测用）。
    pub fn subscribed_kinds(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    /// 某事件种类当前是否被订阅（事件生产者过滤用：未订阅不生产，省
    /// 事件行构造与通道占用——focus 轮询线程同款过滤的通用化）。
    pub fn is_subscribed(&self, kind: &str) -> bool {
        self.lock().contains(kind)
    }

    /// 登记一条待答 ask：分配服务端 ask id（ask-N 自增）并记录 deadline
    /// （askUser 路径；返回 id 供服务端请求行回带）。
    fn register_ask(&self, original_request_id: Value, timeout_ms: u64, server_args: Value) -> String {
        let ask_id = format!("ask-{}", self.next_ask_id.fetch_add(1, Ordering::Relaxed));
        let pending = PendingAsk {
            original_request_id,
            timeout_ms,
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            server_args,
        };
        self.asks_lock().insert(ask_id.clone(), pending);
        ask_id
    }

    /// answer 命中路径：按 id 取走 pending（查无 → None → unknown ask）。
    fn take_ask(&self, ask_id: &str) -> Option<PendingAsk> {
        self.asks_lock().remove(ask_id)
    }

    /// 取走全部已到期 ask（调用方逐条回 ask 超时响应；迟到 answer 事后落
    /// unknown ask，不会给原 id 写第二次响应）。
    fn take_due_asks(&self) -> Vec<(Value, u64)> {
        let now = Instant::now();
        let mut asks = self.asks_lock();
        let due: Vec<String> = asks
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        due.into_iter()
            .filter_map(|id| asks.remove(&id))
            .map(|pending| (pending.original_request_id, pending.timeout_ms))
            .collect()
    }

    /// 距最早未答 ask deadline 的等待预算（None = 无 pending，无限期等待）：
    /// 主循环阻塞等待的时限取它与常规超时的较小者——反向等待到点醒来冲
    /// 超时，不冻结主循环对其余消息的消费。
    fn ask_wait_budget(&self) -> Option<Duration> {
        let now = Instant::now();
        self.asks_lock()
            .values()
            .map(|pending| pending.deadline.saturating_duration_since(now))
            .min()
    }

    /// 未答 ask 数（单测观测用；生产路径经 deadline 预算与到期冲刷消费）。
    #[cfg(test)]
    fn pending_ask_count(&self) -> usize {
        self.asks_lock().len()
    }

    /// stop 退出的作废通知行：对每条未答 ask 产出一行
    /// `{"id":"ask-N","op":"askUser","args":{…},"cancelled":true}`（加法式
    /// 可选键；取走即清表，调用方 best-effort 写出）。
    fn take_cancelled_ask_lines(&self) -> Vec<String> {
        let asks = std::mem::take(&mut *self.asks_lock());
        asks.into_iter()
            .map(|(id, pending)| {
                json!({
                    "id": id,
                    "op": "askUser",
                    "args": pending.server_args,
                    "cancelled": true,
                })
                .to_string()
            })
            .collect()
    }

    /// 退出冲刷：清空 pending 表（stop 路径已经由 [`Self::take_cancelled_ask_lines`]
    /// 清过，此处幂等兜底；drop 未写出的延迟响应，等待方不悬挂）。
    fn clear_asks(&self) {
        self.asks_lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn asks_lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, PendingAsk>> {
        self.asks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 单条待答 ask（服务端反向提问的 pending 表条目）。
#[derive(Debug, Clone)]
struct PendingAsk {
    /// 原 askUser 请求的宿主 id（延迟应答与超时响应回带）。
    original_request_id: Value,
    /// 登记时的 timeoutMs（ask 超时文案回带）。
    timeout_ms: u64,
    /// 反向等待截止时刻（主循环等待预算取各 ask 最早者）。
    deadline: Instant,
    /// 服务端请求行的 args 载荷（stop 作废通知行重建用）。
    server_args: Value,
}

/// 单行处理结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineOutcome {
    /// 空行/无输出（静默跳过）。
    None,
    /// 一行响应（逐行 flush）。
    Reply(String),
    /// askUser 已受理：先向客户端写携带的服务端请求行；原请求 id 的最终
    /// 响应延迟到 answer / 超时（Deferred 应答，主循环跳过立即应答）。
    Ask { server_line: String },
    /// answer 命中 pending：先写原 askUser 请求 id 的最终响应行，再写
    /// answer 自己的 ack 行（顺序契约：延迟应答先于 ack）。
    Answered { original_line: String, ack_line: String },
    /// `stop`：温和退出（不回响应）。
    Stop,
}

/// 处理一行请求（纯函数式入口；serve 循环与单测共用）。
pub fn handle_line(state: &ServeState, line: &str) -> LineOutcome {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LineOutcome::None;
    }
    if trimmed.eq_ignore_ascii_case("stop") {
        return LineOutcome::Stop;
    }
    let request = match serde_json::from_str::<Request>(trimmed) {
        Ok(request) => request,
        Err(e) => {
            return LineOutcome::Reply(reply(Value::Null, Err(Error::BadRequest(e))));
        }
    };
    let id = request.id.clone();
    match dispatch(state, request) {
        Dispatch::Now(outcome) => LineOutcome::Reply(reply(id, outcome)),
        Dispatch::Ask { server_line } => LineOutcome::Ask { server_line },
        Dispatch::Answered { original_request_id, result } => LineOutcome::Answered {
            original_line: reply(original_request_id, Ok(result)),
            ack_line: reply(id, Ok(Value::Null)),
        },
    }
}

/// 循环消息：stdin 请求行 / 订阅事件行 / 超限行（P3-6b）/ stdin EOF 信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeMessage {
    Request(String),
    Event(String),
    /// stdin 行超过 [`MAX_LINE_BYTES`]：读线程已丢弃整行，主循环回一行
    /// ok:false 错误响应（服务继续）。
    Oversize,
    /// stdin EOF（读线程发出）：主循环收到即退出。EOF 退出由此**显式化**，
    /// 不再依赖「通道所有 sender 释放」的循环闭合——事件汇等常驻 sender
    /// 持有者（宿主装配的事件生产者闭包）不再阻止退出；读线程 send 失败
    /// （通道已满/已关）时原「sender 全释放」路径仍是兜底（P1-1 的关停
    /// 标志机制不变）。
    Eof,
}

/// serve 主循环：消费 [`ServeMessage`] 流，响应/事件逐行写 `writer`，
/// 收到 `stop`、消息通道关闭（stdin EOF + 事件生产者全部退出）或 stdout
/// 连续写失败（[`WRITE_FAILURE_LIMIT`]）即返回。op 执行超时取
/// [`op_timeout`]（默认 30s，`HARNESS_OP_TIMEOUT_MS` 可覆盖）。pending
/// askUser 的反向等待由等待预算 + 到期冲刷承载，任何退出路径返回前清空
/// pending 表（stop 另补 cancelled 作废通知行，见模块文档「反向提问」）。
///
/// `tx`/`rx` 为同一有界通道（容量 [`CHANNEL_CAPACITY`]）的两端（调用方
/// 创建）：`tx` 交给 stdin 读线程与事件生产者（bin / 测试），`rx` 由本循环
/// 消费——单通道天然合并两个来源，无锁无轮询。`run` 返回后 `rx` 丢弃，
/// 残留读线程的 send 自然失败退出。`shutdown` 为进程级关停标志（P1-1）：
/// 本循环任何退出路径返回前置位——否则 EOF 退出依赖「通道所有 sender
/// 释放」的循环闭合，而 bin 焦点轮询线程手里的 sender 恰好破坏它
/// （`send` 失败才退出 ↔ 通道已关闭才 send 失败，互为前提永不满足）。
pub fn run<W: Write>(
    reader: impl BufRead + Send + 'static,
    writer: W,
    state: Arc<ServeState>,
    tx: mpsc::SyncSender<ServeMessage>,
    rx: mpsc::Receiver<ServeMessage>,
    shutdown: Arc<AtomicBool>,
) {
    run_with_timeout(reader, writer, state, tx, rx, op_timeout(), shutdown);
}

/// [`run`] 的显式超时形态（测试注入短超时用；生产入口走 [`run`]）。
///
/// 语义注记（请求-响应按序不变）：主循环一次只从通道取一条消息，当前
/// 响应写完后才取下一条——请求严格按到达序逐个执行，无并发 dispatch；
/// 超时隔离只是把重 op 的执行挪到工作线程并限时等待。pending askUser
/// 非空时阻塞等待的时限取「最早 ask deadline 与常规超时的较小者」，到点
/// 先冲已到期 ask 再继续——反向等待绝不冻结主循环（见模块文档
/// 「反向提问」）。
pub fn run_with_timeout<W: Write>(
    reader: impl BufRead + Send + 'static,
    mut writer: W,
    state: Arc<ServeState>,
    tx: mpsc::SyncSender<ServeMessage>,
    rx: mpsc::Receiver<ServeMessage>,
    timeout: Duration,
    shutdown: Arc<AtomicBool>,
) {
    spawn_reader(reader, tx);
    let mut consecutive_write_failures: u32 = 0;
    let mut abandoned_workers: u64 = 0;
    'serve: loop {
        // 到期冲刷：任何阻塞等待前先把已过 deadline 的 ask 逐条回 ask 超时
        // 响应（写失败共用同一连续计数路径）。
        if flush_expired_asks(&state, &mut writer, &mut consecutive_write_failures) {
            break 'serve;
        }
        // pending ask 非空时阻塞等待限流为最早 deadline 的预算：到点醒来
        // 冲超时，其余 stdin 行/事件照常消费；无 pending 退回无限期阻塞
        // （语义与既往一致）。
        let message = match state.ask_wait_budget() {
            None => match rx.recv() {
                Ok(message) => message,
                Err(_) => break 'serve,
            },
            Some(budget) => match rx.recv_timeout(budget) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue 'serve,
                Err(mpsc::RecvTimeoutError::Disconnected) => break 'serve,
            },
        };
        let mut stop = false;
        let lines: Vec<String> = match message {
            ServeMessage::Request(line) => {
                // 轻量 op（纯协议层 ping/version/subscribe/unsubscribe/
                // askUser/answer）与坏行不可能挂起：内联执行不 spawn 线程
                // （P3-4，高频 ping 不再一线程一请求；askUser/answer 的反向
                // 等待由 pending 表 + 等待预算承载，不占线程）；其余走超时
                // 隔离。
                let lightweight = serde_json::from_str::<Request>(line.trim())
                    .map(|r| {
                        matches!(
                            r.op.as_str(),
                            "ping" | "version" | "subscribe" | "unsubscribe" | "askUser" | "answer"
                        )
                    })
                    .unwrap_or(true);
                if lightweight {
                    let outcome = handle_line(&state, &line);
                    outcome_lines(outcome, &mut stop)
                } else {
                    // 超时隔离：handle_line（含 registry.call——UIA 调用可能无限
                    // 挂起）放一次性工作线程执行，主循环限时等待结果。超时即回
                    // ok:false 并放弃该线程：不 join（挂起线程无法中断，join =
                    // 冻结主循环，恰是要防的事故），泄漏一个阻塞线程可接受——
                    // 线程结束后向已丢弃的 result_tx send 自然失败收线。
                    let worker_state = Arc::clone(&state);
                    let worker_line = line.clone();
                    let (result_tx, result_rx) = mpsc::channel();
                    std::thread::spawn(move || {
                        // panic 经 catch_unwind 捕获后走结果通道归因上报，
                        // 不再被 recv_timeout 误报成「操作超时」（P3-5）。
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                handle_line(&worker_state, &worker_line)
                            }))
                            .map(WorkerOutcome::Done)
                            .unwrap_or_else(|payload| {
                                WorkerOutcome::Panicked(panic_message(&payload))
                            });
                        let _ = result_tx.send(outcome);
                    });
                    // 等待预算 = min(op 超时预算, 最早 ask deadline)：到点先冲
                    // 到期 ask（ask 超时响应与 op 超时响应回带不同 id，互不
                    // 干扰），op 预算同时耗尽才放弃工作线程。
                    let started = Instant::now();
                    let outcome = 'worker_wait: loop {
                        let op_left = timeout.saturating_sub(started.elapsed());
                        let budget = match state.ask_wait_budget() {
                            Some(ask_left) => op_left.min(ask_left),
                            None => op_left,
                        };
                        match result_rx.recv_timeout(budget) {
                            Ok(WorkerOutcome::Done(outcome)) => break 'worker_wait outcome,
                            Ok(WorkerOutcome::Panicked(reason)) => {
                                // panic 归因 error 级（P3-5/P3-13）：wire 上另有
                                // ok:false 响应，此处是 stderr 侧的结构化证据
                                // （op + panic 载荷，不拼纯文本消息）。
                                tracing::error!(
                                    op = %request_op_of(&line),
                                    panic = %reason,
                                    "重 op 工作线程 panic"
                                );
                                break 'worker_wait LineOutcome::Reply(reply(
                                    request_id_of(&line),
                                    Err(Error::OpPanicked(reason)),
                                ));
                            }
                            Err(e) => {
                                // 到点先冲到期 ask；result_rx 在超时放弃后
                                // 丢弃——迟到的工作线程 send 失败，响应不留痕。
                                // Disconnected（工作线程异常消失）与预算耗尽
                                // 同走放弃路径。
                                if flush_expired_asks(
                                    &state,
                                    &mut writer,
                                    &mut consecutive_write_failures,
                                ) {
                                    break 'serve;
                                }
                                let exhausted = matches!(e, mpsc::RecvTimeoutError::Disconnected)
                                    || started.elapsed() >= timeout;
                                if !exhausted {
                                    continue 'worker_wait;
                                }
                                abandoned_workers += 1;
                                if abandoned_workers.is_multiple_of(ABANDONED_WARN_STEP) {
                                    // 超时线程累积（P3-4 慢性病）：warn 级结构化
                                    // 字段带累计数/op/超时上限，去 stderr。
                                    tracing::warn!(
                                        abandoned = abandoned_workers,
                                        op = %request_op_of(&line),
                                        timeout_ms = timeout.as_millis() as u64,
                                        "超时工作线程累计放弃（op 疑似持续挂起）"
                                    );
                                }
                                break 'worker_wait LineOutcome::Reply(timeout_reply(&line, timeout));
                            }
                        }
                    };
                    outcome_lines(outcome, &mut stop)
                }
            }
            ServeMessage::Event(line) => {
                // 双保险过滤：bin 焦点轮询已按订阅过滤，此处按订阅表兜底——
                // 未订阅/解析不出 event 种类的事件静默丢弃（空行集）。
                let subscribed = event_kind_of(&line)
                    .is_some_and(|kind| state.subscribed_kinds().iter().any(|k| k == &kind));
                if subscribed {
                    vec![line]
                } else {
                    Vec::new()
                }
            }
            ServeMessage::Oversize => vec![reply(
                Value::Null,
                Err(Error::OversizeLine { limit: MAX_LINE_BYTES }),
            )],
            // stdin EOF（读线程显式信号）：立即退出（常驻事件 sender 不再
            // 阻止退出，见 [`ServeMessage::Eof`] 文档）。
            ServeMessage::Eof => break 'serve,
        };
        for line in &lines {
            if write_response_line(&mut writer, line, &mut consecutive_write_failures) {
                break 'serve;
            }
        }
        if stop {
            // stop 退出的作废通知：客户端仍在读——对每条未答 ask 补一行
            // cancelled:true（顶层加法式可选键；best-effort，写失败不影响
            // 退出），取走即清表。
            for line in state.take_cancelled_ask_lines() {
                let _ = writeln!(writer, "{line}");
            }
            let _ = writer.flush();
            break 'serve;
        }
    }
    // 退出冲刷：清空 pending ask 表（stop 路径已在上面清过，此处幂等兜底；
    // 未写出的延迟响应随退出 drop——进程退出后无意义，仅需等待方不悬挂、
    // 无 panic，对齐 codex 适配器 close 空答案冲刷先例）。
    state.clear_asks();
    // 任何退出路径（stop/通道关闭/写失败关停）统一置位关停标志：后台事件
    // 生产者（bin 焦点轮询）每轮检查后收线（P1-1）。
    shutdown.store(true, Ordering::SeqCst);
}

/// 写一行响应（逐行 flush；连续写失败计数，达到 [`WRITE_FAILURE_LIMIT`]
/// 返回 true 表示应退出主循环——P1-1：客户端管道已断，防「收事件 →
/// 写失败 → 继续」的空转循环。写失败现场经 tracing warn 留痕，P3-13）。
fn write_response_line<W: Write>(
    writer: &mut W,
    line: &str,
    consecutive_write_failures: &mut u32,
) -> bool {
    if writeln!(writer, "{line}").is_ok() {
        let _ = writer.flush();
        *consecutive_write_failures = 0;
        return false;
    }
    *consecutive_write_failures += 1;
    tracing::warn!(
        consecutive = *consecutive_write_failures,
        limit = WRITE_FAILURE_LIMIT,
        line_bytes = line.len(),
        "stdout 响应写失败（客户端管道疑似已断）"
    );
    *consecutive_write_failures >= WRITE_FAILURE_LIMIT
}

/// 把已到期的 ask 逐条回 ask 超时响应（id 回带原 askUser 请求 id）；返回
/// true 表示写失败达到上限应退出。主循环每次阻塞等待前与工作线程等待
/// 到点时调用。
fn flush_expired_asks<W: Write>(
    state: &ServeState,
    writer: &mut W,
    consecutive_write_failures: &mut u32,
) -> bool {
    for (request_id, limit_ms) in state.take_due_asks() {
        let line = reply(request_id, Err(Error::AskTimedOut { limit_ms }));
        if write_response_line(writer, &line, consecutive_write_failures) {
            return true;
        }
    }
    false
}

/// [`LineOutcome`] → 待写行序（[`LineOutcome::Answered`] 保持「原 askUser id
/// 终响应在前、answer ack 在后」顺序；Stop 经 `stop` 标志带出）。
fn outcome_lines(outcome: LineOutcome, stop: &mut bool) -> Vec<String> {
    match outcome {
        LineOutcome::None => Vec::new(),
        LineOutcome::Reply(line) | LineOutcome::Ask { server_line: line } => vec![line],
        LineOutcome::Answered { original_line, ack_line } => vec![original_line, ack_line],
        LineOutcome::Stop => {
            *stop = true;
            Vec::new()
        }
    }
}

/// 单 op 执行超时（serve 进程启动时读取一次）：默认 30s；环境变量
/// `HARNESS_OP_TIMEOUT_MS`（>0 的整数毫秒）可覆盖，非法/0 值回退默认。
pub fn op_timeout() -> Duration {
    const DEFAULT_MS: u64 = 30_000;
    let ms = std::env::var("HARNESS_OP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_MS);
    Duration::from_millis(ms)
}

/// 超时响应行（id 尽量从原请求行解析，失败按 null；错误文案经
/// [`Error::OpTimeout`] 的 Display 产出——与原 format! 逐字节等价）。
fn timeout_reply(request_line: &str, timeout: Duration) -> String {
    let id = request_id_of(request_line);
    json!({
        "id": id,
        "ok": false,
        "error": Error::OpTimeout { limit_ms: timeout.as_millis() }.to_string(),
    })
    .to_string()
}

/// 重 op 工作线程结果：正常完成 / panic（归因区分，P3-5）。
enum WorkerOutcome {
    Done(LineOutcome),
    Panicked(String),
}

/// panic 载荷转字符串（&str/String 之外的载荷给占位说明，尽力带出）。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<非字符串 panic 载荷>".to_string()
    }
}

/// 从原请求行尽量解析 id（解析失败按 null；超时/panic 响应回带）。
fn request_id_of(request_line: &str) -> Value {
    serde_json::from_str::<Request>(request_line.trim())
        .map(|r| r.id)
        .unwrap_or(Value::Null)
}

/// 从原请求行尽量解析 op 名（慢性病日志的结构化字段；解析失败按空串。
/// 只在 warn/error 路径调用，解析开销可忽略）。
fn request_op_of(request_line: &str) -> String {
    serde_json::from_str::<Request>(request_line.trim())
        .map(|r| r.op)
        .unwrap_or_default()
}

/// 从事件行解析事件种类（`{"event":"...","data":...}`；解析失败 → None）。
fn event_kind_of(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("event")?.as_str().map(str::to_string)
}

/// 桥接阻塞 reader → 消息通道（EOF/读失败即结束线程；rx 丢弃后 send 失败
/// 同样收线，不留孤儿）。无效 UTF-8 行 lossy 降级（P2-8：坏一行字节走
/// bad-request 响应路径，不再静默终止整个服务）；单行超 [`MAX_LINE_BYTES`]
/// 时丢弃整行并发 [`ServeMessage::Oversize`]（P3-6b）。
fn spawn_reader(mut reader: impl BufRead + Send + 'static, tx: mpsc::SyncSender<ServeMessage>) {
    std::thread::spawn(move || {
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match read_limited_line(&mut reader, &mut bytes) {
                Ok(ReadLine::Line(line)) => {
                    if tx.send(ServeMessage::Request(line)).is_err() {
                        break;
                    }
                }
                Ok(ReadLine::Oversize) => {
                    if tx.send(ServeMessage::Oversize).is_err() {
                        break;
                    }
                }
                Ok(ReadLine::Eof) => {
                    // EOF 显式信号（读失败不发声——错误路径由「sender 全释放」
                    // 兜底退出）；send 失败（通道满/关）不重试：满时主循环
                    // 迟早排空后 recv 挂等，此时原循环闭合路径接管。
                    let _ = tx.send(ServeMessage::Eof);
                    break;
                }
                Err(_) => break,
             }
        }
    });
}

/// [`read_limited_line`] 的结果。
enum ReadLine {
    /// EOF（无残余数据）。
    Eof,
    /// 一行完整读入（已去行尾、lossy 降级）。
    Line(String),
    /// 行超过 [`MAX_LINE_BYTES`]（整行已丢弃，含越过行尾的剩余字节）。
    Oversize,
}

/// 读一行（fill_buf/consume 逐块积累）：行内容超过 [`MAX_LINE_BYTES`]
/// 时不再整行读入内存，剩余字节分块越过直到行尾/EOF（P3-6b）。
fn read_limited_line(reader: &mut impl BufRead, bytes: &mut Vec<u8>) -> std::io::Result<ReadLine> {
    bytes.clear();
    let mut oversize = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            // EOF：超限残余按超限处理，否则无换行的末行也按行处理。
            return Ok(if oversize {
                ReadLine::Oversize
            } else if bytes.is_empty() {
                ReadLine::Eof
            } else {
                ReadLine::Line(decode_line(bytes))
            });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                // 含行尾的总长超限 → 整行（含已积累部分）丢弃。
                if oversize || bytes.len() + pos + 1 > MAX_LINE_BYTES {
                    reader.consume(pos + 1);
                    return Ok(ReadLine::Oversize);
                }
                bytes.extend_from_slice(&available[..=pos]);
                reader.consume(pos + 1);
                return Ok(ReadLine::Line(decode_line(bytes)));
            }
            None => {
                let n = available.len();
                if oversize || bytes.len() + n > MAX_LINE_BYTES {
                    // 已积累字节作废，本块起只丢不收。
                    oversize = true;
                } else {
                    bytes.extend_from_slice(available);
                }
                reader.consume(n);
            }
        }
    }
}

/// 行字节 → 字符串：去 `\n`/`\r` 行尾 + 无效 UTF-8 lossy 降级（P2-8）。
fn decode_line(bytes: &mut Vec<u8>) -> String {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// dispatch 结果：单行处理的三形态出口（handle_line 转 [`LineOutcome`]）。
#[derive(Debug)]
enum Dispatch {
    /// 常规 op：立即以 [`reply`] 回一行。
    Now(Result<Value, Error>),
    /// askUser：已登记 pending，先向客户端写服务端请求行；原请求 id 的
    /// 最终响应延迟到 answer / 超时（Deferred 应答，主循环跳过立即应答）。
    Ask { server_line: String },
    /// answer 命中 pending：先写原 askUser 请求 id 的最终响应行，再写
    /// answer 自己的 ack 行（顺序契约）。
    Answered { original_request_id: Value, result: Value },
}

/// 分发一个请求（订阅登记 / 反向提问 / 工具调用）。
fn dispatch(state: &ServeState, request: Request) -> Dispatch {
    match request.op.as_str() {
        "subscribe" => Dispatch::Now(subscribe(state, &request.args, true)),
        "unsubscribe" => Dispatch::Now(subscribe(state, &request.args, false)),
        "askUser" => ask_user(state, &request),
        "answer" => answer(state, &request),
        // 仅 crate 内单测可触达的延迟假 op（模拟 UIA 挂起，验证超时隔离）；
        // 不进工具清单/协议文档，真 bin 进程里编译期即不存在。
        #[cfg(test)]
        "_test_sleep" => {
            let ms = request.args.get("ms").and_then(Value::as_u64).unwrap_or(0);
            std::thread::sleep(Duration::from_millis(ms));
            Dispatch::Now(Ok(json!({ "slept": ms })))
        }
        // 仅 crate 内单测可触达的 panic 假 op（验证 panic 归因与超时区分，
        // P3-5）；同样不进工具清单/协议文档。
        #[cfg(test)]
        "_test_panic" => panic!("测试 panic（_test_panic）"),
        op => Dispatch::Now(state.registry.call(op, &request.args)),
    }
}

/// askUser（宿主发起的反向提问）：questions 复用 [`UserQuestion`] 的 serde
/// 形状原样嵌入服务端请求行（serve wire 与归一化类型同构，snake_case，不
/// 另造形状）；登记 pending 后返回 [`Dispatch::Ask`]——原请求 id 的响应
/// 延迟到 answer / 超时（Deferred 应答）。timeoutMs 缺省
/// [`DEFAULT_ASK_TIMEOUT_MS`]；source 可选，提供才随行下发。
fn ask_user(state: &ServeState, request: &Request) -> Dispatch {
    let questions: Vec<UserQuestion> = match request.args.get("questions") {
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(questions) => questions,
            Err(e) => return Dispatch::Now(Err(Error::BadRequest(e))),
        },
        None => return Dispatch::Now(Err(Error::AskQuestionsMissing)),
    };
    let timeout_ms = request
        .args
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_ASK_TIMEOUT_MS);
    let mut server_args = json!({ "questions": questions, "timeoutMs": timeout_ms });
    if let Some(source) = request.args.get("source").and_then(Value::as_str) {
        server_args["source"] = json!(source);
    }
    let ask_id = state.register_ask(request.id.clone(), timeout_ms, server_args.clone());
    Dispatch::Ask {
        server_line: json!({ "id": ask_id, "op": "askUser", "args": server_args }).to_string(),
    }
}

/// answer（宿主应答）：按 args.id 取走 pending，产出两条线——先原 askUser
/// 请求 id 的最终响应（answers 原样透传 + cancelled 语义；顶层
/// `"cancelled":true` 时 answers 按空表作废），后 answer 自己的 ack
/// （result:null，回显宿主任取的 answer id）。查无该 id → unknown ask 错误
/// （已超时/已应答/从未登记；不给原 id 写第二次响应）。
fn answer(state: &ServeState, request: &Request) -> Dispatch {
    let Some(ask_id) = request.args.get("id").and_then(Value::as_str) else {
        return Dispatch::Now(Err(Error::AnswerIdMissing));
    };
    let Some(pending) = state.take_ask(ask_id) else {
        return Dispatch::Now(Err(Error::UnknownAsk { id: ask_id.to_string() }));
    };
    let result = if request.cancelled {
        json!({ "answers": {}, "cancelled": true })
    } else {
        let answers = request.args.get("answers").cloned().unwrap_or_else(|| json!({}));
        json!({ "answers": answers, "cancelled": false })
    };
    Dispatch::Answered { original_request_id: pending.original_request_id, result }
}

/// 订阅/退订（kinds 白名单校验；ack 回当前订阅清单）。
fn subscribe(state: &ServeState, args: &Value, add: bool) -> Result<Value, Error> {
    let kinds = args
        .get("kinds")
        .and_then(Value::as_array)
        .ok_or(Error::SubscribeKindsMissing)?;
    let mut changed = Vec::new();
    for kind in kinds {
        let kind = kind.as_str().ok_or(Error::SubscribeKindNotString)?;
        if !KNOWN_EVENT_KINDS.contains(&kind) {
            return Err(Error::UnknownEventKind { kind: kind.to_string() });
        }
        let mut subs = state.lock();
        if add {
            subs.insert(kind.to_string());
        } else {
            subs.remove(kind);
        }
        changed.push(kind.to_string());
    }
    let key = if add { "subscribed" } else { "unsubscribed" };
    Ok(json!({ key: changed, "active": state.subscribed_kinds() }))
}

/// 响应行封装（错误经 [`Error`] 的 Display 写入 wire——与原字符串形态
/// 逐字节等价，P3-13 枚举化不改协议）。携带 code 的错误（平台/能力级，
/// [`Error::code`]）加法式补 `"code"` 键：仅出现在 `ok:false` 行且有值时
/// ——`ok:true` 行与无 code 错误的形态与既往逐字节一致（旧客户端不受影响）。
fn reply(id: Value, outcome: Result<Value, Error>) -> String {
    match outcome {
        Ok(result) => json!({ "id": id, "ok": true, "result": result }).to_string(),
        Err(error) => {
            let mut reply = json!({ "id": id, "ok": false, "error": error.to_string() });
            if let Some(code) = error.code() {
                reply["code"] = json!(code);
            }
            reply.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::snapshot::synth::SyntheticProbe;
    use crate::harness::snapshot::testutil::{sample_tree, sample_windows};
    use crate::harness::tools::ToolRegistry;
    use std::io::Cursor;

    fn state() -> Arc<ServeState> {
        Arc::new(ServeState::new(ToolRegistry::new(Arc::new(
            SyntheticProbe::new(sample_tree(), sample_windows()),
        ))))
    }

    fn state_without_probe() -> Arc<ServeState> {
        Arc::new(ServeState::new(ToolRegistry::without_probe()))
    }

    #[test]
    fn handle_line_routes_ops_and_echoes_id() {
        let state = state();
        // version：协议三元组 + id 回带。
        let out = handle_line(&state, r#"{"id":1,"op":"version"}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], json!(1));
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["result"]["protocol"], json!(crate::harness::PROTOCOL_VERSION));

        // get_control：合成树命中。
        let out = handle_line(&state, r#"{"id":"a","op":"get_control","args":{"x":15,"y":15}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["control"]["name"], json!("保存"));

        // 未知 op：ok:false + 错误文案。
        let out = handle_line(&state, r#"{"id":9,"op":"nope"}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap().contains("unknown op: nope"));
    }

    #[test]
    fn handle_line_error_isolation_and_control_lines() {
        let state = state();
        // 非法 JSON：id 缺失按 null，错误不扩散到下一行。
        let out = handle_line(&state, "{oops");
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], json!(null));
        assert_eq!(value["ok"], json!(false));
        // 下一行照常处理。
        assert!(matches!(handle_line(&state, r#"{"id":2,"op":"ping"}"#), LineOutcome::Reply(_)));
        // 空行静默；stop 行退出信号。
        assert_eq!(handle_line(&state, "   "), LineOutcome::None);
        assert_eq!(handle_line(&state, "STOP"), LineOutcome::Stop);
    }

    #[test]
    fn subscribe_whitelists_kinds_and_unsubscribes() {
        let state = state();
        let out = handle_line(&state, r#"{"id":1,"op":"subscribe","args":{"kinds":["focus"]}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["active"], json!(["focus"]));
        assert_eq!(state.subscribed_kinds(), vec!["focus"]);
        // 未知 kind 拒绝（错误文案可用清单跟随 KNOWN_EVENT_KINDS）；
        // 退订生效。
        assert!(handle_line(&state, r#"{"id":2,"op":"subscribe","args":{"kinds":["mouse"]}}"#)
            .as_reply_error());
        let out = handle_line(&state, r#"{"id":3,"op":"unsubscribe","args":{"kinds":["focus"]}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["active"], json!([]));
    }

    /// 错误响应的 `code` 键形态（upgrade.md §10.3 加法式）：平台/能力级
    /// 错误（如授权门）补 `code`；普通错误与 ok:true 行**不出现** `code`
    /// （旧客户端形态不受影响）。
    #[test]
    fn error_reply_carries_code_only_for_coded_errors() {
        let state = state();
        // ActuationLocked 带 code：未授权进程调 click → ok:false + code。
        let out = handle_line(&state, r#"{"id":1,"op":"click","args":{"x":1.0,"y":2.0}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["code"], json!("actuation-locked"), "{value}");

        // 无 code 错误（未知 op / 参数校验）：不出现 code 键。
        let out = handle_line(&state, r#"{"id":2,"op":"nope"}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert!(value.as_object().unwrap().get("code").is_none(), "未知 op 无机器码: {value}");

        // ok:true 行恒无 code 键。
        let out = handle_line(&state, r#"{"id":3,"op":"ping"}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(true));
        assert!(value.as_object().unwrap().get("code").is_none(), "ok:true 不带 code: {value}");
    }

    #[test]
    fn sensing_op_fails_closed_without_probe_but_ping_works() {
        let state = state_without_probe();
        let out = handle_line(&state, r#"{"id":1,"op":"list_windows"}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap().contains("探测不可用"));
        assert!(matches!(handle_line(&state, r#"{"id":2,"op":"ping"}"#), LineOutcome::Reply(_)));
    }

    /// `LineOutcome::Reply` 上的便捷断言（错误响应包含 ok:false）。
    trait ReplyExt {
        fn as_reply_error(&self) -> bool;
    }
    impl ReplyExt for LineOutcome {
        fn as_reply_error(&self) -> bool {
            match self {
                LineOutcome::Reply(line) => {
                    serde_json::from_str::<Value>(line).map(|v| v["ok"] == json!(false)).unwrap_or(false)
                }
                _ => false,
            }
        }
    }

    #[test]
    fn run_loopback_request_response_event_and_stop() {
        let state = state();
        // 预登记 focus 订阅（subscribe/unsubscribe 语义已有专项单测）——事件
        // 过滤按该表执行：focus 透传，未订阅种类/解析失败事件丢弃。
        let _ = handle_line(&state, r#"{"op":"subscribe","args":{"kinds":["focus"]}}"#);
        let input = concat!(
            r#"{"id":1,"op":"ping"}"#, "\n",
            r#"{"id":3,"op":"get_control","args":{"x":20,"y":110}}"#, "\n",
            "stop", "\n",
            r#"{"id":4,"op":"ping"}"#, "\n", // stop 之后不再处理。
        );
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        // 事件先于 reader 请求入队（通道 FIFO）：focus 已订阅透传；
        // mouse 未订阅（白名单也永不允许）、坏行解析失败 → 双保险过滤丢弃。
        tx.send(ServeMessage::Event(
            r#"{"event":"focus","data":{"control":{"controlType":"Edit"}}}"#.into(),
        ))
        .unwrap();
        tx.send(ServeMessage::Event(r#"{"event":"mouse","data":{"x":1}}"#.into())).unwrap();
        tx.send(ServeMessage::Event("not-json".into())).unwrap();
        let mut out: Vec<u8> = Vec::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        run_with_timeout(
            Cursor::new(input),
            &mut out,
            state,
            tx,
            rx,
            Duration::from_secs(30),
            shutdown,
        );
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "未订阅事件丢弃 + stop 后不再响应: {text}");
        // 已订阅事件行原样透出。
        assert!(lines[0].starts_with(r#"{"event":"focus""#), "{}", lines[0]);
        let ping: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ping["id"], json!(1));
        assert_eq!(ping["ok"], json!(true));
        let third: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(third["result"]["control"]["controlType"], json!("Edit"));
    }

    /// 超时隔离：挂起 op 不冻结 serve——限时回 ok:false，后续请求照常按序
    /// 执行（挂起工作线程不 join，泄漏一个阻塞线程可接受）。
    #[test]
    fn run_with_timeout_isolates_hung_op_and_keeps_sequential_semantics() {
        let state = state();
        let input = concat!(
            r#"{"id":1,"op":"_test_sleep","args":{"ms":10000}}"#, "\n",
            r#"{"id":2,"op":"ping"}"#, "\n",
            "stop", "\n",
        );
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let mut out: Vec<u8> = Vec::new();
        let started = std::time::Instant::now();
        let shutdown = Arc::new(AtomicBool::new(false));
        run_with_timeout(
            Cursor::new(input),
            &mut out,
            state,
            tx,
            rx,
            Duration::from_millis(300),
            shutdown,
        );
        let elapsed = started.elapsed();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "慢 op 超时回一行 + ping 回一行，stop 不回: {text}");
        // 慢 op：超时响应 ok:false（id 回带）。
        let timed_out: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(timed_out["id"], json!(1));
        assert_eq!(timed_out["ok"], json!(false));
        assert!(timed_out["error"].as_str().unwrap().contains("操作超时"), "{timed_out}");
        // 后续 ping 照常应答（顺序语义：第二条在超时响应之后处理）。
        let ping: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ping["id"], json!(2));
        assert_eq!(ping["ok"], json!(true));
        assert_eq!(ping["result"]["pong"], json!(true));
        // 10s 假挂起没有冻结主循环（300ms 超时 + 少量调度余量）。
        assert!(elapsed < Duration::from_secs(5), "超时隔离失效: {elapsed:?}");
    }

    /// 超时响应行：id 从原请求行尽量解析；解析失败按 null。
    #[test]
    fn timeout_reply_echoes_request_id_or_null() {
        let line = timeout_reply(r#"{"id":42,"op":"dump_tree"}"#, Duration::from_millis(250));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], json!(42));
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap().contains("250"), "{value}");

        let line = timeout_reply("{broken", Duration::from_millis(1));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], json!(null));
        assert_eq!(value["ok"], json!(false));
    }

    /// serve 辅助：跑一轮输入，返回全部输出行（sync_channel + 关停标志
    /// 由本测试模块统一装配）。
    fn serve_roundtrip(state: Arc<ServeState>, input: Vec<u8>, timeout: Duration) -> Vec<String> {
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let mut out: Vec<u8> = Vec::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        run_with_timeout(Cursor::new(input), &mut out, state, tx, rx, timeout, shutdown);
        String::from_utf8(out).unwrap().lines().map(str::to_string).collect()
    }

    /// 无效 UTF-8 行（P2-8）：lossy 降级走 bad-request 响应路径，服务
    /// 不终止，后续请求照常应答。
    #[test]
    fn run_survives_invalid_utf8_line() {
        let state = state();
        // 坏字节放在 JSON 结构外（lossy 替换后必为非法 JSON → bad request）；
        // 用字节 Vec 拼接（&str 字面量不容纳非法 UTF-8）。
        let mut input: Vec<u8> = br#"{"id":1,"#.to_vec();
        input.extend_from_slice(&[0xff, 0xfe]); // 行内坏字节
        input.extend_from_slice(br#""op":"ping"}"#);
        input.push(b'\n');
        input.extend_from_slice(&[0xfd, 0xfc, b'\n']); // 整行坏字节
        input.extend_from_slice(br#"{"id":2,"op":"ping"}"#);
        input.push(b'\n');
        let lines = serve_roundtrip(state, input, Duration::from_secs(30));
        assert_eq!(lines.len(), 3, "两行坏字节各回 bad request + ping 应答: {lines:?}");
        for (i, line) in lines[..2].iter().enumerate() {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["id"], json!(null), "坏行 id 按 null: {i}");
            assert_eq!(value["ok"], json!(false));
            assert!(value["error"].as_str().unwrap().contains("bad request"), "{value}");
        }
        let ping: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(ping["id"], json!(2));
        assert_eq!(ping["ok"], json!(true), "坏字节行不再终止服务（P2-8）");
    }

    /// 超长行（P3-6b）：超过 1 MiB 上限的行回 ok:false 错误响应、整行丢弃，
    /// 服务继续。
    #[test]
    fn run_rejects_oversize_line_and_continues() {
        let state = state();
        // 1 MiB + 4 KiB 的非法 JSON 行（超限即丢弃，内容无所谓）。
        let mut input = format!(r#"{{"id":1,"op":"ping","pad":""{}"#, "x".repeat(MAX_LINE_BYTES + 4096))
            .into_bytes();
        input.push(b'\n');
        input.extend_from_slice(br#"{"id":2,"op":"ping"}"#);
        input.push(b'\n');
        let lines = serve_roundtrip(state, input, Duration::from_secs(30));
        assert_eq!(lines.len(), 2, "超限行回一行错误 + ping 应答: {} 行", lines.len());
        let oversize: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(oversize["ok"], json!(false));
        let error = oversize["error"].as_str().unwrap();
        assert!(error.contains("上限") || error.contains("字节"), "{oversize}");
        let ping: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(ping["id"], json!(2));
        assert_eq!(ping["ok"], json!(true), "超限行后服务继续（P3-6b）");
    }

    /// 工作线程 panic（P3-5）：归因为 `op panicked` 而非「操作超时」，
    /// id 回带，后续请求照常。
    #[test]
    fn run_reports_panicked_op_distinctly_from_timeout() {
        let state = state();
        let input = concat!(
            r#"{"id":1,"op":"_test_panic"}"#, "\n",
            r#"{"id":2,"op":"ping"}"#, "\n",
            "stop", "\n",
        );
        let lines = serve_roundtrip(state, input.as_bytes().to_vec(), Duration::from_secs(30));
        assert_eq!(lines.len(), 2, "panic 响应 + ping 应答: {lines:?}");
        let panicked: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(panicked["id"], json!(1), "panic 响应 id 回带");
        assert_eq!(panicked["ok"], json!(false));
        let error = panicked["error"].as_str().unwrap();
        assert!(error.contains("op panicked"), "panic 归因明确: {error}");
        assert!(!error.contains("操作超时"), "panic 不得误报为超时: {error}");
        // 注：消息文本按 best-effort 带出——&str/String 载荷给原文，其余
        // （新工具链的私有载荷形态）给占位说明，此处不锁内容。
        let ping: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(ping["ok"], json!(true), "panic 不扩散到后续请求");
    }

    /// run 返回后置位关停标志（P1-1 库层）：后台事件生产者据此收线。
    #[test]
    fn run_sets_shutdown_flag_on_return() {
        let state = state();
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let input = concat!(r#"{"id":1,"op":"ping"}"#, "\n", "stop", "\n");
        run_with_timeout(
            Cursor::new(input.as_bytes().to_vec()),
            Vec::new(),
            state,
            tx,
            rx,
            Duration::from_secs(30),
            Arc::clone(&shutdown),
        );
        assert!(shutdown.load(Ordering::SeqCst), "run 返回后关停标志必须置位（P1-1）");
    }

    /// stdout 连续写失败（P1-1）：客户端管道已断 → 连续 WRITE_FAILURE_LIMIT
    /// 次后主循环退出并置位关停，不再空转。
    #[test]
    fn run_exits_after_consecutive_write_failures() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "管道已断"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let state = state();
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let input = concat!(
            r#"{"id":1,"op":"ping"}"#, "\n",
            r#"{"id":2,"op":"ping"}"#, "\n",
            r#"{"id":3,"op":"ping"}"#, "\n",
            r#"{"id":4,"op":"ping"}"#, "\n",
        );
        // 每个响应都写失败：第 3 次后退出（第 4 个 ping 不应答即证明）。
        run_with_timeout(
            Cursor::new(input.as_bytes().to_vec()),
            FailingWriter,
            state,
            tx,
            rx,
            Duration::from_secs(30),
            Arc::clone(&shutdown),
        );
        assert!(shutdown.load(Ordering::SeqCst), "写失败退出同样置位关停（P1-1）");
    }

    /* --- 反向提问（askUser/answer）------------------------------------- */

    /// askUser（dispatch 层）：登记 pending + 产出服务端请求行（id ask-N
    /// 自增、questions 保持 UserQuestion serde 形状、timeoutMs 缺省回填）；
    /// 原请求 id 无立即响应（Deferred 应答）。
    #[test]
    fn ask_user_defers_reply_and_registers_pending() {
        let state = state();
        let questions = r#"[{"id":"q1","question":"选哪个？","header":"分组","options":[{"label":"甲","description":"说明"}],"multi_select":false}]"#;
        let line = format!(r#"{{"id":10,"op":"askUser","args":{{"questions":{questions},"timeoutMs":5000}}}}"#);
        let LineOutcome::Ask { server_line } = handle_line(&state, &line) else {
            panic!("askUser 应产出服务端请求行")
        };
        let value: Value = serde_json::from_str(&server_line).unwrap();
        assert_eq!(value["id"], json!("ask-1"), "服务端 ask id 自 ask-1 起独立命名空间");
        assert_eq!(value["op"], json!("askUser"));
        assert_eq!(value["args"]["timeoutMs"], json!(5000));
        // questions 与 UserQuestion serde 形状逐字段一致（snake_case
        // multi_select；header/description 可选项原样保留）。
        assert_eq!(value["args"]["questions"], serde_json::from_str::<Value>(questions).unwrap());
        assert_eq!(state.pending_ask_count(), 1, "pending 登记且无立即响应");

        // 并发多问：id 自增（ask-2）；timeoutMs 缺省 120000；预算取最早 deadline。
        let second = r#"{"id":11,"op":"askUser","args":{"questions":[{"id":"q2","question":"继续？"}]}}"#;
        let LineOutcome::Ask { server_line } = handle_line(&state, second) else {
            panic!("第二条 askUser 应产出服务端请求行")
        };
        let value: Value = serde_json::from_str(&server_line).unwrap();
        assert_eq!(value["id"], json!("ask-2"));
        assert_eq!(value["args"]["timeoutMs"], json!(DEFAULT_ASK_TIMEOUT_MS));
        assert_eq!(state.pending_ask_count(), 2);
        assert!(state.ask_wait_budget().is_some());
    }

    /// answer（dispatch 层）：命中 pending 后产出两条线——先原 askUser 请求
    /// id 的最终响应（answers 透传 + cancelled:false），再 answer 自己的
    /// ack（result:null，回显宿主任取的 id）；pending 命中即移除。
    #[test]
    fn answer_writes_deferred_reply_then_ack() {
        let state = state();
        let ask_line = r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"选哪个？","options":[{"label":"甲"}]}]}}"#;
        let LineOutcome::Ask { .. } = handle_line(&state, ask_line) else {
            panic!("askUser 应产出服务端请求行")
        };
        let out = handle_line(
            &state,
            r#"{"id":11,"op":"answer","args":{"id":"ask-1","answers":{"q1":["甲"]}}}"#,
        );
        let LineOutcome::Answered { original_line, ack_line } = out else {
            panic!("answer 应产出两条线")
        };
        // 顺序契约：原 askUser id 的最终响应在前。
        let original: Value = serde_json::from_str(&original_line).unwrap();
        assert_eq!(original["id"], json!(10));
        assert_eq!(original["ok"], json!(true));
        assert_eq!(original["result"]["answers"], json!({"q1": ["甲"]}));
        assert_eq!(original["result"]["cancelled"], json!(false));
        let ack: Value = serde_json::from_str(&ack_line).unwrap();
        assert_eq!(ack["id"], json!(11));
        assert_eq!(ack["ok"], json!(true));
        assert_eq!(ack["result"], json!(null));
        assert_eq!(state.pending_ask_count(), 0, "answer 命中即移除 pending");
    }

    /// answer 异常路径：查无该 id / 已消费后再答 → unknown ask；缺 id 参数
    /// → 参数缺失文案；均不登记/不残留 pending。
    #[test]
    fn answer_unknown_or_missing_ask_id_is_error() {
        let state = state();
        // 从未登记：unknown ask。
        let out = handle_line(&state, r#"{"id":1,"op":"answer","args":{"id":"ask-9"}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap().contains("unknown ask: ask-9"), "{value}");
        // 缺 id 参数。
        let out = handle_line(&state, r#"{"id":2,"op":"answer","args":{}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value["error"].as_str().unwrap().contains("answer 需要"), "{value}");
        // 已消费的 ask 再次应答：同一 unknown ask 路径（不给原 id 写第二次响应）。
        let _ = handle_line(&state, r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}]}}"#);
        let _ = handle_line(&state, r#"{"id":11,"op":"answer","args":{"id":"ask-1","answers":{}}}"#);
        let out = handle_line(&state, r#"{"id":12,"op":"answer","args":{"id":"ask-1"}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value["error"].as_str().unwrap().contains("unknown ask"), "{value}");
        assert_eq!(state.pending_ask_count(), 0);
    }

    /// askUser 参数校验：缺 questions → 专用文案；元素非法 → bad request；
    /// 失败路径不登记 pending；source 提供时随服务端请求行下发。
    #[test]
    fn ask_user_rejects_missing_or_malformed_questions() {
        let state = state();
        let out = handle_line(&state, r#"{"id":1,"op":"askUser","args":{}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap().contains("askUser 需要"), "{value}");
        assert_eq!(state.pending_ask_count(), 0, "失败路径不登记 pending");

        // questions 元素不是合法 UserQuestion（缺 id/题干）→ bad request。
        let out = handle_line(&state, r#"{"id":2,"op":"askUser","args":{"questions":[{"label":"不是问题"}]}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap().contains("bad request"), "{value}");

        // source 可选：提供才随行下发。
        let out = handle_line(
            &state,
            r#"{"id":3,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}],"source":"工具名"}}"#,
        );
        let LineOutcome::Ask { server_line } = out else { panic!("应有服务端请求行") };
        let value: Value = serde_json::from_str(&server_line).unwrap();
        assert_eq!(value["args"]["source"], json!("工具名"));
    }

    /// answer 顶层 `"cancelled":true`：最终响应 cancelled:true 且 answers 按
    /// 空表作废（顶层加法式键，缺省 false 由 Request serde 兜底）。
    #[test]
    fn cancelled_answer_reports_cancelled_with_empty_answers() {
        let state = state();
        let _ = handle_line(&state, r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}]}}"#);
        let out = handle_line(&state, r#"{"id":11,"op":"answer","args":{"id":"ask-1"},"cancelled":true}"#);
        let LineOutcome::Answered { original_line, ack_line } = out else {
            panic!("answer 应产出两条线")
        };
        let original: Value = serde_json::from_str(&original_line).unwrap();
        assert_eq!(original["result"]["cancelled"], json!(true));
        assert_eq!(original["result"]["answers"], json!({}), "取消时 answers 按空表作废");
        let ack: Value = serde_json::from_str(&ack_line).unwrap();
        assert_eq!(ack["ok"], json!(true));
    }

    /// 到期冲刷（dispatch 层）：timeoutMs:0 的 ask 立即到期、被
    /// take_due_asks 取走（携带原请求 id 与 timeoutMs）；迟到 answer 落
    /// unknown ask，不给原 id 写第二次响应。
    #[test]
    fn due_ask_expires_and_late_answer_is_unknown() {
        let state = state();
        let _ = handle_line(&state, r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}],"timeoutMs":0}}"#);
        assert_eq!(state.pending_ask_count(), 1);
        let due = state.take_due_asks();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, json!(10), "到期条目携带原 askUser 请求 id");
        assert_eq!(due[0].1, 0);
        assert_eq!(state.pending_ask_count(), 0);
        let out = handle_line(&state, r#"{"id":11,"op":"answer","args":{"id":"ask-1","answers":{"q1":["甲"]}}}"#);
        let LineOutcome::Reply(line) = out else { panic!("应有响应") };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value["error"].as_str().unwrap().contains("unknown ask"), "{value}");
    }

    /// 等待预算取各 pending ask 最早 deadline（并发多问各自独立计时；
    /// 无 pending → None = 无限期等待）。
    #[test]
    fn ask_wait_budget_takes_earliest_deadline() {
        let state = state();
        assert_eq!(state.ask_wait_budget(), None, "无 pending 无预算");
        let _ = handle_line(&state, r#"{"id":1,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}],"timeoutMs":5000}}"#);
        let first = state.ask_wait_budget().unwrap();
        let _ = handle_line(&state, r#"{"id":2,"op":"askUser","args":{"questions":[{"id":"q2","question":"？"}],"timeoutMs":50}}"#);
        let second = state.ask_wait_budget().unwrap();
        assert!(second < first, "预算取最早 deadline: {second:?} vs {first:?}");
        assert!(second <= Duration::from_millis(50));
        assert!(first <= Duration::from_millis(5000));
    }

    /// run 回环：askUser → 服务端请求行 → answer → 「原 id 终响应 + ack」
    /// 依序写出，随后 stop 干净退出（pending 已清空 → 无作废通知行）。
    #[test]
    fn run_loop_ask_answer_and_stop_writes_lines_in_order() {
        let state = state();
        let input = concat!(
            r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"选哪个？","options":[{"label":"甲"},{"label":"乙"}],"multi_select":false}],"timeoutMs":60000}}"#, "\n",
            r#"{"id":11,"op":"answer","args":{"id":"ask-1","answers":{"q1":["乙"]}}}"#, "\n",
            "stop", "\n",
        );
        let lines = serve_roundtrip(state, input.as_bytes().to_vec(), Duration::from_secs(30));
        assert_eq!(lines.len(), 3, "服务端请求行 + 终响应 + ack（stop 不回）: {lines:?}");
        let ask: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(ask["id"], json!("ask-1"));
        assert_eq!(ask["op"], json!("askUser"));
        let original: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(original["id"], json!(10));
        assert_eq!(original["ok"], json!(true));
        assert_eq!(original["result"]["answers"], json!({"q1": ["乙"]}));
        let ack: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(ack["id"], json!(11));
        assert_eq!(ack["result"], json!(null));
    }

    /// run 回环：pending 未答时 stop——对每条未答 ask 补一行 cancelled:true
    /// 作废通知（顶层加法式可选键，args 原样重建）后退出。
    #[test]
    fn run_loop_stop_cancels_pending_asks() {
        let state = state();
        let input = concat!(
            r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}]}}"#, "\n",
            "stop", "\n",
        );
        let lines = serve_roundtrip(state, input.as_bytes().to_vec(), Duration::from_secs(30));
        assert_eq!(lines.len(), 2, "服务端请求行 + 作废通知行: {lines:?}");
        let cancel: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(cancel["id"], json!("ask-1"));
        assert_eq!(cancel["op"], json!("askUser"));
        assert_eq!(cancel["cancelled"], json!(true));
        assert_eq!(cancel["args"]["timeoutMs"], json!(DEFAULT_ASK_TIMEOUT_MS));
    }

    /// run 回环：pending 未答时 stdin EOF——直接清表退出（无作废通知行），
    /// 服务收场不悬挂。
    #[test]
    fn run_loop_eof_with_pending_asks_exits_cleanly() {
        let state = state();
        let mut input = r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}]}}"#.to_string().into_bytes();
        input.push(b'\n');
        let lines = serve_roundtrip(state, input, Duration::from_secs(30));
        assert_eq!(lines.len(), 1, "EOF 只留服务端请求行，清表退出: {lines:?}");
        let ask: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(ask["id"], json!("ask-1"));
    }

    /// run 回环：timeoutMs:0 的 ask 在下一轮等待前即到期冲刷（原 id 回 ask
    /// 超时响应），后续 ping 照常应答——反向等待不冻结主循环。
    #[test]
    fn run_loop_expires_due_ask_without_blocking_follow_ups() {
        let state = state();
        let input = concat!(
            r#"{"id":10,"op":"askUser","args":{"questions":[{"id":"q1","question":"？"}],"timeoutMs":0}}"#, "\n",
            r#"{"id":2,"op":"ping"}"#, "\n",
            "stop", "\n",
        );
        let lines = serve_roundtrip(state, input.as_bytes().to_vec(), Duration::from_secs(30));
        assert_eq!(lines.len(), 3, "服务端请求行 + ask 超时响应 + ping 应答: {lines:?}");
        let timed_out: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(timed_out["id"], json!(10), "ask 超时响应回带原 askUser 请求 id");
        assert_eq!(timed_out["ok"], json!(false));
        assert!(timed_out["error"].as_str().unwrap().contains("提问超时"), "{timed_out}");
        let ping: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(ping["id"], json!(2));
        assert_eq!(ping["ok"], json!(true), "ask 超时不阻塞后续请求");
    }
}
