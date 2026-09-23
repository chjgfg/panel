// 网页版控制台：把一个 `ssh root@<本机IP>` 会话桥到浏览器里的 WebSocket 终端。
//
// 为什么要 PTY：ssh 检测到没有 tty 就不肯交互（密码/私钥口令提示直接失败），
// vim/top 这类全屏程序也要 tty 才能画界面。所以这里用 portable-pty 造一个伪
// 终端，让 ssh 挂上去，它就以为自己在真终端里跑。
//
// 认证方式：SSH 私钥存在服务器上（跟配置文件同目录的 panel_ssh_key，权限 0600），
// 通过 /api/sshkey 接口配置。存服务器而不是浏览器里，换个浏览器也不用重配。
// 连接时若已配私钥，就用 `ssh -i panel_ssh_key -o IdentitiesOnly=yes
// -o PreferredAuthentications=publickey` 发起公钥认证；私钥带口令的话 ssh 会在
// 这个 PTY 里提示输入。
//
// 桥接分三条线：
//   reader 线程  —— 阻塞读 PTY 输出，塞进 tokio channel，主循环再发给浏览器；
//                   顺便扫一眼有没有「Permission denied」，有就置公钥认证失败标记
//   writer 线程  —— 从 channel 取浏览器击键，阻塞写进 PTY
//   waiter 线程  —— 等 ssh 进程退出，通过 oneshot 通知主循环收摊
// portable-pty 的读写是阻塞式的，不能直接在 async 里调，所以各开一个系统线程。
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::state::App;

pub async fn terminal_ws(
    State(app): State<Arc<App>>,
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
    // 已配好私钥就把它的路径交给 ssh -i；没配就走默认认证（可能失败或走密码）。
    let keyfile = key_configured(&app.ssh_key_path).then(|| app.ssh_key_path.clone());
    ws.on_upgrade(move |socket| bridge(socket, target, keyfile))
}

// ---------- SSH 私钥的存/查/清（都落在服务器上，换浏览器不丢）----------

#[derive(serde::Serialize)]
pub struct KeyStatus {
    configured: bool,
}

/// 只回「配没配」，绝不把私钥内容回显给浏览器
pub async fn sshkey_status(State(app): State<Arc<App>>) -> Json<KeyStatus> {
    Json(KeyStatus {
        configured: key_configured(&app.ssh_key_path),
    })
}

/// 保存私钥：body 是私钥明文。落成 0600 文件，覆盖旧的。
pub async fn sshkey_save(State(app): State<Arc<App>>, body: String) -> Response {
    let norm = normalize_key(&body);
    if norm.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "私钥不能为空").into_response();
    }
    // 轻校验：像不像私钥，早点拦下明显贴错的内容
    if !norm.contains("PRIVATE KEY") {
        return (StatusCode::BAD_REQUEST, "这看起来不是 SSH 私钥（应含 PRIVATE KEY）")
            .into_response();
    }
    match write_key_file(&app.ssh_key_path, &norm) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("保存失败：{e}")).into_response(),
    }
}

/// 清除已保存的私钥（没配过也算成功，幂等）
pub async fn sshkey_clear(State(app): State<Arc<App>>) -> Response {
    match std::fs::remove_file(&app.ssh_key_path) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("清除失败：{e}")).into_response(),
    }
}

/// 私钥文件存在且非空 = 已配置
fn key_configured(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// 规整私钥文本：统一成 \n 换行（Windows 的 \r\n 会让部分 ssh 版本报 invalid
/// format），去掉尾部多余空白后补一个换行（私钥文件必须以换行结尾）。
fn normalize_key(raw: &str) -> String {
    let mut s = raw.replace("\r\n", "\n").replace('\r', "\n");
    s.truncate(s.trim_end().len());
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn write_key_file(path: &Path, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    // 先把权限收紧到 0600，再写内容（ssh 对宽松权限的私钥会拒绝加载）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(content.as_bytes())?;
    Ok(())
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

/// 在字节流里找子串（认证失败特征串很短，朴素查找足够）
fn find_sub(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 公钥行的特征前缀（authorized_keys / *.pub / known_hosts 里这些开头）
const PUBKEY_TOKENS: &[&[u8]] = &[
    b"ssh-rsa ",
    b"ssh-ed25519 ",
    b"ssh-dss ",
    b"ecdsa-sha2-",
    b"sk-ssh-ed25519@",
    b"sk-ecdsa-sha2-",
];

/// 口令哈希的特征串（/etc/shadow、mkpasswd/openssl passwd 输出里就长这样）。
/// crypt 格式 `$id$...`：6=sha512、5=sha256、1=md5、2a/2b/2y=bcrypt、y=yescrypt、7=scrypt。
const HASH_TOKENS: &[&[u8]] = &[
    b"$6$", b"$5$", b"$2a$", b"$2b$", b"$2y$", b"$y$", b"$7$", b"$argon2",
];

/// 输出侧的敏感内容拦截器：把流经终端的私钥 PEM 块、公钥行、口令哈希行替换成提示，
/// 这样不管对方用 cat / less / vi / base64 哪种方式去读，敏感内容都到不了浏览器。
///
/// 按行处理：完整行逐行判定；尾部那截还没换行的不完整行，只有在「看起来敏感」
/// 时才暂存等下一块，否则立即放行——否则交互式提示符、全屏程序（top/vim）会被卡住。
///
/// 这是**尽力而为**的遮挡，不是安全边界：控制台本质是 root 会话，真要绕过办法很多
/// （改文件名、编码后再传、写脚本……）。要紧的私钥根本就不该留在这台机器上。
struct KeyFilter {
    /// 正处在一段要屏蔽的私钥 PEM 内（BEGIN 之后、END 之前）
    in_pem: bool,
    /// 上一行是否被拦掉了——连续多行敏感（如整份 shadow）只提示一次，不刷屏
    prev_redacted: bool,
    /// 跨 chunk 的不完整尾行缓冲
    buf: Vec<u8>,
}

impl KeyFilter {
    fn new() -> Self {
        Self {
            in_pem: false,
            prev_redacted: false,
            buf: Vec::new(),
        }
    }

    /// 喂入一块原始输出，返回可以安全发给浏览器的字节
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            self.filter_line(&line, &mut out);
        }
        // 处理尾部不完整行：含敏感迹象（或正在 PEM 内）才攥着等下一块，
        // 否则立即放行以保交互。异常长的一行设个安全阀，别一直攥着不放。
        if self.in_pem || Self::sensitive(&self.buf) {
            if self.buf.len() > 4096 {
                let line = std::mem::take(&mut self.buf);
                self.filter_line(&line, &mut out);
            }
        } else {
            out.append(&mut self.buf);
        }
        out
    }

    /// 连接结束时把攥着的尾行吐出来（若在 PEM 内则丢弃）
    fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.filter_line(&line, &mut out);
        }
        out
    }

    fn filter_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        if self.in_pem {
            // PEM 体和 END 行一律丢弃；见到 END 就退出屏蔽态
            if find_sub(line, b"-----END") {
                self.in_pem = false;
            }
            self.prev_redacted = true;
            return;
        }
        let notice: Option<&[u8]> =
            if find_sub(line, b"-----BEGIN") && find_sub(line, b"PRIVATE KEY") {
                self.in_pem = true;
                Some("\r\n[已拦截：私钥内容不给读]\r\n".as_bytes())
            } else if PUBKEY_TOKENS.iter().any(|t| find_sub(line, t)) {
                Some("\r\n[已拦截：公钥内容不给读]\r\n".as_bytes())
            } else if HASH_TOKENS.iter().any(|t| find_sub(line, t)) {
                Some("\r\n[已拦截：疑似口令哈希不给读]\r\n".as_bytes())
            } else {
                None
            };
        match notice {
            Some(msg) => {
                // 连续多行敏感只在头一行提示，避免整份 shadow 刷一屏「已拦截」
                if !self.prev_redacted {
                    out.extend_from_slice(msg);
                }
                self.prev_redacted = true;
            }
            None => {
                out.extend_from_slice(line);
                self.prev_redacted = false;
            }
        }
    }

    /// 不完整尾行是否有敏感迹象（有就先攥着，等它成整行再判定）
    fn sensitive(partial: &[u8]) -> bool {
        find_sub(partial, b"-----BEGIN")
            || find_sub(partial, b"PRIVATE KEY")
            || PUBKEY_TOKENS.iter().any(|t| find_sub(partial, t))
            || HASH_TOKENS.iter().any(|t| find_sub(partial, t))
    }
}

/// 黑客爱翻的敏感文件/目录（口令、密钥、凭证、历史记录）。命令行里一旦引用到
/// 这些路径就整条拦掉、不放行——挡的是「随手 cat 一下」这类顺手的窥探。
const SENSITIVE_PATHS: &[&[u8]] = &[
    b"/etc/shadow",
    b"/etc/gshadow",
    b"/etc/sudoers",
    b"/etc/ssh/ssh_host_", // sshd 的主机私钥
    b"id_rsa",
    b"id_dsa",
    b"id_ecdsa",
    b"id_ed25519",
    b"authorized_keys",
    b"known_hosts",
    b".aws/credentials",
    b".git-credentials",
    b".netrc",
    b".pgpass",
    b".my.cnf",
    b".kube/config",
    b"kubeconfig",
    b".docker/config.json",
    b".bash_history",
    b".zsh_history",
    b".mysql_history",
    b".python_history",
    b".gnupg",
    b".password-store",
    b"panel_ssh_key", // 面板自己存的私钥
    b"panel.toml",    // 面板配置，里面有明文密码
    b".env",
];

/// 一行命令是否碰了敏感路径
fn cmd_touches_secret(line: &[u8]) -> bool {
    SENSITIVE_PATHS.iter().any(|p| find_sub(line, p))
}

/// 输入侧的命令拦截器：逐字节跟一行命令，回车时若这行引用了敏感路径，就吞掉回车
/// （命令不执行），由调用方再发个 Ctrl-U 把已回显的命令抹掉。
///
/// 只跟得住「一个字一个字敲出来」的简单命令：一旦出现方向键/历史翻页（ESC 序列）
/// 或 Tab 补全，就置 dirty、这行放弃拦截（宁可放过，也不误伤）。同样是尽力而为。
struct InputGuard {
    line: Vec<u8>,
    /// 这行掺进了没法可靠还原的编辑操作（方向键/补全等），本行不拦
    dirty: bool,
}

impl InputGuard {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            dirty: false,
        }
    }

    /// 处理一块浏览器击键，返回 (要转发给 PTY 的字节, 是否拦下了某条命令)
    fn feed(&mut self, chunk: &[u8]) -> (Vec<u8>, bool) {
        let mut fwd = Vec::with_capacity(chunk.len());
        let mut blocked = false;
        for &b in chunk {
            match b {
                b'\r' | b'\n' => {
                    if !self.dirty && cmd_touches_secret(&self.line) {
                        blocked = true; // 吞掉这个回车，命令不执行
                    } else {
                        fwd.push(b);
                    }
                    self.line.clear();
                    self.dirty = false;
                }
                0x1b => {
                    // ESC 序列（方向键/历史等），没法可靠跟踪，本行放弃拦截
                    self.dirty = true;
                    self.line.clear();
                    fwd.push(b);
                }
                0x09 => {
                    // Tab 补全会改写命令行，放弃跟踪
                    self.dirty = true;
                    fwd.push(b);
                }
                0x03 => {
                    // Ctrl-C：放弃当前行
                    self.line.clear();
                    self.dirty = false;
                    fwd.push(b);
                }
                0x15 => {
                    // Ctrl-U：清空当前行
                    self.line.clear();
                    fwd.push(b);
                }
                0x7f | 0x08 => {
                    // 退格
                    self.line.pop();
                    fwd.push(b);
                }
                b if b >= 0x20 => {
                    self.line.push(b);
                    fwd.push(b);
                }
                b => fwd.push(b), // 其它控制字符原样透传，不动 line
            }
        }
        (fwd, blocked)
    }
}

async fn bridge(mut socket: WebSocket, target: String, keyfile: Option<PathBuf>) {
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

    // 拼 ssh 命令。accept-new：首次连自动记住 host key，不卡在 yes/no 提示上。
    // 有私钥就强制走公钥认证（IdentitiesOnly 只用这把钥、不掺 agent 里的），
    // 这样认证失败会干脆利落地报 publickey，前端好据此提示。
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

    // 公钥认证失败标记：reader 线程扫到 "Permission denied" 就置位，结束时告诉前端
    let auth_fail = Arc::new(AtomicBool::new(false));

    // reader：阻塞读 PTY 输出 -> tokio channel
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let auth_fail_r = auth_fail.clone();
    std::thread::spawn(move || {
        let mut filter = KeyFilter::new();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    // ssh 认证被拒时会打 "... Permission denied (publickey)."。
                    // 在原始输出上判定（它不是密钥，不会被下面的过滤器动到）。
                    if !auth_fail_r.load(Ordering::Relaxed) && find_sub(chunk, b"Permission denied")
                    {
                        auth_fail_r.store(true, Ordering::Relaxed);
                    }
                    // 过滤掉私钥/公钥内容再发给浏览器
                    let filtered = filter.push(chunk);
                    if !filtered.is_empty() && out_tx.blocking_send(filtered).is_err() {
                        break;
                    }
                }
            }
        }
        // 收尾：把攥着的尾行吐出来（PEM 内的会被丢弃）
        let rest = filter.flush();
        if !rest.is_empty() {
            let _ = out_tx.blocking_send(rest);
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
    // 输入侧命令拦截：命令行里引用敏感路径时拦下该命令（尽力而为，见 InputGuard）
    let mut guard = InputGuard::new();
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
                    let (fwd, blocked) = guard.feed(&b);
                    if !fwd.is_empty() && in_tx.send(fwd).is_err() {
                        break;
                    }
                    if blocked {
                        // 命令被拦：发个 Ctrl-U 抹掉已回显的命令（回车已被吞、没换行），
                        // 再发个换行把这次被拦回显到终端，然后补一句提示。
                        // 已知小瑕疵：从换行到 Ctrl-U 到达之间，shell 可能把提示符也回显出来，
                        // 于是提示符后面跟着提示语——不影响功能，能接受。
                        let _ = in_tx.send(vec![0x15]);
                        let notice = "\r\n\x1b[31m[已拦截：这条命令引用了受保护的敏感文件/目录，未执行]\x1b[0m\r\n";
                        if ws_tx
                            .send(Message::Binary(notice.as_bytes().to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        let _ = in_tx.send(vec![b'\r']);
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
    // 前端据此弹「公钥认证失败，检查私钥配置」。然后杀 ssh、关连接。
    if auth_fail.load(Ordering::Relaxed) {
        let _ = ws_tx.send(Message::Text("AUTHFAIL".into())).await;
    }
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

    #[test]
    fn 字节流子串查找() {
        assert!(find_sub(b"xx Permission denied (publickey).", b"Permission denied"));
        assert!(!find_sub(b"welcome to server", b"Permission denied"));
    }

    // 把若干块喂给过滤器，拼出最终转发给浏览器的文本
    fn run_filter(chunks: &[&[u8]]) -> String {
        let mut f = KeyFilter::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(f.push(c));
        }
        out.extend(f.flush());
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn 私钥pem整块被拦掉() {
        let dump = b"cat id_rsa\r\n\
                     -----BEGIN OPENSSH PRIVATE KEY-----\r\n\
                     b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAA=\r\n\
                     AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\
                     -----END OPENSSH PRIVATE KEY-----\r\n\
                     $ ";
        let out = run_filter(&[dump]);
        // 命令回显和提示符还在，但私钥的头/体/尾都没了
        assert!(out.contains("cat id_rsa"));
        assert!(out.contains("已拦截"));
        assert!(!out.contains("BEGIN OPENSSH PRIVATE KEY"));
        assert!(!out.contains("b3BlbnNzaC1rZXk")); // base64 私钥体
        assert!(out.ends_with("$ ")); // 提示符照常放行（末尾无换行）
    }

    #[test]
    fn 私钥被拆成多块喂进来也拦得住() {
        // 模拟 read() 把 PEM 切在奇怪的位置
        let out = run_filter(&[
            b"-----BEGIN RSA PRIV",
            b"ATE KEY-----\nMIIEpAIBAAKC\nAQEA\n-----END RSA PRIVATE KEY-----\ndone\n",
        ]);
        assert!(!out.contains("MIIEpAIBAAKC"));
        assert!(out.contains("已拦截"));
        assert!(out.contains("done"));
    }

    #[test]
    fn 公钥行被拦掉() {
        let out = run_filter(&[b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc user@host\n"]);
        assert!(!out.contains("AAAAC3NzaC1lZDI1NTE5"));
        assert!(out.contains("已拦截"));
    }

    #[test]
    fn 普通输出与交互提示符照常放行() {
        // 整行普通输出
        assert_eq!(run_filter(&[b"hello world\n"]), "hello world\n");
        // 不带换行的交互提示符：不能被攥住，必须立刻放行，否则终端像卡死
        let mut f = KeyFilter::new();
        assert_eq!(String::from_utf8_lossy(&f.push(b"root@host:~# ")), "root@host:~# ");
    }

    #[test]
    fn 口令哈希行被拦掉() {
        let out = run_filter(&[b"root:$6$abcDEF123$xyz...:19000:0:99999:7:::\n"]);
        assert!(!out.contains("$6$abcDEF123"));
        assert!(out.contains("已拦截"));
    }

    #[test]
    fn 整份shadow连续敏感只提示一次() {
        let dump = b"root:$6$aaaa$bbbb:19000:0:99999:7:::\n\
                     daemon:*:19000:0:99999:7:::\n\
                     bin:*:19000:0:99999:7:::\n";
        let out = run_filter(&[dump]);
        assert_eq!(out.matches("已拦截").count(), 1); // 连着的敏感行只提示一次
    }

    #[test]
    fn 命令行引用敏感路径被拦() {
        let mut g = InputGuard::new();
        // 敲 "cat /etc/shadow" 再回车 -> 回车被吞、报告拦下
        let (fwd, blocked) = g.feed(b"cat /etc/shadow\r");
        assert!(blocked);
        assert!(!fwd.contains(&b'\r')); // 回车没转发出去，命令不会执行
        assert_eq!(fwd, b"cat /etc/shadow"); // 已敲的字符照常回显

        // 退格把敏感词删掉后回车，就该放行
        let mut g = InputGuard::new();
        g.feed(b"cat /etc/shadow");
        g.feed(&[0x7f; 14]); // 删掉 "/etc/shadow"
        let (fwd, blocked) = g.feed(b"\r");
        assert!(!blocked);
        assert_eq!(fwd, b"\r");
    }

    #[test]
    fn 普通命令不被拦() {
        let mut g = InputGuard::new();
        let (fwd, blocked) = g.feed(b"ls -la /opt/apps\r");
        assert!(!blocked);
        assert_eq!(fwd, b"ls -la /opt/apps\r");
    }

    #[test]
    fn 命令行碰了面板私钥和配置也得拦() {
        assert!(cmd_touches_secret(b"cat panel_ssh_key"));
        assert!(cmd_touches_secret(b"vim /opt/panel/panel.toml"));
        assert!(cmd_touches_secret(b"tail -f ~/.bash_history"));
        assert!(!cmd_touches_secret(b"ls /opt/apps"));
    }

    #[test]
    fn 规整私钥统一换行且补尾换行() {
        assert_eq!(normalize_key("-----BEGIN-----\r\nabc\r\n"), "-----BEGIN-----\nabc\n");
        assert_eq!(normalize_key("key\n\n\n"), "key\n"); // 去尾部多余空白后补一个
        assert_eq!(normalize_key("   "), ""); // 全空白 -> 空
    }

    #[test]
    fn 存查清私钥走一遍() {
        let mut path = std::env::temp_dir();
        path.push(format!("panel-test-key-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(!key_configured(&path)); // 一开始没配

        write_key_file(&path, "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n").unwrap();
        assert!(key_configured(&path)); // 写完就算配了

        std::fs::remove_file(&path).unwrap();
        assert!(!key_configured(&path)); // 清掉又没配了
    }
}
