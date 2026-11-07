//! 被控端：采集 → 差分 → 编码 → 发送；回放输入。
//!
//! 由 T3 实现，用法：
//! ```text
//! lrc-host --listen 0.0.0.0:7800 --password 1234 [--display 0] [--fps 30] [--quality 85]
//! ```

fn main() {
    // T3 实现：CLI → 初始化采集 → tokio 免用（std 线程模型）→ 连接/发送循环
}
