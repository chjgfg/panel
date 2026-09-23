// 网页版控制台：把一个 `ssh root@<本机IP>` 会话桥到浏览器里的 WebSocket 终端。
//
// 为什么要 PTY：ssh 检测到没有 tty 就不肯交互（密码提示直接失败），vim/top
// 这类全屏程序也要 tty 才能画界面。所以这里用 portable-pty 造一个伪终端，
// 让 ssh 挂上去，它就以为自己在真终端里跑。
//
// 桥接分三条线：
//   reader 线程  —— 阻塞读 PTY 输出，塞进 tokio channel，主循环再发给浏览器
//   writer 线程  —— 从 channel 取浏览器击键，阻塞写进 PTY
//   waiter 线程  —— 等 ssh 进程退出，通过 oneshot 通知主循环收摊
// portable-pty 的读写是阻塞式的，不能直接在 async 里调，所以各开一个系统线程。
use std::io::{Read, Write};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::state::App;

pub async fn terminal_ws(
    State(_app): State<Arc<App>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // 同源校验，防跨站 WebSocket 劫持（CSWSH）：cookie 是 SameSite=Lax 本就挡住了
    // 跨站脚本发起的连接，这里再核一遍 Origin 的 host 必须和 Host 一致，纵深防御。
    // 非浏览器客户端（无 Origin）已被 require_auth 的 cookie 拦在外面，放行。
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let same = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .is_some_and(|o| Some(o) == host.as_deref());
        if !same {
            return (StatusCode::FORBIDDEN, "跨源 WebSocket 拒绝").into_response();
        }
    }

    // ssh 的目标 = 浏览器访问面板用的主机名（Host 头去掉端口）。
    // 用户就是冲着这个 IP 开的面板，ssh root@它 正好是需求要的「连本机」。
    let target = ssh_target(host.as_deref());
    ws.on_upgrade(move |socket| bridge(socket, target))
}

/// 从 Host 头解析出 ssh 目标：去掉端口，校验字符，取不到就回退 127.0.0.1。
/// - `1.2.3.4:80`  -> `1.2.3.4`
/// - `[::1]:80`    -> `::1`
/// - `example.com` -> `example.com`
fn ssh_target(host: Option<&str>) -> String {
    let fallback = "127.0.0.1".to_string();
    let Some(h) = host else { return fallback };
    let h = h.trim();
    // IPv6 字面量带方括号：[::1]:80 —— 取方括号里的部分
    let bare = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, _)) => addr,
            None => return fallback,
        }
    } else {
        // 普通 host[:port]，从右边切掉端口（IPv6 无括号时不含单个冒号规则，这里只切最后一段数字端口）
        h.rsplit_once(':').map_or(h, |(a, _)| a)
    };
    let ok = !bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    if ok { bare.to_string() } else { fallback }
}

/// 解析前端发来的缩放控制帧：文本 `R <cols> <rows>`。返回 (cols, rows)。
fn parse_resize(t: &str) -> Option<(u16, u16)> {
    let mut it = t.split_whitespace();
    if it.next()? != "R" {
        return None;
    }
    let cols: u16 = it.next()?.parse().ok()?;
    let rows: u16 = it.next()?.parse().ok()?;
    Some((cols.max(1), rows.max(1)))
}

async fn bridge(mut socket: WebSocket, target: String) {
    // 造 PTY
    let pair = match native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("开不了终端：{e}\r\n").into()))
                .await;
            return;
        }
    };

    // ssh root@目标。accept-new：首次连自动记住 host key，不卡在 yes/no 提示上。
    let mut cmd = CommandBuilder::new("ssh");
    cmd.arg("-o");
    cmd.arg("StrictHostKeyChecking=accept-new");
    cmd.arg("-o");
    cmd.arg("ConnectTimeout=10");
    cmd.arg(format!("root@{target}"));
    cmd.env("TERM", "xterm-256color");
    cmd.cwd("/root");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("起不了 ssh：{e}\r\n").into()))
                .await;
            return;
        }
    };
    // slave 留着不用，父进程这端要尽早关掉，否则 ssh 退出后 PTY 不会收到 EOF
    drop(pair.slave);

    let master = pair.master;
    let mut reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("读不了终端：{e}\r\n").into()))
                .await;
            let _ = child.kill();
            return;
        }
    };
    let mut writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("写不了终端：{e}\r\n").into()))
                .await;
            let _ = child.kill();
            return;
        }
    };

    // reader：阻塞读 PTY 输出 -> tokio channel
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // writer：std channel <- 浏览器击键，阻塞写进 PTY
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    // waiter：ssh 退出后通知主循环
    let mut killer = child.clone_killer();
    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = exit_tx.send(());
    });

    let (mut ws_tx, mut ws_rx) = socket.split();
    loop {
        tokio::select! {
            // PTY 有输出 -> 发给浏览器（二进制原样透传）
            data = out_rx.recv() => match data {
                Some(bytes) => {
                    if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // reader 线程结束（PTY EOF）
            },
            // 浏览器来消息
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if in_tx.send(b.to_vec()).is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    // 目前只有缩放控制帧：R <cols> <rows>
                    if let Some((cols, rows)) = parse_resize(t.as_str()) {
                        let _ = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // Ping/Pong 由 axum 处理
                Some(Err(_)) => break,
            },
            // ssh 进程自己退出了
            _ = &mut exit_rx => break,
        }
    }

    // 收摊：杀掉 ssh（浏览器关弹窗/断线时别让它挂着），关掉 WebSocket。
    // in_tx / out_rx 在这里 drop，两个 IO 线程随之因 channel 关闭而退出。
    let _ = killer.kill();
    let _ = ws_tx.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从host头解析ssh目标() {
        assert_eq!(ssh_target(Some("1.2.3.4:80")), "1.2.3.4");
        assert_eq!(ssh_target(Some("1.2.3.4")), "1.2.3.4");
        assert_eq!(ssh_target(Some("panel.example.com:8080")), "panel.example.com");
        assert_eq!(ssh_target(Some("[::1]:80")), "::1");
        assert_eq!(ssh_target(None), "127.0.0.1");
        // 非法字符（可能夹带注入意图）一律回退，绝不原样拼进命令行
        assert_eq!(ssh_target(Some("a b c")), "127.0.0.1");
        assert_eq!(ssh_target(Some("a;rm -rf/")), "127.0.0.1");
    }

    #[test]
    fn 解析缩放控制帧() {
        assert_eq!(parse_resize("R 120 40"), Some((120, 40)));
        assert_eq!(parse_resize("R 0 0"), Some((1, 1))); // 下限保护
        assert_eq!(parse_resize("hello"), None);
        assert_eq!(parse_resize("R 120"), None);
    }
}
