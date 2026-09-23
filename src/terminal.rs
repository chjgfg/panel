// 网页版控制台：把一个 `ssh root@<本机IP>` 会话桥到浏览器里的 WebSocket 终端。
//
// 为什么要 PTY：ssh 检测到没有 tty 就不肯交互（密码/私钥口令提示直接失败），
// vim/top 这类全屏程序也要 tty 才能画界面。所以这里用 portable-pty 造一个伪
// 终端，让 ssh 挂上去，它就以为自己在真终端里跑。
//
// 认证方式：前端可在「配置密钥」里粘贴 SSH 私钥。连接握手时前端先发一帧
// 初始配置（含私钥或「无钥」），后端把私钥落成一个 0600 的临时文件，用
// `ssh -i 临时文件 -o IdentitiesOnly=yes -o PreferredAuthentications=publickey`
// 发起公钥认证，连接结束后立刻删掉该临时文件。
//
// 桥接分三条线：
//   reader 线程  —— 阻塞读 PTY 输出，塞进 tokio channel，主循环再发给浏览器；
//                   顺便扫一眼有没有「Permission denied」，有就置公钥认证失败标记
//   writer 线程  —— 从 channel 取浏览器击键，阻塞写进 PTY
//   waiter 线程  —— 等 ssh 进程退出，通过 oneshot 通知主循环收摊
// portable-pty 的读写是阻塞式的，不能直接在 async 里调，所以各开一个系统线程。
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
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
        // 普通 host[:port]，从右边切掉端口
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

/// 从初始配置帧解析私钥：文本以 `K\n` 开头则其余为私钥内容，其它（如 `N`）表示无钥。
/// trim 后为空也当无钥。
fn parse_init_key(t: &str) -> Option<String> {
    let k = t.strip_prefix("K\n")?;
    let k = k.trim();
    (!k.is_empty()).then(|| k.to_string())
}

/// 等前端发来的第一帧初始配置（含私钥或无钥）。带超时兜底：
/// 拿到正常文本帧 -> Ok(Some(key)/None)；对端关闭 -> Err(())；超时按无钥放行。
async fn recv_init(socket: &mut WebSocket) -> Result<Option<String>, ()> {
    match tokio::time::timeout(Duration::from_secs(15), socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => Ok(parse_init_key(t.as_str())),
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => Err(()),
        Ok(Some(Ok(_))) => Ok(None),      // 非文本先到（不该发生），当无钥
        Ok(Some(Err(_))) => Err(()),      // 连接错误
        Err(_) => Ok(None),               // 超时：不卡住，按无钥继续
    }
}

/// 把私钥写成一个仅本次连接使用的临时文件，权限 0600（ssh 对宽松权限的私钥会拒绝加载）。
/// ssh 只能从文件读 identity，没法走 stdin/env，所以必须落地；用完由调用方删除。
fn write_temp_key(content: &str) -> Option<PathBuf> {
    let mut rand = [0u8; 16];
    getrandom::fill(&mut rand).ok()?;
    let hex: String = rand.iter().map(|b| format!("{b:02x}")).collect();
    let path = std::env::temp_dir().join(format!("panel-sshkey-{hex}"));

    let mut f = std::fs::File::create(&path).ok()?;
    // 收摊时才谈权限没意义——先把权限收紧到 0600，再写入内容
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(content.as_bytes()).ok()?;
    // 私钥必须以换行结尾，否则部分 ssh 版本会报 "invalid format"
    if !content.ends_with('\n') {
        f.write_all(b"\n").ok()?;
    }
    Some(path)
}

async fn bridge(mut socket: WebSocket, target: String) {
    // 1) 先收初始配置帧，拿到（可选的）私钥
    let key = match recv_init(&mut socket).await {
        Ok(k) => k,
        Err(()) => return, // 对端已关/出错，没什么可做
    };
    let keyfile = key.as_deref().and_then(write_temp_key);
    // 让主动断连/异常路径也能删掉私钥文件：结束前统一 remove
    let cleanup_key = keyfile.clone();
    let cleanup = || {
        if let Some(p) = &cleanup_key {
            let _ = std::fs::remove_file(p);
        }
    };

    // 2) 造 PTY
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
            cleanup();
            return;
        }
    };

    // 3) 拼 ssh 命令。accept-new：首次连自动记住 host key，不卡在 yes/no 提示上。
    //    有私钥就强制走公钥认证（IdentitiesOnly 只用这把钥、不掺 agent 里的），
    //    这样认证失败会干脆利落地报 publickey，前端好据此提示。
    //    私钥若带口令，ssh 会在这个 PTY 里提示输入，用户照常输即可。
    let mut cmd = CommandBuilder::new("ssh");
    if let Some(kf) = &keyfile {
        cmd.arg("-i");
        cmd.arg(kf.display().to_string());
        cmd.arg("-o");
        cmd.arg("IdentitiesOnly=yes");
        cmd.arg("-o");
        cmd.arg("PreferredAuthentications=publickey");
    }
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
            cleanup();
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
            cleanup();
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
            cleanup();
            return;
        }
    };

    // 公钥认证失败标记：reader 线程扫到 "Permission denied" 就置位，结束时告诉前端
    let auth_fail = Arc::new(AtomicBool::new(false));

    // reader：阻塞读 PTY 输出 -> tokio channel
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let auth_fail_r = auth_fail.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    // ssh 认证被拒时会打 "... Permission denied (publickey)."
                    if !auth_fail_r.load(Ordering::Relaxed)
                        && find_sub(chunk, b"Permission denied")
                    {
                        auth_fail_r.store(true, Ordering::Relaxed);
                    }
                    if out_tx.blocking_send(chunk.to_vec()).is_err() {
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

    // 收摊：认证失败的话先给前端发一个状态帧（文本帧=状态，二进制帧才是终端内容），
    // 前端据此弹「公钥认证失败，检查私钥配置」。然后杀 ssh、关连接、删私钥文件。
    if auth_fail.load(Ordering::Relaxed) {
        let _ = ws_tx.send(Message::Text("AUTHFAIL".into())).await;
    }
    let _ = killer.kill();
    let _ = ws_tx.send(Message::Close(None)).await;
    cleanup();
}

/// 在字节流里找子串（认证失败特征串很短，朴素查找足够）
fn find_sub(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
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

    #[test]
    fn 解析初始配置帧里的私钥() {
        assert_eq!(
            parse_init_key("K\n-----BEGIN KEY-----\nabc\n"),
            Some("-----BEGIN KEY-----\nabc".to_string())
        );
        assert_eq!(parse_init_key("N"), None); // 无钥
        assert_eq!(parse_init_key("K\n   \n"), None); // 空白当无钥
        assert_eq!(parse_init_key("R 80 24"), None); // 非初始帧
    }

    #[test]
    fn 字节流子串查找() {
        assert!(find_sub(b"xx Permission denied (publickey).", b"Permission denied"));
        assert!(!find_sub(b"welcome to server", b"Permission denied"));
    }
}
