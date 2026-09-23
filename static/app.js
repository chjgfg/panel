const $ = id => document.getElementById(id);
const POLL = 3000;
let timer = null, logKey = null, runKey = null;

async function req(path, opts) {
  const r = await fetch(path, { credentials: 'same-origin', ...opts });
  if (r.status === 401) { showLogin(); throw new Error('未登录'); }
  if (!r.ok) throw new Error((await r.text()).trim() || 'HTTP ' + r.status);
  return r;
}

function banner(msg) {
  $('banner').textContent = msg || '';
  $('banner').hidden = !msg;
}

// 弹窗滚动锁：模态弹窗打开时锁掉 body 滚动，否则滚轮在弹窗里滚到头之后
// 事件会冒泡给 body，后面的页面跟着一起滚（滚动穿透）。
// 按当前还有哪个弹窗开着来开关，关一个弹窗不会误解锁另一个还开着的。
// 注：目前所有调用点均已注释停用——移动端 body 锁滚动后 fixed 弹窗会
// 保留滚动位置、头部（关闭按钮 ×）被挤出可视区，看不到 ×；宁可留着
// 轻微滚动穿透，也要保证手机/平板能关掉弹窗。
function syncScrollLock() {
  const open = ['runbox', 'confirmBox', 'logBox', 'srcBox']
    .some(id => !$(id).hidden);
  document.body.style.overflow = open ? 'hidden' : '';
}

function showLogin() {
  clearInterval(timer);
  timer = null;
  logKey = null;
  runKey = null;
  banner('');
  $('logBox').hidden = true;
  $('runbox').hidden = true;
  $('confirmBox').hidden = true;
  closeSrcBox();
  closeTerm();
  $('keyBox').hidden = true;
  // syncScrollLock();
  $('app').hidden = true;
  $('login').hidden = false;
  $('pw').focus();
}

function showApp() {
  $('login').hidden = true;
  $('app').hidden = false;
  refresh();
  timer ||= setInterval(refresh, POLL);
}

function fmtDur(s) {
  if (s == null) return '—';
  s = Math.floor(s);
  const d = s / 86400 | 0, h = s % 86400 / 3600 | 0, m = s % 3600 / 60 | 0;
  return d ? `${d}天 ${h}小时` : h ? `${h}小时 ${m}分` : m ? `${m}分 ${s % 60}秒` : `${s}秒`;
}

function fmtMem(b) {
  if (b == null) return '—';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (b >= 1024 && i < u.length - 1) { b /= 1024; i++; }
  return b.toFixed(i ? 1 : 0) + u[i];
}

// 项目一级行的聚合内存，统一用 MB 展示（需求：内存单位统一为 MB）。
// 聚合值是若干 bin 内存求和，通常已够大，MB 一档正好，不再自动升 GB。
function fmtMemMB(b) {
  if (b == null) return '—';
  return (b / (1024 * 1024)).toFixed(1) + 'MB';
}

// ---------- 整机资源 ----------

function ratio(used, total) {
  return total > 0 ? (used / total) * 100 : null;
}

/// 仪表填充色按严重度走：正常 → 注意 → 危险。
/// 颜色只是辅助，数值本身就在旁边，所以不靠颜色单独传达信息。
function severity(p) {
  return p >= 90 ? 'var(--bad)' : p >= 70 ? 'var(--warn)' : 'var(--accent)';
}

function tile(label, value, sub, p) {
  const d = document.createElement('div');
  d.className = 'tile';
  const b = document.createElement('b');
  b.textContent = label;
  const v = document.createElement('strong');
  v.textContent = value;
  const s = document.createElement('small');
  s.textContent = sub || '';
  d.append(b, v, s);
  if (p != null) {
    const m = document.createElement('div');
    m.className = 'meter';
    m.style.setProperty('--m', severity(p));
    const i = document.createElement('i');
    i.style.width = Math.min(100, Math.max(0, p)) + '%';
    m.appendChild(i);
    d.appendChild(m);
  }
  return d;
}

function renderHost(h) {
  const t = [
    tile('CPU', h.cpu == null ? '—' : h.cpu.toFixed(1) + '%', `${h.cores} 核`, h.cpu),
  ];
  const mp = ratio(h.mem_used, h.mem_total);
  t.push(tile('内存', mp == null ? '—' : mp.toFixed(0) + '%',
    `${fmtMem(h.mem_used)} / ${fmtMem(h.mem_total)}`, mp));
  for (const d of h.disks) {
    const dp = ratio(d.used, d.total);
    t.push(tile('磁盘 ' + d.mount, dp == null ? '—' : dp.toFixed(0) + '%',
      `${fmtMem(d.used)} / ${fmtMem(d.total)}`, dp));
  }
  if (h.swap_total > 0) {
    const sp = ratio(h.swap_used, h.swap_total);
    t.push(tile('Swap', sp.toFixed(0) + '%',
      `${fmtMem(h.swap_used)} / ${fmtMem(h.swap_total)}`, sp));
  }
  t.push(tile('负载', h.load[0].toFixed(2),
    `5 分 ${h.load[1].toFixed(2)} · 15 分 ${h.load[2].toFixed(2)}`));
  t.push(tile('已开机', fmtDur(h.uptime)));
  $('stats').replaceChildren(...t);
}

// 全程用 textContent 建 DOM，不拼 innerHTML，就不用操心转义
function cell(tr, text) {
  const td = document.createElement('td');
  td.textContent = text;
  tr.appendChild(td);
  return td;
}

function btn(ops, label, disabled, onclick, cls) {
  const b = document.createElement('button');
  b.textContent = label;
  b.disabled = disabled;
  b.onclick = onclick;
  if (cls) b.className = cls;
  ops.appendChild(b);
}

let lastList = null;              // 最近一次拿到的项目列表，展开/折叠重绘用
const expanded = new Set();       // 已展开（显示 bin 子行）的项目 key

function render(list) {
  lastList = list;
  const frag = document.createDocumentFragment();
  for (const u of list) {
    const block = document.createElement('div');
    block.className = 'proj-block';

    // 项目行：一张自身的表格，行内容保留表格语义（名称/状态/时长/CPU/内存/操作）。
    // 样式类跟表头表格一致，列宽被 table-layout:fixed 钉死，多行永不错位
    const tbl = document.createElement('table');
    tbl.className = 'stack-table';
    const tb = document.createElement('tbody');
    tb.appendChild(projectRow(u));
    tbl.appendChild(tb);
    block.appendChild(tbl);

    // bin 独立面板：在表格之外、项目行下方；只有展开时显示
    if (!u.external) {
      const panel = document.createElement('div');
      panel.className = 'bin-panel';
      panel.hidden = !expanded.has(u.key);
      panel.appendChild(binPanel(u));
      block.appendChild(panel);
    }
    frag.appendChild(block);
  }
  $('projList').replaceChildren(frag);
}

/// 项目一级的行：名称（带展开箭头）、汇总状态、资源、仅【运行…】和【展开/收起】。
/// 停止/重启/日志都在下方独立的 bin 面板里，不在项目行放，避免「点到项目还是点到 bin」混淆。
function projectRow(u) {
  const running = u.active === 'active';
  const outside = u.outside_pid != null;
  const alive = running || outside;
  const tr = document.createElement('tr');

  const name = cell(tr, '');
  name.className = 'name';
  const expandable = !u.external && (u.instances || []).length > 0;
  if (expandable) {
    const caret = document.createElement('span');
    caret.className = 'caret';
    caret.textContent = expanded.has(u.key) ? '▾' : '▸';
    caret.onclick = toggleExpand;
    name.appendChild(caret);
  }
  name.appendChild(document.createTextNode(u.name));
  const small = document.createElement('small');
  small.classList.add('subline');
  if (outside) {
    small.textContent = `面板外启动 · pid ${u.outside_pid} · 日志看不到`;
  } else if (u.external) {
    small.textContent = u.unit + '（你自己的 service）';
  } else if ((u.running_bins || []).length || u.pid) {
    const parts = [];
    if (u.running_bins && u.running_bins.length) parts.push(`运行 ${u.running_bins.length} 个 bin`);
    if (u.pid) parts.push('pid ' + u.pid);
    small.textContent = parts.join(' · ');
  } else {
    small.textContent = `${(u.bins || []).length} 个可运行程序`;
  }
  name.appendChild(small);

  let statusText;
  if (outside) statusText = '运行中（面板外）';
  else if (!u.loaded) statusText = '未运行';
  else if (running) statusText = '运行中';
  else if (u.active === 'failed') statusText = '失败';
  else statusText = '已停止';
  const st = cell(tr, statusText);
  const dot = document.createElement('span');
  dot.className = 'dot ' + (alive ? 'on' : u.active === 'failed' ? 'bad' : 'off');
  st.prepend(dot);

  cell(tr, fmtDur(u.uptime));
  cell(tr, u.cpu == null ? '—' : u.cpu.toFixed(1) + '%');
  cell(tr, fmtMemMB(u.memory));

  const ops = cell(tr, '');
  ops.className = 'ops';
  if (u.external) {
    // 自写的 unit 没有 bin 面板，对它来说「项目就是这一个 unit」，不存在选择歧义，
    // 启动/停止/重启/日志直接留在项目行
    btn(ops, '启动', false, () => doAction(u, 'start', '启动'), 'go');
    btn(ops, '停止', false, () => doAction(u, 'stop', '停止'), 'stop');
    btn(ops, '重启', false, () => doAction(u, 'restart', '重启'), 'restart');
    btn(ops, '日志', false, () => openLogs(u), 'chip');
  } else {
    // 普通项目：项目顶层只留「展开/收起」和「源码」，启动动作下移到 bin 面板各行
    if (expandable) btn(ops, expanded.has(u.key) ? '收起' : '展开', false, toggleExpand, 'chip');
    btn(ops, '源码', false, () => openSrc(u), 'chip');
  }
  return tr;

  // 展开/收起：切换 key 的展开态并整表重绘
  function toggleExpand() {
    expanded.has(u.key) ? expanded.delete(u.key) : expanded.add(u.key);
    if (lastList) render(lastList);
  }
}

/// bin 独立面板内容：一张自己的表，列 = bin/状态/时长/CPU/内存/操作。
/// 每行一个 bin，带独立的停止/重启/日志，只作用于这一个 bin。
function binPanel(u) {
  const tbl = document.createElement('table');
  const thead = document.createElement('thead');
  const htr = document.createElement('tr');
  for (const t of ['bin', '状态', '运行时长', 'CPU', '内存', '操作']) {
    const th = document.createElement('th');
    th.textContent = t;
    htr.appendChild(th);
  }
  thead.appendChild(htr);
  tbl.appendChild(thead);

  const tb = document.createElement('tbody');
  const insts = u.instances || [];
  if (insts.length === 0) {
    const tr = document.createElement('tr');
    const td = document.createElement('td');
    td.colSpan = 6;
    td.textContent = '这个项目没有可运行的 bin';
    tr.appendChild(td);
    tb.appendChild(tr);
    tbl.appendChild(tb);
    return tbl;
  }
  for (const inst of insts) tb.appendChild(binRow(u, inst));
  tbl.appendChild(tb);
  return tbl;
}

/// bin 行：显示该 bin 自己的状态/资源，操作只动这一个 bin
function binRow(u, inst) {
  const tr = document.createElement('tr');

  const name = cell(tr, '');
  name.className = 'name';
  const nm = document.createElement('span');
  nm.textContent = inst.bin;
  const small = document.createElement('small');
  small.classList.add('subline');
  small.textContent = (
    inst.running ? (inst.pid ? `pid ${inst.pid}` : '运行中') :
    inst.active === 'failed' ? '上次失败' : '未运行'
  );
  name.append(nm, small);

  const st = cell(tr, inst.running ? '运行中' : inst.active === 'failed' ? '失败' : '未运行');
  const dot = document.createElement('span');
  dot.className = 'dot ' + (inst.running ? 'on' : inst.active === 'failed' ? 'bad' : 'off');
  st.prepend(dot);

  cell(tr, fmtDur(inst.uptime));
  cell(tr, inst.cpu == null ? '—' : inst.cpu.toFixed(1) + '%');
  cell(tr, fmtMem(inst.memory));

  const ops = cell(tr, '');
  ops.className = 'ops';
  // 四个按钮永远都在，按运行态做可用性控制：
  //   未运行：启动🉑、停止/重启灰；运行中：启动灰、停止/重启🉑；日志恒🉑
  btn(ops, '启动', inst.running, () => openRun(u, inst.bin), 'go');
  btn(ops, '停止', !inst.running, () => binAction(u, inst, 'stop'), 'stop');
  btn(ops, '重启', !inst.running, () => binAction(u, inst, 'restart'), 'restart');
  btn(ops, '日志', false, () => openLogs(u, inst.bin), 'chip');
  return tr;
}

/// 项目一级的通用操作（外部 unit 的启动/停止/重启），走自定义确认弹窗
function doAction(u, act, label) {
  const tone = act === 'stop' ? 'danger' : act === 'restart' ? 'caution' : '';
  openConfirm(`确认${label}`, `确认${label}该程序「${u.name}」吗？`, async () => {
    try {
      await req(`api/units/${u.key}/${act}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      });
      banner('');
    } catch (e) {
      if (e.message !== '未登录') banner(`${label}「${u.name}」失败: ${e.message}`);
    }
    refresh();
  }, tone);
}

// ---------- 自定义确认弹窗（替换原生 confirm）----------
let confirmCb = null;

/// 打开确认弹窗：设标题/文案，确定时执行 onOk（可 async），执行完自动关。
/// 关闭仅三种：右上角 X、取消按钮、确定执行完成后。
/// tone 决定确定按钮的语义色：danger 红（停止）/ caution 黄（重启）/ 空=默认蓝。
function openConfirm(title, msg, onOk, tone) {
  confirmCb = onOk;
  $('confirmTitle').textContent = title;
  $('confirmMsg').textContent = msg;
  $('confirmOk').className = tone || 'primary';
  $('confirmBox').hidden = false;
  // syncScrollLock();
}
function closeConfirm() {
  confirmCb = null;
  $('confirmBox').hidden = true;
  // syncScrollLock();
}
$('confirmCloseX').onclick = closeConfirm;      // X 不执行操作
$('confirmCancel').onclick = closeConfirm;      // 取消不执行操作
$('confirmOk').onclick = async () => {          // 确定：执行完毕后关闭
  const cb = confirmCb;
  if (!cb) return closeConfirm();
  await cb();
  closeConfirm();
};

/// 单个 bin 的停止/重启：先弹自定义确认，确定后才执行，只动这一个 bin 的 unit
function binAction(u, inst, act) {
  const label = act === 'stop' ? '停止' : '重启';
  const tone = act === 'stop' ? 'danger' : 'caution';
  openConfirm(`确认${label}`, `确认${label}该 bin 程序「${inst.bin}」吗？`, async () => {
    try {
      await req(`api/units/${u.key}/bins/${encodeURIComponent(inst.bin)}/${act}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      });
      banner('');
    } catch (e) {
      if (e.message !== '未登录') banner(`${label}「${inst.bin}」失败: ${e.message}`);
    }
    refresh();
  }, tone);
}

// ---------- 选程序（多选，每行独立参数）+ 填参数 ----------

/// 弹窗一次只针对一个 bin：点哪个 bin 的「启动」就弹哪个，只显示它+参数文本域。
let runBin = null;   // 当前弹窗要运行的 bin

function openRun(u, bin) {
  runKey = u.key;
  runBin = bin;
  // 标题跟随点击的 bin 动态变化；弹窗内部只剩参数文本域
  $('runName').textContent = bin;
  $('runArgs').value = '';
  $('runErr').textContent = '';
  $('runGo').disabled = false;
  $('runbox').hidden = false;
  // syncScrollLock();
  $('runArgs').focus();
}

$('runGo').onclick = async () => {
  if (!runKey || !runBin) return;
  const go = $('runGo');
  go.disabled = true;
  $('runErr').textContent = '';
  try {
    // 只提交当前这个 bin；参数原样透传（空格、换行都不拆）
    await req(`api/units/${runKey}/start`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ bins: [{ bin: runBin, args: $('runArgs').value }] }),
    });
    // 执行完成自动关闭弹窗
    $('runbox').hidden = true;
    // syncScrollLock();
    runKey = null; runBin = null;
  } catch (e) {
    if (e.message !== '未登录') $('runErr').textContent = e.message;
  } finally {
    go.disabled = false;
    refresh();
  }
};

// 关闭方式仅三种：右上角 X、取消按钮、点「运行」执行完成后。遮罩点击不关闭。
function closeRunBox() {
  runKey = null;
  runBin = null;
  $('runbox').hidden = true;
  $('runErr').textContent = '';
  // syncScrollLock();
}
$('closeRunX').onclick = closeRunBox;
$('closeRun').onclick = closeRunBox;

function openLogs(u, preselBin) {
  $('logName').textContent = u.name;
  $('logBox').hidden = false;
  // syncScrollLock();
  logKey = u.key;
  // 筛选项：全部 + 每个 bin。多 bin 同时跑时能只看某一个，或全部按时间混合看
  const sel = $('logFilter');
  sel.replaceChildren();
  const all = document.createElement('option');
  all.value = '';
  all.textContent = '全部';
  sel.appendChild(all);
  for (const b of u.bins || []) {
    const o = document.createElement('option');
    o.value = b;
    o.textContent = b;
    sel.appendChild(o);
  }
  // 从单个 bin 的日志按钮进来时，自动把筛选切到这个 bin
  if (preselBin && u.bins && u.bins.includes(preselBin)) sel.value = preselBin;
  $('logBody').textContent = '加载中…';
  loadLogs(true);
}

let logCache = null;   // 最近一次拉到的 [{bin, log}]，切筛选时不用重拉

async function loadLogs(jump) {
  if (!logKey) return;
  try {
    logCache = await (await req(`api/units/${logKey}/logs?lines=300`)).json();
    renderLogs(jump);
  } catch { /* 401 已由 req() 处理 */ }
}

/// 把拉到的各 bin 日志整到一块：全部模式按行首时间戳混合排序、每条标来源 bin；
/// 单 bin 模式只看那一个。用 DOM 文本节点渲染，日志里再怪的字符也不怕被当成 HTML。
function renderLogs(jump) {
  const filter = $('logFilter').value;
  const p = $('logBody');
  // 保持在底部时才跟随滚动（轮询时定位不会被顶到底部打断）
  const atBottom = p.scrollHeight - p.scrollTop - p.clientHeight < 40;
  p.replaceChildren();
  if (!logCache || logCache.length === 0) {
    p.append('(暂无日志)');
    return;
  }
  // 拆成 (时间戳, 来源, 正文) 三元组
  const rows = [];
  for (const one of logCache) {
    for (const line of (one.log || '').split('\n')) {
      if (line.trim() === '') continue;
      rows.push({ time: line.slice(0, 25), src: one.bin, text: line });
    }
  }
  const keep = filter ? rows.filter(r => r.src === filter) : rows;
  if (keep.length === 0) { p.append('(暂无日志)'); return; }
  // 全部模式按 ISO 时间戳字符串排序（行首都带，字符串序即时间序）；单 bin 模式不用排
  if (!filter) keep.sort((a, b) => (a.time < b.time ? -1 : a.time > b.time ? 1 : 0));
  for (const r of keep) {
    const tag = document.createElement('span');
    tag.className = 'src';
    tag.textContent = `[${r.src}] `;
    p.append(tag, document.createTextNode(r.text + '\n'));
  }
  if (jump || atBottom) p.scrollTop = p.scrollHeight;   // 首开/切筛选直接到底；轮询也跟随
}

async function refresh() {
  if (document.hidden) return;   // 后台标签页不用白跑
  try {
    render(await (await req('api/units')).json());
    $('tick').textContent = '更新于 ' + new Date().toLocaleTimeString('zh-CN', { hour12: false });
    if ($('banner').textContent.startsWith('刷新失败')) banner('');
    await loadLogs(false);
  } catch (e) {
    if (e.message !== '未登录') banner('刷新失败: ' + e.message);
  }
  // 整机资源单独一个 try：读不到（比如不是 Linux）也不该影响项目列表
  try {
    renderHost(await (await req('api/host')).json());
  } catch { /* 401 已由 req() 处理 */ }
}

$('loginForm').onsubmit = async e => {
  e.preventDefault();
  $('loginBtn').disabled = true;
  $('loginErr').textContent = '';
  try {
    const r = await fetch('api/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: $('pw').value })
    });
    if (!r.ok) { $('loginErr').textContent = (await r.text()).trim() || '登录失败'; return; }
    $('pw').value = '';
    showApp();
  } catch {
    $('loginErr').textContent = '连不上服务器';
  } finally {
    $('loginBtn').disabled = false;
  }
};

$('logout').onclick = async () => {
  try { await fetch('api/logout', { method: 'POST', credentials: 'same-origin' }); } catch {}
  showLogin();
};

$('oracleCloud').onclick = () => window.open(
  'https://cloud.oracle.com/compute/instances?region=ap-singapore-2',
  '_blank',
  'noopener,noreferrer'
);
$('ddia').onclick = () => window.open(
  'https://ddia.vonng.com/',
  '_blank',
  'noopener,noreferrer'
);
$('refresh').onclick = () => refresh();

// ---------- 主题切换（黑夜 ↔ 白天）----------
// html.light 这个 class 是唯一真源：CSS 变量全看它。data-theme 只是记录你选了哪档。
function applyTheme() {
  const t = document.documentElement.dataset.theme || 'dark';
  const light = t === 'light';
  document.documentElement.classList.toggle('light', light);
  $('themeBtn').textContent = light ? '主题·白天' : '主题·黑夜';
}
$('themeBtn').onclick = () => {
  const next = (document.documentElement.dataset.theme || 'dark') === 'light' ? 'dark' : 'light';
  document.documentElement.dataset.theme = next;
  localStorage.setItem('theme', next);
  applyTheme();
};
applyTheme();

// ---------- 网页控制台（xterm 终端 + WebSocket，后端在 PTY 里跑 ssh root@本机）----------
// term/fitAddon 懒创建一次后复用；每次打开/重连各建一条 WebSocket。
let term = null, fitAddon = null, termWs = null, termResizeHandler = null, termAuthFail = false;

// SSH 私钥：只存当前浏览器标签的会话（sessionStorage），关标签即失效
const SSH_KEY = 'panel_ssh_key';
const getSshKey = () => sessionStorage.getItem(SSH_KEY) || '';
const setSshKey = v => { v ? sessionStorage.setItem(SSH_KEY, v) : sessionStorage.removeItem(SSH_KEY); };

function updateKeyHint() {
  const has = !!getSshKey();
  const el = $('termKeyHint');
  el.textContent = has ? '已配置私钥（公钥认证）' : '未配置私钥';
  el.classList.toggle('on', has);
}

// WebSocket 地址：从当前页所在目录拼，天然带上秘密前缀；http→ws / https→wss
function termUrl() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const base = location.pathname.replace(/[^/]*$/, '');
  return `${proto}//${location.host}${base}api/terminal`;
}

function ensureTerm() {
  if (term) return;
  term = new Terminal({
    cursorBlink: true,
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
    fontSize: 13,
    theme: { background: '#000000' }
  });
  fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open($('termBody'));
  const enc = new TextEncoder();
  // 击键走二进制帧；缩放走 "R cols rows" 文本帧（后端按帧类型区分）
  term.onData(d => { if (termWs && termWs.readyState === 1) termWs.send(enc.encode(d)); });
  term.onResize(({ cols, rows }) => {
    if (termWs && termWs.readyState === 1) termWs.send('R ' + cols + ' ' + rows);
  });
}

function fitTerm() {
  if (fitAddon) { try { fitAddon.fit(); } catch {} }
}

function connectTerm() {
  termAuthFail = false;
  const ws = new WebSocket(termUrl());
  ws.binaryType = 'arraybuffer';
  termWs = ws;
  ws.onopen = () => {
    // 握手第一帧：初始配置。有私钥就发 "K\n<私钥>"，否则发 "N"（后端据此拼 ssh -i）
    const key = getSshKey();
    ws.send(key ? ('K\n' + key) : 'N');
    fitTerm();
    term.focus();
  };
  ws.onmessage = e => {
    // 二进制帧 = 终端内容；文本帧 = 后端状态消息
    if (typeof e.data !== 'string') { term.write(new Uint8Array(e.data)); return; }
    if (e.data === 'AUTHFAIL') { termAuthFail = true; return; }
    term.write(e.data); // 其它文本（后端错误提示）直接显示到终端
  };
  ws.onclose = () => {
    if (termWs !== ws) return;
    termWs = null;
    if (termAuthFail) {
      term.write('\r\n\x1b[31m[公钥认证失败] 服务器拒绝了这把私钥。请点“配置密钥”检查：'
        + '私钥是否完整、是否与服务器 authorized_keys 里的公钥匹配、是否选对了这台机器的钥匙。\x1b[0m\r\n');
    } else {
      term.write('\r\n\x1b[2m[连接已断开，点“重连”重新连接]\x1b[0m\r\n');
    }
  };
}

function openTerm() {
  $('termTarget').textContent = location.hostname;
  $('termBox').hidden = false;
  updateKeyHint();
  ensureTerm();
  // 先让弹窗渲染出尺寸再 fit + 连接，否则终端行列数算不对
  requestAnimationFrame(() => { fitTerm(); connectTerm(); });
  if (!termResizeHandler) {
    termResizeHandler = () => { if (!$('termBox').hidden) fitTerm(); };
    window.addEventListener('resize', termResizeHandler);
  }
}

function closeTerm() {
  if (termWs) { try { termWs.close(); } catch {} termWs = null; }
  $('termBox').hidden = true;
}

function reconnectTerm() {
  if (termWs) { try { termWs.close(); } catch {} termWs = null; }
  if (term) term.reset();
  requestAnimationFrame(() => { fitTerm(); connectTerm(); });
}

$('console').onclick = openTerm;
$('termClose').onclick = closeTerm;
$('termCloseX').onclick = closeTerm;
$('termReconnect').onclick = reconnectTerm;

// 私钥配置弹窗
$('termKey').onclick = () => {
  $('keyText').value = getSshKey();
  $('keyErr').textContent = '';
  $('keyBox').hidden = false;
  $('keyText').focus();
};
$('keyCloseX').onclick = $('keyCancel').onclick = () => { $('keyBox').hidden = true; };
$('keyClear').onclick = () => { setSshKey(''); $('keyText').value = ''; $('keyErr').textContent = ''; updateKeyHint(); };
$('keySave').onclick = () => {
  const v = $('keyText').value.trim();
  // 轻校验：像不像一把私钥，早点拦下明显贴错的内容
  if (v && !/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----/.test(v)) {
    $('keyErr').textContent = '这看起来不是 SSH 私钥（应包含 “BEGIN ... PRIVATE KEY”）';
    return;
  }
  setSshKey(v);
  updateKeyHint();
  $('keyBox').hidden = true;
  // 用新私钥立刻重连（若控制台已打开）
  if (!$('termBox').hidden) reconnectTerm();
};


// 切换日志源筛选：用上一次拉到的数据直接重渲染，不用重新请求
$('logFilter').onchange = () => { if (logCache) renderLogs(true); };

// 关闭日志弹窗：清空日志内容，下次再点日志按钮重新拉取
function closeLogBox() {
  logKey = null;
  logCache = null;
  $('logBody').replaceChildren();
  $('logBox').hidden = true;
  // syncScrollLock();
}
$('logCloseX').onclick = closeLogBox;
$('logClose').onclick = closeLogBox;

// ---------- 查看源码 ----------

let srcKey = null;        // 当前弹窗对应的项目 key
let srcCache = null;      // 最近一次拉到的目录树，切文件不用重拉
let srcCur = null;        // 当前打开的文件节点

function openSrc(u) {
  srcKey = u.key;
  srcCache = null;
  srcCur = null;
  $('srcName').textContent = u.name;
  $('srcPath').textContent = '';
  $('srcBody').textContent = '';
  srcMsg('');
  $('srcBox').hidden = false;
  // syncScrollLock();
  loadTree();
}

async function loadTree() {
  if (!srcKey) return;
  $('srcTree').replaceChildren();
  const tip = document.createElement('div');
  tip.textContent = '加载中…';
  tip.className = 'src-path';
  $('srcTree').appendChild(tip);
  try {
    srcCache = await (await req(`api/units/${srcKey}/tree`)).json();
    renderTree();
  } catch (e) {
    if (e.message !== '未登录') {
      $('srcTree').replaceChildren();
      srcMsg('拉目录失败: ' + e.message, false);
    }
  }
}

/// 目录树渲染。目录点名字展开/收起（纯前端折叠，不发请求），
/// 文件点名字右侧显示内容。全程 textContent/DOM 建，不怕文件名里有怪字符。
function renderTree() {
  const tree = $('srcTree');
  tree.replaceChildren();
  if (!srcCache) return;
  // 全部收起：大项目一口气全展开会渲染几千个节点，慢；点哪个目录再展开哪个
  tree.appendChild(treeNodes(srcCache.children, false));
}

function treeNodes(nodes, expanded) {
  const wrap = document.createElement('div');
  wrap.className = 'kids';
  for (const n of nodes) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'node';
    row.textContent = n.name;
    if (n.dir) {
      const tw = document.createElement('span');
      tw.className = 'tw';
      tw.textContent = expanded ? '▾' : '▸';
      row.prepend(tw);
      let open = expanded;
      let kids = null;
      row.onclick = () => {
        open = !open;
        tw.textContent = open ? '▾' : '▸';
        // children 被 skip_serializing_if 省掉时当作空目录处理
        if (open && !kids && (n.children || []).length) {
          kids = treeNodes(n.children, false);
          row.after(kids);
        } else if (kids) {
          kids.hidden = !open;
        }
      };
      if (expanded && (n.children || []).length) {
        kids = treeNodes(n.children, false);
      }
      wrap.appendChild(row);
      if (kids) wrap.appendChild(kids);
    } else {
      row.onclick = () => showFile(n, row);
      wrap.appendChild(row);
    }
  }
  return wrap;
}

/// 文件内容前端缓存：树接口只给结构，点开文件时走 /file 拉内容。
/// 同一次打开源码弹窗期间，来回切文件不用重复请求。
const fileCache = new Map();

/* ---------- 语法高亮 ----------
   用 highlight.js(本地 vendor,35+ 门语言,别名的扩展名它自己认)。
   它输出的 HTML 已转义过,直接塞 innerHTML 安全。 */

/// 后缀 → highlight.js 语言名。没列出的后缀交 hljs.getLanguage 兜底,
/// 再不行就纯文本不上色。
const LANG_BY_EXT = {
  rs: 'rust', py: 'python', pyw: 'python',
  sh: 'bash', bash: 'bash', zsh: 'bash',
  js: 'javascript', mjs: 'javascript', cjs: 'javascript',
  ts: 'typescript', tsx: 'typescript', jsx: 'typescript',
  toml: 'toml', json: 'json', yaml: 'yaml', yml: 'yaml',
  html: 'xml', htm: 'xml', xml: 'xml', svg: 'xml',
  css: 'css', scss: 'scss', less: 'less',
  md: 'markdown', markdown: 'markdown',
  c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', hpp: 'cpp', cxx: 'cpp',
  cs: 'csharp', go: 'go', java: 'java', kt: 'kotlin', swift: 'swift',
  php: 'php', rb: 'ruby', lua: 'lua', pl: 'perl', r: 'r',
  sql: 'sql', graphql: 'graphql', gql: 'graphql',
  mk: 'makefile', makefile: 'makefile',
  diff: 'diff', patch: 'diff', wasm: 'wasm',
  /* env/ini 系:key=value 结构,ini 语法正好;gitignore 走 bash 的
     「# 注释 + 路径」习惯高亮 */
  env: 'ini', ini: 'ini',
};

/// 完整文件名(小写) → 语言。给 Makefile/Dockerfile 这种没有扩展名、
/// 或扩展名不代表语言(makefile)的文件。
const LANG_BY_NAME = {
  'makefile': 'makefile', 'gnumakefile': 'makefile',
  'dockerfile': 'bash', 'dockerfile.dev': 'bash',
  '.gitignore': 'bash', '.dockerignore': 'bash',
  '.env': 'ini', '.env.local': 'ini', '.env.example': 'ini',
  '.gitattributes': 'ini',
};

/// 各语言的徽标:名字 + 社区品牌色。
/// 未知语言没有徽标,不显示。
const LANG_TAG = {
  rust:    ['Rust', '#e0af68'],
  python:  ['Python', '#e8c268'],
  bash:    ['Shell', '#9ece6a'],
  javascript: ['JS', '#e8c170'],
  typescript: ['TS', '#7aa2f7'],
  toml:    ['TOML', '#7aa2f7'],
  json:    ['JSON', '#2ac3de'],
  xml:     ['HTML', '#f7768e'],
  css:     ['CSS', '#bb9af7'],
  scss:    ['SCSS', '#f7768e'],
  yaml:    ['YAML', '#9ece6a'],
  markdown: ['MD', '#c0caf5'],
  c:       ['C', '#7aa2f7'],
  cpp:     ['C++', '#7aa2f7'],
  csharp:  ['C#', '#9ece6a'],
  go:      ['Go', '#2ac3de'],
  java:    ['Java', '#e8c170'],
  kotlin:  ['Kotlin', '#f7768e'],
  swift:   ['Swift', '#f7768e'],
  php:     ['PHP', '#7aa2f7'],
  ruby:    ['Ruby', '#f7768e'],
  lua:     ['Lua', '#7aa2f7'],
  perl:    ['Perl', '#2ac3de'],
  sql:     ['SQL', '#e8c170'],
  graphql: ['GraphQL', '#f7768e'],
  makefile: ['Make', '#e8c170'],
  diff:    ['Diff', '#f7768e'],
  ini:     ['ENV', '#9ece6a'],
};

/// 按路径挑语言:先看完整文件名表(Makefile/.env 这类),再按扩展名,
/// 表里没有的问 hljs(它自己认 ps1 等一堆别名),还不认识就 null。
function langOf(path) {
  const name = path.slice(path.lastIndexOf('/') + 1).toLowerCase();
  if (LANG_BY_NAME[name]) return LANG_BY_NAME[name];
  const m = /\.([^.]+)$/.exec(name);
  if (!m) return null;
  const ext = m[1];
  return LANG_BY_EXT[ext] || (hljs.getLanguage(ext) ? ext : null);
}

/// 渲染整个文件:hljs 高亮过的 HTML 按行拆开,每行套一个 .cl div
/// (CSS 计数器出行号)。多行 token(块注释/模板字符串)拆开后标签会断,
/// 所以逐行高亮——每行独立走一遍 hljs,断不了。
/// 行号列宽度按总行数位数定(几个字符几 ch),和文件规模匹配。
function renderCode(text, lang) {
  const lines = text.split('\n');
  $('srcBody').style.setProperty('--ln-w', Math.max(3, String(lines.length).length) + 'ch');
  const opt = lang ? { language: lang, ignoreIllegals: true } : undefined;
  return lines.map(l => {
    // 没认出语言就纯转义文本,别交给 hljs 猜(猜错还不如纯色)
    const inner = lang
      ? hljs.highlight(l, opt).value
      : l.replace(/[&<>]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
    return `<div class="cl"><code>${inner}</code></div>`;
  }).join('');
}

/// 语言徽标上色:品牌色文字 + 微透底
function setLangTag(lang) {
  const tag = $('langTag');
  if (!lang || !LANG_TAG[lang]) { tag.hidden = true; return; }
  const [name, fg] = LANG_TAG[lang];
  tag.textContent = name;
  tag.style.color = fg;
  tag.style.background = `color-mix(in oklab, ${fg} 16%, transparent)`;
  tag.hidden = false;
}

async function showFile(n, row) {
  srcCur = n;
  $('srcTree').querySelectorAll('.node.cur').forEach(el => el.classList.remove('cur'));
  if (row) row.classList.add('cur');
  $('srcPath').textContent = n.path;
  const lang = langOf(n.path);
  setLangTag(lang);
  if (n.big) {
    $('srcBody').replaceChildren();
    $('srcBody').textContent = '(超过 512KB,不展示内容)';
    return;
  }
  if (fileCache.has(n.path)) {
    const c = fileCache.get(n.path);
    $('srcBody').replaceChildren();
    if (c == null) {
      setLangTag(null);
      $('srcBody').textContent = '(二进制文件,不展示内容)';
    } else {
      $('srcBody').innerHTML = renderCode(c, lang);
    }
    return;
  }
  $('srcBody').replaceChildren();
  $('srcBody').textContent = '加载中…';
  try {
    const body = await (await req(`api/units/${srcKey}/file?path=${encodeURIComponent(n.path)}`)).json();
    fileCache.set(n.path, body.content);
    if (srcCur !== n) return;   // 等待期间用户又点了别的文件，别覆盖
    $('srcBody').replaceChildren();
    if (body.content == null) {
      setLangTag(null);
      $('srcBody').textContent = '(二进制文件,不展示内容)';
    } else {
      $('srcBody').innerHTML = renderCode(body.content, lang);
    }
  } catch (e) {
    if (e.message !== '未登录') srcMsg('读文件失败: ' + e.message, false);
  }
}

/// 结果提示条:ok=true 绿色成功,false 红色失败;几秒后自动消失
let srcMsgTimer = null;
function srcMsg(text, ok) {
  const m = $('srcMsg');
  clearTimeout(srcMsgTimer);
  m.textContent = text || '';
  m.hidden = !text;
  m.className = 'src-msg' + (text ? (ok ? ' ok' : ' bad') : '');
  if (text) srcMsgTimer = setTimeout(() => { m.hidden = true; }, 5000);
}

$('srcPull').onclick = async () => {
  if (!srcKey) return;
  const b = $('srcPull');
  b.disabled = true;
  srcMsg('正在 git pull…', true);
  try {
    const r = await req(`api/units/${srcKey}/pull`);
    srcMsg('拉取成功', true);
    await loadTree();   // 代码变了,目录树重新拉一遍
  } catch (e) {
    if (e.message !== '未登录') srcMsg('拉取失败: ' + e.message, false);
  } finally {
    b.disabled = false;
  }
};

/// 复制文本。navigator.clipboard 只在安全上下文(HTTPS/localhost)可用，
/// 面板是裸 HTTP 访问的，所以要退回 execCommand 这条老路：把文本塞进一个
/// 临时 textarea，选中后让浏览器执行复制，完事把节点删掉。
function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) {
    return navigator.clipboard.writeText(text);
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  // 移出可视区但不 display:none —— display:none 的元素选不中内容
  ta.style.position = 'fixed';
  ta.style.left = '-9999px';
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  try {
    const ok = document.execCommand('copy');
    ta.remove();
    return ok ? Promise.resolve() : Promise.reject(new Error('execCommand 返回失败'));
  } catch (e) {
    ta.remove();
    return Promise.reject(e);
  }
}

$('srcCopy').onclick = async () => {
  if (!srcCur) return srcMsg('先在左边选一个文件', false);
  const content = fileCache.get(srcCur.path);
  if (content == null) return srcMsg('这个文件没有可复制的内容', false);
  try {
    await copyText(content);
    srcMsg(`已复制 ${srcCur.path}`, true);
  } catch {
    srcMsg('复制失败：浏览器拒绝了复制操作', false);
  }
};

function closeSrcBox() {
  srcKey = null;
  srcCache = null;
  srcCur = null;
  fileCache.clear();
  $('srcTree').replaceChildren();
  $('srcBody').textContent = '';
  $('srcPath').textContent = '';
  $('srcMsg').hidden = true;
  $('srcBox').hidden = true;
  // syncScrollLock();
}
$('srcCloseX').onclick = closeSrcBox;

// ---------- 目录树的拖拽调宽 + 显示/隐藏 ----------

const TREE_MIN = 140, TREE_MAX = 480;   // 可拖的宽度范围（px）

/// 拖拽条:mousedown 记下起点,mousemove 实时改树的宽度,松开时存进 localStorage,
/// 下次打开弹窗还是你上次调好的宽度。
(() => {
  const grip = $('srcTreeGrip'), tree = $('srcTree'), layout = document.querySelector('.src-layout');
  let dragging = false, startX = 0, startW = 0;
  grip.addEventListener('mousedown', e => {
    dragging = true;
    startX = e.clientX;
    startW = tree.getBoundingClientRect().width;
    grip.classList.add('on');
    document.body.style.cursor = 'col-resize';
    // 拖拽期间全局禁掉文本选择,免得拖着拖着把代码选得满屏都是
    document.body.style.userSelect = 'none';
    e.preventDefault();
  });
  document.addEventListener('mousemove', e => {
    if (!dragging) return;
    const w = Math.min(TREE_MAX, Math.max(TREE_MIN, startW + e.clientX - startX));
    tree.style.width = w + 'px';
  });
  document.addEventListener('mouseup', () => {
    if (!dragging) return;
    dragging = false;
    grip.classList.remove('on');
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
    localStorage.setItem('srcTreeW', tree.getBoundingClientRect().width);
  });
  // 打开时恢复上次宽度;没存过就用 CSS 里的默认值
  const saved = +localStorage.getItem('srcTreeW');
  if (saved >= TREE_MIN && saved <= TREE_MAX) tree.style.width = saved + 'px';
})();

/// 显示/隐藏目录树:隐藏后代码区占满整个弹窗。状态也记进 localStorage。
$('srcTreeToggle').onclick = () => {
  const layout = document.querySelector('.src-layout');
  const hidden = layout.classList.toggle('notree');
  $('srcTreeToggle').textContent = hidden ? '显示目录' : '隐藏目录';
  localStorage.setItem('srcTreeHidden', hidden ? '1' : '');
};
(() => {
  // 打开弹窗时应用上次的选择(写在 IIFE 里,打开时不闪一下树再消失)
  if (localStorage.getItem('srcTreeHidden')) {
    document.querySelector('.src-layout').classList.add('notree');
    $('srcTreeToggle').textContent = '显示目录';
  }
})();

document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });

fetch('api/me', { credentials: 'same-origin' })
  .then(r => r.ok ? showApp() : showLogin())
  .catch(showLogin);
