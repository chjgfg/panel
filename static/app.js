const $ = id => document.getElementById(id);
const POLL = 3000;
let timer = null, logKey = null, runKey = null;

async function req(path, opts) {
  const r = await fetch(path, { credentials: 'same-origin', ...opts });
  if (r.status === 401) { showLogin(); throw new Error('未登录'); }
  if (!r.ok) throw new Error((await r.text()).trim() || 'HTTP ' + r.status);
  return r;
}

// 顶部横幅：默认报错（红），ok=true 报成功（绿）。
function banner(msg, ok) {
  $('banner').textContent = msg || '';
  $('banner').hidden = !msg;
  $('banner').classList.toggle('ok', !!ok);
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
  $('auditBox').hidden = true;
  // syncScrollLock();
  $('app').hidden = true;
  $('login').hidden = false;
  $('pw').focus();
}

function showApp() {
  $('login').hidden = true;
  $('app').hidden = false;
  showSkeleton();
  refresh();
  loadVersion();
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
    // --m 设在卡上而不是仪表上：数值文字要跟仪表同色（自定义属性往下继承）
    d.style.setProperty('--m', severity(p));
    if (p >= 90) d.classList.add('hi');   // 顶到上限的卡整张描边转红
    const m = document.createElement('div');
    m.className = 'meter';
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
  return b;   // 回传：点下去要临时禁用自己的按钮（如拉取代码）用得上
}

/// 操作组之间的发丝分隔：把「看」和「动它」两组按钮分开，
/// 比给两组按钮配不同颜色克制，又不会六个按钮糊成一排。
function opsGap(ops) {
  const s = document.createElement('span');
  s.className = 'ops-gap';
  ops.appendChild(s);
}

let lastList = null;              // 最近一次拿到的项目列表，展开/折叠重绘用
const expanded = new Set();       // 已展开（显示 bin 子行）的项目 key

// 每个项目一块常驻 DOM，轮询只改文字、不换节点。
// 以前每 3 秒 replaceChildren 整表重建，代价有三个，全都表现为「一跳一跳」：
//   1) 浏览器不把 :hover 补给新节点（要等鼠标真的动一下）——鼠标停在行上或
//      「展开/收起」按钮上，高亮每 3 秒丢一次；
//   2) bin 面板的滚动位置被清零；
//   3) CSS 动画从头重播。
// 复用节点把这三条一次性解决，也是以后想给列表加任何动效的前提。
const views = new Map();   // 项目 key -> view
let emptyNode = null;      // 列表为空时的引导条，只在这时候挂在容器上

// ---------- 列表的筛选、排序、展开记忆、趋势线 ----------

// 这三样都记进 localStorage：面板天天开，不该每次重新筛一遍
const filters = {
  name: localStorage.getItem('fltName') || '',
  state: localStorage.getItem('fltState') || '',
  sort: localStorage.getItem('sortBy') || 'name',
};

/// 展开状态以前只在内存里，刷新一次就得重新点一遍
function saveExpanded() {
  localStorage.setItem('expanded', JSON.stringify([...expanded]));
}
try {
  for (const k of JSON.parse(localStorage.getItem('expanded') || '[]')) expanded.add(k);
} catch { /* 存坏了就当没记过 */ }

// 每个项目最近若干次的 CPU / 内存采样，只画趋势用（内存里存，刷新重来）。
// 40 个点 × 3 秒 ≈ 两分钟窗口，够看出「在涨」。
const HIST = 40;
const hist = new Map();

// 一次轮询只采一个点。render() 不只轮询会调（筛选、展开都调），
// 不设这道闸的话：连着敲八个字筛项目，趋势线就被八根重复采样拉平了。
let lastSample = 0;

function pushHist(list) {
  const t = Date.now();
  if (t - lastSample < POLL - 300) return;
  lastSample = t;
  const sample = (k, cpu, mem) => {
    let h = hist.get(k);
    if (!h) { h = { cpu: [], mem: [] }; hist.set(k, h); }
    h.cpu.push(cpu || 0);
    h.mem.push(mem || 0);
    if (h.cpu.length > HIST) { h.cpu.shift(); h.mem.shift(); }
  };
  for (const u of list) {
    sample(u.key, u.cpu, u.memory);
    // bin 一级也留历史：子列表的趋势线要和父行同一套读法
    for (const inst of u.instances || []) sample(u.key + '/' + inst.bin, inst.cpu, inst.memory);
  }
}

/// 首字母徽标的字：目录名只能含 ASCII 字母数字和 ._ -（后端 ok_name 卡过），
/// 所以按分隔符切词取头两个首字母，单词就取头两个字母
function initials(name) {
  const segs = name.split(/[-_.\s]+/).filter(Boolean);
  const s = segs.length >= 2 ? segs[0][0] + segs[1][0] : name.slice(0, 2);
  return s.toUpperCase();
}

/// 名字哈希 → 固定色相。只从六个档里取，不是 0~360 随便来：
/// 色相散开时一屏看着像调色盘，收在六档才像同一套系统里的项目
const AV_HUES = [212, 158, 42, 344, 268, 24];
function hueFor(name) {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
  return AV_HUES[h % AV_HUES.length];
}

/// 迷你折线。用 SVG 不用 canvas：不必管设备像素比，也不会因重绘闪一下。
/// 走 createElementNS 而不是拼 innerHTML —— 这页其它地方全程 textContent，
/// 不给注入留口子（这里虽然只有数字，但坏了惯例更糟）
let sparkSeq = 0;   // 每张图的渐变 id（同页多张，不能重名）

function spark(vals, cls) {
  // 全 0 不画：没跑着的项目攒一整条底线，看着像「趋势平稳」，其实是根本没数据
  if (!vals || vals.length < 2 || vals.every(v => !v)) return null;
  const w = 30, h = 11, ns = 'http://www.w3.org/2000/svg';
  const mk = t => document.createElementNS(ns, t);
  const max = Math.max(...vals, 1);
  const svg = mk('svg');
  svg.setAttribute('viewBox', `0 0 ${w} ${h}`);
  svg.setAttribute('width', w);
  svg.setAttribute('height', h);
  svg.setAttribute('class', 'spark ' + cls);
  // 一根线两种颜色靠一个变量带走：折线、渐变两端都读它，不用为 cpu/内存各写一套规则
  svg.style.setProperty('--spk', cls === 'cpu' ? 'var(--warn)' : 'var(--accent)');
  const pts = vals.map((v, i) =>
    `${(i * w / (vals.length - 1)).toFixed(1)},${(h - 0.5 - v / max * (h - 1)).toFixed(1)}`);
  // 面积层用自上而下的渐隐，而不是平涂一块：只有两三个点时折线几乎是平的，
  // 平涂会把整条 30×11 糊成一块实心色块（上一版就是这个样子）
  const gid = 'spk' + (++sparkSeq);
  const grad = mk('linearGradient');
  grad.setAttribute('id', gid);
  grad.setAttribute('x1', '0'); grad.setAttribute('y1', '0');
  grad.setAttribute('x2', '0'); grad.setAttribute('y2', '1');
  for (const [off, op] of [['0', '.38'], ['1', '0']]) {
    const st = mk('stop');
    st.setAttribute('offset', off);
    st.style.stopColor = 'var(--spk)';
    st.style.stopOpacity = op;
    grad.appendChild(st);
  }
  const defs = mk('defs');
  defs.appendChild(grad);
  // 首尾两点本来就在 x=0 和 x=w，补两个底角即闭合
  const area = mk('polygon');
  area.setAttribute('points', `0,${h} ${pts.join(' ')} ${w},${h}`);
  area.setAttribute('fill', `url(#${gid})`);
  const pl = mk('polyline');
  pl.setAttribute('points', pts.join(' '));
  // 悬停读数：30px 的线只能看出个方向，「峰值多少、现在多少」得靠这行字
  const fmt = cls === 'cpu' ? x => x.toFixed(1) + '%' : x => fmtMemMB(x);
  const ti = mk('title');
  ti.textContent = `${cls === 'cpu' ? 'CPU' : '内存'} 最近 ${vals.length} 次采样：`
    + `现值 ${fmt(vals[vals.length - 1])}，峰值 ${fmt(Math.max(...vals))}，`
    + `谷值 ${fmt(Math.min(...vals))}`;
  svg.append(defs, area, pl, ti);
  return svg;
}

/// 一格「数字 + 迷你趋势线」：文本节点常驻不动，只换那根 SVG。
/// 父行和 bin 行是同一种格子，共用一份换法
function metric(cell, text, slot, vals, cls, txt) {
  text.nodeValue = txt;
  if (slot) slot.remove();
  const s = spark(vals, cls);
  if (s) cell.appendChild(s);
  return s;
}

/// 按当前的筛选/排序条件挑出要显示的项目。排序键取负数是为了「大的在前」
function visible(list) {
  const q = filters.name.trim().toLowerCase();
  const out = list.filter(u => {
    if (q && !u.name.toLowerCase().includes(q)) return false;
    const on = u.active === 'active' || u.outside_pid != null;
    if (filters.state === 'on' && !on) return false;
    if (filters.state === 'off' && on) return false;
    if (filters.state === 'failed' && u.active !== 'failed') return false;
    return true;
  });
  const k = { name: u => u.name, mem: u => u.memory || 0, cpu: u => u.cpu || 0, up: u => u.uptime || 0 }[filters.sort]
    || (u => u.name);
  if (filters.sort === 'name') out.sort((a, b) => k(a).localeCompare(k(b)));
  else out.sort((a, b) => k(b) - k(a));
  return out;
}

// 控件初值 = 上次存的选择。在第一次 render 之前挂好，首屏直接是你上次看的那副样子
$('fltName').value = filters.name;
$('fltState').value = filters.state;
$('sortBy').value = filters.sort;
function onFilterChange() {
  filters.name = $('fltName').value;
  filters.state = $('fltState').value;
  filters.sort = $('sortBy').value;
  localStorage.setItem('fltName', filters.name);
  localStorage.setItem('fltState', filters.state);
  localStorage.setItem('sortBy', filters.sort);
  // 重排全靠复用节点做，打字筛的时候不会闪
  if (lastList) render(lastList);
}
$('fltName').oninput = onFilterChange;
$('fltState').onchange = onFilterChange;
$('sortBy').onchange = onFilterChange;

let booted = false;   // 第一批数据到手没有（首屏骨架行用）

/// 第一次拉列表在单核机器上要一两秒，这期间页面只有右上角一个小点在呼吸，
/// 看着像没加载出来。先垫三行灰条占住位置，数据一到整片换掉。
function showSkeleton() {
  if (booted) return;
  const holder = $('projList');
  holder.replaceChildren();
  for (let i = 0; i < 3; i++) {
    const row = document.createElement('div');
    row.className = 'sk';
    // 六根灰条按真列宽（24/15/13/7/10/31）摆，数据一到换成真行时位置不跳
    for (const w of ['24%', '15%', '13%', '7%', '10%', '31%']) {
      const bar = document.createElement('i');
      bar.style.width = w;
      row.appendChild(bar);
    }
    holder.appendChild(row);
  }
}

function render(list) {
  lastList = list;
  pushHist(list);
  const shown = visible(list);
  const holder = $('projList');
  // 首屏：把骨架行清掉再放真行。此刻 views 还是空的，清空不会误删真节点
  if (!booted) { booted = true; holder.replaceChildren(); }
  $('listCount').textContent = shown.length === list.length
    ? `${list.length} 个项目`
    : `显示 ${shown.length} / 共 ${list.length}`;
  if (emptyNode && shown.length) { emptyNode.remove(); emptyNode = null; }
  const seen = new Set();
  let i = 0;
  for (const u of shown) {
    seen.add(u.key);
    let v = views.get(u.key);
    // 外部 unit 与否决定操作列的按钮构成，变了只能重建（极少发生）
    if (v && v.external !== !!u.external) { v.block.remove(); views.delete(u.key); v = null; }
    if (!v) { v = buildView(u); views.set(u.key, v); }
    syncView(v, u);
    const at = holder.children[i];          // 顺序对齐：新插的项目落在该在的位置
    if (at !== v.block) holder.insertBefore(v.block, at || null);
    i++;
  }
  for (const [k, v] of [...views]) if (!seen.has(k)) { v.block.remove(); views.delete(k); }
  if (!shown.length && !emptyNode) {
    // 新机器上打开面板是一片空白，看着像坏了：说清去哪儿加项目
    emptyNode = document.createElement('div');
    emptyNode.className = 'empty';
    emptyNode.textContent = list.length
      ? '没有符合当前筛选条件的项目'
      : '扫描目录里还没有项目：往 panel.toml 的 dirs 指的目录里放（或 git clone）一个子目录，刷新就有';
  }
  if (emptyNode) holder.appendChild(emptyNode);
}

/// 展开/收起：只改展开态再走一次 render（节点复用，高度过渡由 CSS 接手）
function toggleExpand(key) {
  const opening = !expanded.has(key);
  opening ? expanded.add(key) : expanded.delete(key);
  saveExpanded();
  if (lastList) render(lastList);
}

/// 状态格：胶囊 + 圆点。返回引用，档位和文字每轮就地改
function badgeCell(tr) {
  const td = cell(tr, '');
  const badge = document.createElement('span');
  badge.className = 'badge off';
  const dot = document.createElement('i');
  dot.className = 'dot';
  const text = document.createTextNode('');
  badge.append(dot, text);
  td.appendChild(badge);
  return { td, badge, text };
}

/// 建一个项目的骨架 DOM，并把每轮要改的节点存进 view 备用
function buildView(u) {
  const v = { u, key: u.key, external: !!u.external, binRows: new Map(), btn: {} };
  v.block = document.createElement('div');
  v.block.className = 'proj-block';
  v.block.dataset.key = u.key;

  // 项目行：一张自身的表格，列宽被 table-layout:fixed 钉死，跟表头表格永不错位
  const tbl = document.createElement('table');
  tbl.className = 'stack-table';
  const tb = document.createElement('tbody');
  v.tr = document.createElement('tr');
  tb.appendChild(v.tr);
  tbl.appendChild(tb);
  v.block.appendChild(tbl);

  v.name = cell(v.tr, '');
  v.name.className = 'name';
  v.caret = document.createElement('span');
  v.caret.className = 'caret';
  v.caret.onclick = () => toggleExpand(u.key);
  // 徽标的字和色都由项目名决定，而 key 就是目录名 —— 一辈子不变，建一次就够
  v.av = document.createElement('span');
  v.av.className = 'avatar';
  v.av.textContent = initials(u.name);
  v.av.style.setProperty('--av', hueFor(u.name));
  v.nameText = document.createTextNode('');
  v.sub = document.createElement('small');
  v.sub.className = 'subline';
  v.name.append(v.caret, v.av, v.nameText, v.sub);

  const b = badgeCell(v.tr);
  v.badge = b.badge;
  v.badgeText = b.text;
  v.dur = cell(v.tr, '');
  // CPU / 内存两格：数字后面挂一条迷你趋势线。挂在同一格里是为了不改行高 ——
  // 单独开一列会把刚调好的六列宽度全部推翻
  v.cpuText = document.createTextNode('');
  v.memText = document.createTextNode('');
  v.cpu = cell(v.tr, '');
  v.cpu.appendChild(v.cpuText);
  v.mem = cell(v.tr, '');
  v.mem.appendChild(v.memText);

  v.ops = cell(v.tr, '');
  v.ops.className = 'ops';
  if (v.external) {
    // 自写的 unit 没有 bin 面板，「项目就是这一个 unit」，启动/停止/重启/日志留在项目行
    v.btn.start = btn(v.ops, '启动', false, () => doAction(v.u, 'start', '启动'), 'go');
    v.btn.stop = btn(v.ops, '停止', false, () => doAction(v.u, 'stop', '停止'), 'stop');
    v.btn.restart = btn(v.ops, '重启', false, () => doAction(v.u, 'restart', '重启'), 'restart');
    v.btn.log = btn(v.ops, '日志', false, () => openLogs(v.u), 'chip');
  } else {
    // 普通项目：顶层只留「展开/收起」和「源码」，启动动作下移到 bin 面板各行
    v.btn.expand = btn(v.ops, '展开', false, () => toggleExpand(u.key), 'chip');
    v.btn.src = btn(v.ops, '源码', false, () => openSrc(v.u), 'chip');
  }
  // 项目级的两个快捷入口，两种项目都有。处理器一律读 v.u：节点要长期复用，
  // 闭包不能锁死在第一次那个 u 上
  opsGap(v.ops);
  v.btn.pull = btn(v.ops, '拉取代码', false, () => rowPull(v.u, v.btn.pull), 'chip');
  v.btn.term = btn(v.ops, '控制台', false, () => openTerm(v.u), 'chip');

  if (!v.external) buildPanel(v);
  return v;
}

/// 把一份新的项目数据就地刷进已存在的 view：只改文字和状态，不换节点。
function syncView(v, u) {
  v.u = u;
  const running = u.active === 'active';
  const outside = u.outside_pid != null;
  const alive = running || outside;
  const expandable = !u.external && (u.instances || []).length > 0;
  const open = expanded.has(u.key);

  v.caret.hidden = !expandable;
  v.caret.textContent = open ? '▾' : '▸';
  v.nameText.nodeValue = u.name;
  if (outside) {
    v.sub.textContent = `面板外启动 · pid ${u.outside_pid} · 日志看不到`;
  } else if (u.external) {
    v.sub.textContent = u.unit + '（你自己的 service）';
  } else if ((u.running_bins || []).length || u.pid) {
    const parts = [];
    if (u.running_bins && u.running_bins.length) parts.push(`运行 ${u.running_bins.length} 个 bin`);
    if (u.pid) parts.push('pid ' + u.pid);
    v.sub.textContent = parts.join(' · ');
  } else {
    v.sub.textContent = `${(u.bins || []).length} 个可运行程序`;
  }
  // 失败时把 systemd 给的原因（Result + 退出码）直接缀在后面：以前只有一个
  // 「失败」，为什么挂的必须点进日志翻到底才知道
  if (u.fail) v.sub.textContent = v.sub.textContent ? `${v.sub.textContent} · ${u.fail}` : u.fail;

  let statusText;
  if (outside) statusText = '运行中（面板外）';
  else if (!u.loaded) statusText = '未运行';
  else if (running) statusText = '运行中';
  else if (u.active === 'failed') statusText = '失败';
  else statusText = '已停止';
  v.badge.className = 'badge ' + (alive ? 'on' : u.active === 'failed' ? 'bad' : 'off');
  v.badgeText.nodeValue = statusText;
  // 行左缘那条状态色带的档位（CSS 里的 .st-*）。同值不重设：class 一变，
  // box-shadow 过渡就重播，每 3 秒轮询会让它一直闪
  const st = alive ? 'st-on' : u.active === 'failed' ? 'st-bad' : 'st-off';
  if (v.tr.className !== st) v.tr.className = st;

  v.dur.textContent = fmtDur(u.uptime);
  // 趋势线要攒够两个点才画（一条横线没有信息量）
  const h = hist.get(u.key);
  v.cpuSvg = metric(v.cpu, v.cpuText, v.cpuSvg, h && h.cpu, 'cpu',
    u.cpu == null ? '—' : u.cpu.toFixed(1) + '%');
  v.memSvg = metric(v.mem, v.memText, v.memSvg, h && h.mem, 'mem', fmtMemMB(u.memory));

  if (v.btn.expand) {
    v.btn.expand.hidden = !expandable;
    v.btn.expand.textContent = open ? '收起' : '展开';
  }
  if (!v.panel) return;
  // 只在展开态真的变了时才动 hidden：重设同一个值虽不改样式，但这里谨慎为上——
  // display 一变 CSS 动画就重播，轮询不该碰到它
  const wantHidden = !open;
  if (v.panel.hidden !== wantHidden) v.panel.hidden = wantHidden;
  syncBinRows(v, u);
}

/// bin 面板：一张自己的表（列 = bin/状态/时长/CPU/内存/操作），行按 bin 名复用。
/// 折叠就是 hidden=true；节点复用之后，轮询不会反复翻这个标志，动画也不会重播
function buildPanel(v) {
  v.panel = document.createElement('div');
  v.panel.className = 'bin-panel';
  v.panel.hidden = true;
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
  v.binBody = document.createElement('tbody');
  v.binEmpty = document.createElement('tr');
  const td = document.createElement('td');
  td.colSpan = 6;
  td.textContent = '这个项目没有可运行的 bin';
  v.binEmpty.appendChild(td);
  v.binBody.appendChild(v.binEmpty);
  tbl.appendChild(v.binBody);
  v.panel.appendChild(tbl);
  v.block.appendChild(v.panel);
}

function buildBinRow(v, bin) {
  const r = { bin, inst: { bin }, tr: document.createElement('tr') };
  const name = cell(r.tr, '');
  name.className = 'name';
  const nm = document.createElement('span');
  nm.className = 'bin-name';
  nm.textContent = bin;            // 行按 bin 名建，这一格不用每轮再刷
  r.sub = document.createElement('small');
  r.sub.className = 'subline';
  name.append(nm, r.sub);
  const b = badgeCell(r.tr);
  r.badge = b.badge;
  r.badgeText = b.text;
  r.dur = cell(r.tr, '');
  // CPU / 内存和父行同款：数字后面挂一条趋势线（按这个 bin 自己的采样）
  r.cpuText = document.createTextNode('');
  r.memText = document.createTextNode('');
  r.cpu = cell(r.tr, '');
  r.cpu.appendChild(r.cpuText);
  r.mem = cell(r.tr, '');
  r.mem.appendChild(r.memText);
  const ops = cell(r.tr, '');
  ops.className = 'ops';
  // 四个按钮永远都在，运行态只改 disabled：
  //   未运行 → 启动可点、停止/重启灰；运行中 → 反之；日志恒可点
  r.btn = {};
  r.btn.start = btn(ops, '启动', false, () => openRun(v.u, bin), 'go');
  r.btn.stop = btn(ops, '停止', false, () => binAction(v.u, r.inst, 'stop'), 'stop');
  r.btn.restart = btn(ops, '重启', false, () => binAction(v.u, r.inst, 'restart'), 'restart');
  r.btn.log = btn(ops, '日志', false, () => openLogs(v.u, bin), 'chip');
  return r;
}

/// 一行一个 bin：同名的行复用，只刷文字和可用态；bin 增减才增删行
function syncBinRows(v, u) {
  const insts = u.instances || [];
  v.binEmpty.hidden = insts.length > 0;
  const seen = new Set();
  let i = 0;
  for (const inst of insts) {
    seen.add(inst.bin);
    let r = v.binRows.get(inst.bin);
    if (!r) { r = buildBinRow(v, inst.bin); v.binRows.set(inst.bin, r); }
    r.inst = inst;
    // 副行只在「有话说」时出现：没跑着的 bin 再写一遍「未运行」，跟右边那颗
    // 胶囊就是同一句话说两遍
    const sub = inst.running ? (inst.pid ? `pid ${inst.pid}` : '') : (inst.fail || '');
    r.sub.textContent = sub;
    r.sub.hidden = !sub;
    r.badge.className = 'badge ' + (inst.running ? 'on' : inst.active === 'failed' ? 'bad' : 'off');
    r.badgeText.nodeValue = inst.running ? '运行中' : inst.active === 'failed' ? '失败' : '未运行';
    // 项目行和 bin 行共用同一套左缘档位，两个层级的状态口径要一致
    const bst = inst.running ? 'st-on' : inst.active === 'failed' ? 'st-bad' : 'st-off';
    if (r.tr.className !== bst) r.tr.className = bst;
    r.dur.textContent = fmtDur(inst.uptime);
    // 单位跟父行同一把尺：父行 1159.1MB、子行 1.1GB 那种两套口径最掉价
    const ih = hist.get(v.key + '/' + inst.bin);
    r.cpuSvg = metric(r.cpu, r.cpuText, r.cpuSvg, ih && ih.cpu, 'cpu',
      inst.cpu == null ? '—' : inst.cpu.toFixed(1) + '%');
    r.memSvg = metric(r.mem, r.memText, r.memSvg, ih && ih.mem, 'mem', fmtMemMB(inst.memory));
    r.btn.start.disabled = inst.running;
    r.btn.stop.disabled = !inst.running;
    r.btn.restart.disabled = !inst.running;
    const at = v.binBody.children[i];
    if (at !== r.tr) v.binBody.insertBefore(r.tr, at || null);
    i++;
  }
  for (const [b, r] of [...v.binRows]) {
    if (!seen.has(b)) { r.tr.remove(); v.binRows.delete(b); }
  }
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

// ---------- 拉取代码（git pull）----------

/// 对一个项目 git pull。show(text, ok) 决定结果提示显示在哪：
/// 源码弹窗里传 srcMsg，列表行上传一个走顶部横幅的。
/// 返回是否成功，调用方据此决定要不要重画（列表要，弹窗只重扫目录树）。
async function pullCode(key, show) {
  show('正在 git pull…', true);
  try {
    // 状态变更走 POST：GET 的话 SameSite=Lax 会带着 cookie 放行跨站的顶层导航，
    // 聊天里一个链接点进来就能替你 git pull
    const r = await req(`api/units/${key}/pull`, { method: 'POST' });
    // 后端把 git 的 stdout 原样返回，之前整个扔掉只说「拉取成功」。
    // 「Already up to date.」和「真的更新了」是两回事，前者说明这次白点
    const out = (await r.text()).trim();
    const head = (out.split('\n').find(l => l.trim()) || '').trim();
    show(head ? `拉取成功：${head}` : '拉取成功', true);
    return true;
  } catch (e) {
    if (e.message !== '未登录') {
      // 失败时后端把 git 的 stderr 原样回传，可能一大段；横幅只留第一行，
      // 要看全文点进项目里（横幅是单行通道，堆六行会把页面顶歪）
      const head = (e.message.split('\n').find(l => l.trim()) || '').trim();
      show('拉取失败: ' + head, false);
    }
    return false;
  }
}

/// 列表行上的拉取：提示走横幅，成功后整表刷一次
/// （拉下来的新代码可能增删了 src/bin/ 下的程序，bin 列表得跟着变）。
function rowPull(u, b) {
  b.disabled = true;
  pullCode(u.key, (t, ok) => banner(`${u.name}: ${t}`, ok))
    .then(ok => {
      if (!ok) return;
      refresh();
      // 成功提示不像错误那样该赖着不走：几秒后自己收掉。先记下当前文案，
      // 免得这期间冒出来的新提示（比如刷新失败）被我们误清。
      const msg = $('banner').textContent;
      setTimeout(() => { if ($('banner').textContent === msg) banner(''); }, 5000);
    })
    .finally(() => { b.disabled = false; });
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

/// journalctl -o short-iso 的行首时间戳：2026-08-29T09:34:09+08:00，
/// 可能带小数秒、偏移量也可能写成 +0800，所以只锁死「日期T时间」那一段
const TS_RE = /^(\d{4}-\d\d-\d\dT[\d:.]+(?:[+-]\d{2}:?\d{2})?)\s*/;

/// 错误特征词：整行标红用。只挑「确实出事了」的词，不挑 warn/note 之类，
/// 否则长日志一半是红的就等于没有。status=[1-9] 是 systemd 的非零退出码。
const ERR_RE = /(fail|error|fatal|panic|denied|refused|unauthorized|segmentation|core dumped|out of memory|\bOOM\b|SIGKILL|SIGSEGV|cannot|could not|status=[1-9])/i;

/// 警告档：没到出错但值得看一眼的。排在错误后面判，一行只归一档
const WARN_RE = /(warn|deprecat|timed out|timeout|retry|retrying|throttl|too many|not enabled)/i;

/// 把拉到的各 bin 日志整到一块：全部模式按行首时间戳混合排序、每条标来源 bin；
/// 单 bin 模式只看那一个。用 DOM 文本节点渲染，日志里再怪的字符也不怕被当成 HTML。
function renderLogs(jump) {
  const filter = $('logFilter').value;
  const q = $('logSearch').value.trim().toLowerCase();
  const p = $('logBody');
  // 跟随开关：想停在中间看某一行时，每 3 秒的轮询不该把你顶回底部
  const follow = $('logFollow').checked;
  // 保持在底部时才跟随滚动（轮询时定位不会被顶到底部打断）
  const atBottom = p.scrollHeight - p.scrollTop - p.clientHeight < 40;
  p.replaceChildren();
  if (!logCache || logCache.length === 0) {
    $('logCount').textContent = '';
    p.append('(暂无日志)');
    return;
  }
  // 拆成 (时间戳, 来源, 正文) 三元组
  const rows = [];
  let last = '';
  for (const one of logCache) {
    for (const line of (one.log || '').split('\n')) {
      if (line.trim() === '') continue;
      const m = TS_RE.exec(line);
      if (m) last = m[1];
      // 不以时间戳开头的行（panic 栈、多行输出）沿用上一条的时间：不然全部模式
      // 按时间混排时，它们会被单独甩到最前面，和自己那条崩溃头分家
      rows.push({ time: last, src: one.bin, text: line });
    }
  }
  const keep1 = filter ? rows.filter(r => r.src === filter) : rows;
  // 关键字筛：日志里翻一行 panic 不该靠肉眼扫 300 行
  const keep = q ? keep1.filter(r => r.text.toLowerCase().includes(q)) : keep1;
  if (keep.length === 0) {
    $('logCount').textContent = q ? `这 ${keep1.length} 行里没有「${$('logSearch').value.trim()}」` : '';
    p.append('(暂无日志)');
    return;
  }
  // 全部模式按 ISO 时间戳字符串排序（字符串序即时间序）；单 bin 模式保持原序不用排
  if (!filter) keep.sort((a, b) => (a.time < b.time ? -1 : a.time > b.time ? 1 : 0));
  for (const r of keep) {
    // 一行一个 div：整行才能做 hover 和「这行是错误」的底色，
    // 拼成一坨文本节点就只能靠字色，扫长日志很吃力
    const ln = document.createElement('div');
    // 一行只归一档：错误 > 警告 > 普通
    ln.className = 'ln' + (ERR_RE.test(r.text) ? ' err' : WARN_RE.test(r.text) ? ' warn' : '');
    const tag = document.createElement('span');
    tag.className = 'src';
    tag.textContent = `[${r.src}] `;
    ln.appendChild(tag);
    // 只有行首确实是 journalctl short-iso 时间戳才单拎出来上色；进程自己打的续行
    // （panic 栈、多行输出）不以时间戳开头，整行照旧，不然会误染正文头一截
    const m = TS_RE.exec(r.text);
    if (m) {
      const ts = document.createElement('span');
      ts.className = 'ts';
      ts.textContent = m[1] + ' ';
      ln.append(ts, document.createTextNode(r.text.slice(m[0].length)));
    } else {
      ln.appendChild(document.createTextNode(r.text));
    }
    p.appendChild(ln);
  }
  $('logCount').textContent = q
    ? `匹配 ${keep.length} / ${keep1.length} 行`
    : `共 ${keep.length} 行`;
  // 关闭「跟随底部」后，轮询不再把你顶回底部；主动打开/切筛选（jump）仍然直达最新
  if (jump || (follow && atBottom)) p.scrollTop = p.scrollHeight;
}

async function refresh() {
  if (document.hidden) return;   // 后台标签页不用白跑
  // 单核机器上 /api/units 可能要一两秒，这期间界面毫无动静、看着像卡死，
  // 给 tick 前的小点一个呼吸当进度指示（不挡操作，也不另开 loading 层）
  document.body.classList.add('fetching');
  try {
    render(await (await req('api/units')).json());
    $('tick').textContent = '更新于 ' + new Date().toLocaleTimeString('zh-CN', { hour12: false });
    if ($('banner').textContent.startsWith('刷新失败')) banner('');
    await loadLogs(false);
    // 整机资源单独一个 try：读不到（比如不是 Linux）也不该影响项目列表
    try {
      renderHost(await (await req('api/host')).json());
    } catch { /* 401 已由 req() 处理 */ }
  } catch (e) {
    if (e.message !== '未登录') banner('刷新失败: ' + e.message);
    // 第一次就没拉回来：别让骨架行一直闪，闪着的灰条比空白更像卡住了
    if (!booted) $('projList').replaceChildren();
  } finally {
    document.body.classList.remove('fetching');
  }
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
  // 收成「图标 + 两字」：头部按钮本来就挤，「主题·白天」是这排最宽的一个
  $('themeBtn').textContent = light ? '☀ 白天' : '☾ 黑夜';
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
// SSH 私钥存在服务器上（/api/sshkey），换浏览器也不用重配。
// termKey = 这条终端绑定的项目；null = 导航栏那个通用控制台（不 cd，落在 /root）。
let term = null, fitAddon = null, termWs = null, termResizeHandler = null, termAuthFail = false;
let termKey = null;
const termEnc = new TextEncoder();   // 击键和粘贴都走它，两处共用一个编码器

// 查服务器上配没配私钥，刷新底部提示（出于安全，后端不回显私钥内容）
async function updateKeyHint() {
  const el = $('termKeyHint');
  try {
    const { configured } = await (await req('api/sshkey')).json();
    el.textContent = configured ? '已配置私钥（公钥认证）' : '未配置私钥';
    el.classList.toggle('on', configured);
  } catch {
    el.textContent = '';
    el.classList.remove('on');
  }
}

// WebSocket 地址：从当前页所在目录拼，天然带上秘密前缀；http→ws / https→wss。
// 绑了项目就把 key 带上，后端据此 cd 到该项目目录（目录由服务器自己扫，不传路径）。
function termUrl() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const base = location.pathname.replace(/[^/]*$/, '');
  const q = termKey ? '?key=' + encodeURIComponent(termKey) : '';
  return `${proto}//${location.host}${base}api/terminal${q}`;
}

function ensureTerm() {
  if (term) return;
  term = new Terminal({
    cursorBlink: true,
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Cascadia Code", monospace',
    fontSize: 13,
    lineHeight: 1.15,
    letterSpacing: 0,
    scrollback: 5000,
    // GitHub 暗色调色板，和面板的深色卡片协调，比默认纯黑更耐看
    theme: {
      background: '#0b0e14',
      foreground: '#c9d1d9',
      cursor: '#58a6ff',
      cursorAccent: '#0b0e14',
      selectionBackground: 'rgba(88,166,255,0.30)',
      black: '#484f58', red: '#ff7b72', green: '#3fb950', yellow: '#d29922',
      blue: '#58a6ff', magenta: '#bc8cff', cyan: '#39c5cf', white: '#b1bac4',
      brightBlack: '#6e7681', brightRed: '#ffa198', brightGreen: '#56d364', brightYellow: '#e3b341',
      brightBlue: '#79c0ff', brightMagenta: '#d2a8ff', brightCyan: '#56d4dd', brightWhite: '#f0f6fc'
    }
  });
  fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open($('termBody'));
  // 击键走二进制帧；缩放走 "R cols rows" 文本帧（后端按帧类型区分）
  term.onData(d => { if (termWs && termWs.readyState === 1) termWs.send(termEnc.encode(d)); });
  term.onResize(({ cols, rows }) => {
    if (termWs && termWs.readyState === 1) termWs.send('R ' + cols + ' ' + rows);
  });
  // Ctrl+V 在 xterm 里会被当成控制字符直接送给 shell（屏幕上就是一串 ^V），粘贴反而
  // 丢了；有选区时 Ctrl/Cmd+C 也该走浏览器复制，而不是发 SIGINT。
  // 自定义键处理器返回 false = 让 xterm 完全不碰这个键，交给浏览器原生 paste/copy，
  // 之后由 xterm 自己的 paste/copy 监听（或下面 window 上的兜底）把内容送进终端。
  term.attachCustomKeyEventHandler(e => {
    if (e.type !== 'keydown' || e.altKey) return true;
    const mod = e.ctrlKey || e.metaKey;
    if (mod && e.code === 'KeyV') return false;
    if (mod && e.code === 'KeyC' && term.hasSelection()) return false;
    return true;
  });
}

function fitTerm() {
  if (fitAddon) { try { fitAddon.fit(); } catch {} }
}

function connectTerm() {
  termAuthFail = false;
  // 每条新连接都是一个全新的远端 shell，屏幕上留着的旧会话内容已经是历史了。
  // 不清的话新提示符直接接在旧提示符后面，切几个项目就攒出一长串路径
  if (term) term.reset();
  const ws = new WebSocket(termUrl());
  ws.binaryType = 'arraybuffer';
  termWs = ws;
  // 私钥在服务器上，后端连接时自己读取，前端不用再发。连上直接 fit。
  ws.onopen = () => { $('termDot').classList.add('on'); fitTerm(); term.focus(); };
  ws.onmessage = e => {
    // 二进制帧 = 终端内容；文本帧 = 后端状态消息
    if (typeof e.data !== 'string') { term.write(new Uint8Array(e.data)); return; }
    if (e.data === 'AUTHFAIL') { termAuthFail = true; return; }
    term.write(e.data); // 其它文本（后端错误提示）直接显示到终端
  };
  ws.onclose = () => {
    if (termWs !== ws) return;
    termWs = null;
    $('termDot').classList.remove('on');
    if (termAuthFail) {
      term.write('\r\n\x1b[31m[公钥认证失败] 服务器拒绝了这把私钥。请点“配置密钥”检查：'
        + '私钥是否完整、是否与服务器 authorized_keys 里的公钥匹配、是否选对了这台机器的钥匙。\x1b[0m\r\n');
    } else {
      term.write('\r\n\x1b[2m[连接已断开，点“重连”重新连接]\x1b[0m\r\n');
    }
  };
}

/// 打开控制台。传项目 = 这个项目的专属控制台（一进去就在它目录里）；
/// 不传 = 导航栏那个通用控制台（不 cd，落在 root 家目录）。
function openTerm(u) {
  const prev = termKey;
  termKey = u ? u.key : null;
  $('termTarget').textContent = u ? `${location.hostname} · ${u.name}` : location.hostname;
  $('termBox').hidden = false;
  updateKeyHint();
  ensureTerm();
  if (!termResizeHandler) {
    termResizeHandler = () => { if (!$('termBox').hidden) fitTerm(); };
    window.addEventListener('resize', termResizeHandler);
  }
  // 已经连着一条：换项目就得先断掉重连，否则两条 WebSocket 同时往同一个终端里写，
  // 输出互相穿插；没换项目则保持这条会话不断（点两下不该把人家正在跑的程序打断）。
  if (termWs) { if (prev !== termKey) reconnectTerm(); return; }
  // 先让弹窗渲染出尺寸再 fit + 连接，否则终端行列数算不对
  requestAnimationFrame(() => { fitTerm(); connectTerm(); });
}

function closeTerm() {
  if (termWs) { try { termWs.close(); } catch {} termWs = null; }
  $('termBox').hidden = true;
}

function reconnectTerm() {
  if (termWs) { try { termWs.close(); } catch {} termWs = null; }
  requestAnimationFrame(() => { fitTerm(); connectTerm(); });   // 清屏统一在 connectTerm 里做
}

// 导航栏那个是通用控制台：不绑项目。必须包一层，别把点击事件当成项目传进去
$('console').onclick = () => openTerm();
$('termClose').onclick = closeTerm;
$('termCloseX').onclick = closeTerm;
$('termReconnect').onclick = reconnectTerm;

// ---------- 粘贴兜底 ----------
// xterm 只认落进终端内部的 paste（它监听 helper textarea 和 .xterm 元素，一接手就
// stopPropagation）。焦点不在终端上时——从别处复制完点了弹窗别的地方再 Ctrl+V、
// 或者走右键菜单的「粘贴」——事件落在 body 上，xterm 看不见，粘贴就静默丢了。
// 在 window 上兜一层：被 xterm 处理过的事件它自己已经 stopPropagation 拦掉，
// 冒泡不到这一层，所以不会重复粘贴。
window.addEventListener('paste', e => {
  if (!term || $('termBox').hidden) return;
  const text = e.clipboardData && e.clipboardData.getData('text/plain');
  if (!text) return;
  e.preventDefault();
  term.paste(text);   // 走 xterm 的粘贴通道：认 bracketed paste，多行不会当场被执行
});
// Windows 习惯右键粘贴。右键不改焦点，菜单里的「粘贴」就会落到 body 上被丢掉；
// 按下时先把焦点抓回终端，菜单粘贴才落得进终端（再走上面那条兜底）。
$('termBody').addEventListener('contextmenu', () => { if (term) term.focus(); });

// 私钥配置弹窗。私钥存服务器（不回显），所以打开时输入框留空，保存即覆盖。
$('termKey').onclick = () => {
  $('keyText').value = '';
  $('keyErr').textContent = '';
  $('keyBox').hidden = false;
  $('keyText').focus();
};
$('keyCloseX').onclick = $('keyCancel').onclick = () => { $('keyBox').hidden = true; };
$('keyClear').onclick = async () => {
  $('keyErr').textContent = '';
  try {
    await req('api/sshkey', { method: 'DELETE' });
    $('keyText').value = '';
    await updateKeyHint();
  } catch (e) {
    if (e.message !== '未登录') $('keyErr').textContent = '清除失败：' + e.message;
  }
};
$('keySave').onclick = async () => {
  const v = $('keyText').value.trim();
  $('keyErr').textContent = '';
  // 轻校验：像不像一把私钥，早点拦下明显贴错的内容
  if (!v || !/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----/.test(v)) {
    $('keyErr').textContent = '这看起来不是 SSH 私钥（应包含 “BEGIN ... PRIVATE KEY”）';
    return;
  }
  try {
    await req('api/sshkey', {
      method: 'PUT',
      headers: { 'Content-Type': 'text/plain; charset=utf-8' },
      body: v
    });
    $('keyBox').hidden = true;
    await updateKeyHint();
    // 用新私钥立刻重连（若控制台已打开）
    if (!$('termBox').hidden) reconnectTerm();
  } catch (e) {
    if (e.message !== '未登录') $('keyErr').textContent = '保存失败：' + e.message;
  }
};



// 切换日志源筛选：用上一次拉到的数据直接重渲染，不用重新请求
$('logFilter').onchange = () => { if (logCache) renderLogs(true); };
// 关键字筛选是纯前端的：只在已经拉到的那几百行里挑，打字时不该回头敲服务器
$('logSearch').oninput = () => { if (logCache) renderLogs(false); };

// 关闭日志弹窗：清空日志内容，下次再点日志按钮重新拉取
function closeLogBox() {
  logKey = null;
  logCache = null;
  $('logBody').replaceChildren();
  $('logSearch').value = '';
  $('logCount').textContent = '';
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
  // 标题里也挂那枚首字母徽标：从列表点进来看的是同一个项目，认知的锚点不该断
  const av = $('srcAv');
  av.textContent = initials(u.name);
  av.style.setProperty('--av', hueFor(u.name));
  av.hidden = false;
  $('srcQuery').value = '';
  endFind();
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

// ---------- 跨文件搜索 ----------
// 左栏那棵树和搜索结果共用一块地方：输入即搜（防抖 300ms），清空即回树。
// findSeq 用来丢迟到响应的 —— 连打两个字会发两次请求，先发的可能后到，
// 不认序号的话旧结果会把新结果盖掉。
let findTimer = null, findSeq = 0, findActive = false, jumpTimer = null;

/// 退出搜索态：清空输入或关掉弹窗时叫，之后树才允许重新往左栏画
function endFind() {
  findActive = false;
  findSeq++;
  clearTimeout(findTimer);
}

function findHead(text) {
  const d = document.createElement('div');
  d.className = 'find-head';
  d.textContent = text;
  return d;
}

async function runFind() {
  const q = $('srcQuery').value.trim();
  if (!srcKey) return;
  if (q.length < 2) { endFind(); renderTree(); return; }
  const seq = ++findSeq;
  findActive = true;
  const tree = $('srcTree');
  tree.replaceChildren(findHead(`在 ${srcName()} 里搜「${q}」…`));
  let r;
  try {
    r = await (await req(`api/units/${srcKey}/search?q=${encodeURIComponent(q)}`)).json();
  } catch (e) {
    if (seq !== findSeq) return;
    endFind();
    renderTree();
    if (e.message !== '未登录') srcMsg('搜索失败: ' + e.message, false);
    return;
  }
  if (seq !== findSeq) return;   // 已经有更新的一次在路上，这份丢掉
  renderFind(r, q);
}

function srcName() {
  return $('srcName').textContent;
}

function renderFind(r, q) {
  const tree = $('srcTree');
  tree.replaceChildren();
  const bits = [`${r.hits.length} 处命中`, `${r.files} 个文件`, `扫过 ${r.scanned} 个`];
  if (r.truncated) bits.push('已到上限，后面没扫完');
  tree.appendChild(findHead(bits.join(' · ')));
  if (!r.hits.length) {
    tree.appendChild(findHead('没有匹配。搜的是正文里的字和文件路径；target、node_modules 这些不搜。'));
    return;
  }
  for (const h of r.hits) tree.appendChild(hitRow(h, q));
}

/// 一行结果：图标 + 路径:行号 + 那一行原文（命中的那段包一层高亮）。
/// 全程建节点 + textContent，路径和代码里出现怪字符也不会被当成标记语言
function hitRow(h, q) {
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'hit-row';
  const line = document.createElement('span');
  line.className = 'hit-path';
  line.appendChild(iconEl(iconOf(h.path.slice(h.path.lastIndexOf('/') + 1)), h.path));
  const p = document.createElement('span');
  p.className = 'hit-file';
  p.textContent = h.path;
  line.appendChild(p);
  if (h.line) {
    const n = document.createElement('span');
    n.className = 'ln';
    n.textContent = ':' + h.line;
    line.appendChild(n);
  } else {
    // 行号 0 = 只有文件名对上了，正文里没有；不标出来会让人以为搜到了正文
    const n = document.createElement('span');
    n.className = 'ln';
    n.textContent = ' 文件名';
    line.appendChild(n);
  }
  b.appendChild(line);
  if (h.text) b.appendChild(hitText(h.text, q));
  b.onclick = () => openHit(h);
  return b;
}

function hitText(text, q) {
  const d = document.createElement('div');
  d.className = 'hit-text';
  const at = text.toLowerCase().indexOf(q.toLowerCase());
  if (at < 0) {
    d.textContent = text;
    return d;
  }
  const mk = s => { const m = document.createElement('mark'); m.textContent = s; return m; };
  d.append(
    document.createTextNode(text.slice(0, at)),
    mk(text.slice(at, at + q.length)),
    document.createTextNode(text.slice(at + q.length)));
  return d;
}

/// 点一条结果：把那个文件读进右栏，再跳到那一行。
/// 传 null 当 row —— 树里没有这一行（可能它在没展开的目录里），不硬找
async function openHit(h) {
  const name = h.path.slice(h.path.lastIndexOf('/') + 1);
  await showFile({ name, path: h.path, big: false }, null);
  if (h.line) jumpTo(h.line);
}

/// 跳到某一行并闪一下。用 offsetTop 自己算滚动，不用 scrollIntoView ——
/// 后者会连祖先一起滚，弹窗后面的列表会跟着跳位（#srcBody 已设 position:relative）
function jumpTo(line) {
  const p = $('srcBody');
  const el = p.children[line - 1];
  if (!el) return;
  p.scrollTop = Math.max(0, el.offsetTop - p.clientHeight / 2 + el.offsetHeight / 2);
  p.querySelectorAll('.cl.hit').forEach(e => e.classList.remove('hit'));
  el.classList.add('hit');
  clearTimeout(jumpTimer);
  jumpTimer = setTimeout(() => el.classList.remove('hit'), 1800);
}

$('srcQuery').oninput = () => {
  clearTimeout(findTimer);
  findTimer = setTimeout(runFind, 300);
};

/// 目录树渲染。目录点名字展开/收起（纯前端折叠，不发请求），
/// 文件点名字右侧显示内容。全程 textContent/DOM 建，不怕文件名里有怪字符。
function renderTree() {
  if (findActive) return;   // 搜索结果正占着左栏，迟到的目录树别把它盖回去
  const tree = $('srcTree');
  tree.replaceChildren();
  if (!srcCache) return;
  // 全部收起：大项目一口气全展开会渲染几千个节点，慢；点哪个目录再展开哪个
  tree.appendChild(treeNodes(srcCache.children, false));
}

/// 文件后缀 → 语言品牌色。色表就是右上角那枚语言徽标用的同一份（LANG_TAG），
/// 一处真源：文件点的颜色和点开之后那个标签的颜色必然对得上
function langColor(name) {
  const l = langOf(name);
  return l && LANG_TAG[l] ? LANG_TAG[l][1] : null;
}

/* ---------- 文件图标 ----------
   图标本体是 index.html 里那套内联 <symbol>（描边式、只用 currentColor 上色）。
   这里只管两件事：用哪个图形、上什么色。色还是从 LANG_TAG 取，不另开一份表 */
const ICON_BY_LANG = {
  rust: 'i-code', python: 'i-code', javascript: 'i-code', typescript: 'i-code',
  go: 'i-code', c: 'i-code', cpp: 'i-code', csharp: 'i-code', java: 'i-code',
  kotlin: 'i-code', swift: 'i-code', php: 'i-code', ruby: 'i-code', lua: 'i-code',
  perl: 'i-code', graphql: 'i-code', wasm: 'i-code', diff: 'i-code', r: 'i-code',
  css: 'i-code', scss: 'i-code', less: 'i-code', xml: 'i-code',
  markdown: 'i-doc', bash: 'i-term', sql: 'i-db',
  ini: 'i-gear', toml: 'i-gear', json: 'i-gear', yaml: 'i-gear', makefile: 'i-gear',
};
// 图片/压缩包/二进制这些后缀不是「语言」，langOf 认不出来，单独一张表。
// 这张表要先问：.svg 才能按图片算（否则被认成 xml，一堆尖括号）
const ICON_BY_EXT = {
  png: 'i-img', jpg: 'i-img', jpeg: 'i-img', gif: 'i-img', webp: 'i-img', bmp: 'i-img',
  ico: 'i-img', avif: 'i-img', svg: 'i-img', pdf: 'i-doc',
  zip: 'i-cube', tar: 'i-cube', gz: 'i-cube', xz: 'i-cube', bz2: 'i-cube', '7z': 'i-cube',
  exe: 'i-cube', so: 'i-cube', dll: 'i-cube', bin: 'i-cube', dat: 'i-cube',
  woff: 'i-cube', woff2: 'i-cube', ttf: 'i-cube', otf: 'i-cube',
};
// 锁文件按整个文件名判：Cargo.lock 的语义是「锁」，而它会被认成 toml，
// 不先拦一步就上一副齿轮了
const LOCK_NAMES = {
  'cargo.lock': 1, 'package-lock.json': 1, 'yarn.lock': 1, 'pnpm-lock.yaml': 1,
  'uv.lock': 1, 'poetry.lock': 1, 'composer.lock': 1, 'go.sum': 1,
};

function iconOf(name) {
  const low = name.toLowerCase();
  if (LOCK_NAMES[low]) return 'i-lock';
  const m = /\.([^.]+)$/.exec(low);
  if (m && ICON_BY_EXT[m[1]]) return ICON_BY_EXT[m[1]];
  const lang = langOf(name);
  return (lang && ICON_BY_LANG[lang]) || 'i-file';
}

/// 造一个图标。nameForColor 传文件名就按语言上色，传 null（目录）就走灰档 ——
/// 一屏里颜色只用来回答「这是什么文件」，不给目录抢戏
function iconEl(id, nameForColor) {
  const NS = 'http://www.w3.org/2000/svg';
  const s = document.createElementNS(NS, 'svg');
  s.setAttribute('class', 'fico');
  s.setAttribute('aria-hidden', 'true');
  const c = nameForColor && langColor(nameForColor);
  if (c) s.style.setProperty('--fc', c);
  const u = document.createElementNS(NS, 'use');
  u.setAttribute('href', '#' + id);
  s.appendChild(u);
  return s;
}

function treeNodes(nodes, expanded) {
  const wrap = document.createElement('div');
  wrap.className = 'kids';
  for (const n of nodes) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'node';
    row.textContent = n.name;
    // 图标固定占一格（1em）：目录那格是箭头，文件那格留空，名字才对齐得起来
    const tw = document.createElement('span');
    tw.className = 'tw';
    if (n.dir) {
      row.classList.add('dir');   // 目录名略粗：和文件只靠箭头区分不够，扫树时看不出层级骨架
      tw.textContent = expanded ? '▾' : '▸';
      row.prepend(tw, iconEl('i-folder', null));
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
      row.prepend(tw, iconEl(iconOf(n.name), n.name));
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

/// 路径写成面包屑：目录那串保持灰，文件名单独提出来。
/// 全程 textContent + 建节点，路径里真出现怪字符也不会被当成标记语言
function showPath(p) {
  const el = $('srcPath');
  el.replaceChildren();
  const i = p.lastIndexOf('/');
  if (i >= 0) {
    const dir = document.createElement('span');
    dir.className = 'dir';
    dir.textContent = p.slice(0, i + 1);
    el.appendChild(dir);
  }
  const f = document.createElement('span');
  f.className = 'file';
  f.textContent = p.slice(i + 1);
  el.appendChild(f);
}

async function showFile(n, row) {
  srcCur = n;
  $('srcTree').querySelectorAll('.node.cur').forEach(el => el.classList.remove('cur'));
  if (row) row.classList.add('cur');
  showPath(n.path);
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
  if (await pullCode(srcKey, srcMsg)) await loadTree();   // 代码变了,目录树重新拉一遍
  b.disabled = false;
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
  endFind();
  $('srcQuery').value = '';
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

/// 版本 / 构建 / 启动时刻：怀疑服务器上跑的不是最新一版时，先看这行，别靠猜
async function loadVersion() {
  try {
    const v = await (await req('api/version')).json();
    const el = $('ver');
    el.textContent = `v${v.ver} · ${v.git} · 起于 ${shortTime(v.started)}`;
    el.title = `代码 ${v.git}\n构建于 ${fullTime(v.built)}\n本次启动 ${fullTime(v.started)}`;
  } catch { /* 读不到版本不影响主界面，留着空位就行 */ }
}

const pad2 = n => String(n).padStart(2, '0');
function shortTime(secs) {
  const d = new Date(secs * 1000);
  return `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}
function fullTime(secs) {
  return new Date(secs * 1000).toLocaleString('zh-CN', { hour12: false });
}

// ---------- 操作记录 ----------
// 后端是内存环形（200 条，重启即丢），所以每次打开都重新拉，不做缓存
async function loadAudit() {
  const body = $('auditBody');
  try {
    const list = await (await req('api/audit')).json();
    $('auditNote').textContent = `${list.length} 条`;
    body.replaceChildren();
    if (!list.length) {
      body.appendChild(document.createTextNode('还没有记录。（面板重启就清空，不当长期留痕用）'));
      return;
    }
    // 最新一条放最上面：想知道「刚才谁点了什么」不该翻到底再倒着读
    for (const it of [...list].reverse()) {
      const row = document.createElement('div');
      row.className = 'audit-row';
      for (const [cls, text] of [['ts', fullTime(it.at)], ['who', it.who], ['what', it.what]]) {
        const s = document.createElement('span');
        s.className = cls;
        s.textContent = text;
        row.appendChild(s);
      }
      body.appendChild(row);
    }
  } catch (e) {
    if (e.message !== '未登录') body.textContent = '读取失败：' + e.message;
  }
}

$('auditBtn').onclick = () => { $('auditBox').hidden = false; loadAudit(); };
$('auditRefresh').onclick = loadAudit;
$('auditClose').onclick = $('auditCloseX').onclick = () => { $('auditBox').hidden = true; };

// ---------- 快捷键 ----------
// r 刷新、Esc 关弹窗。两种情况必须放行，否则是抢键：
//   1) 焦点在输入框/下拉里（筛项目名、贴私钥、日志框里打字），r 就是字母 r；
//   2) 控制台开着，按键属于里面那个 shell，被当成快捷键会莫名刷新一次页面。
const isTyping = () => ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName);

document.addEventListener('keydown', e => {
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  if (e.key === 'Escape') {
    // 搜索框里 Esc 先清查询、留在弹窗内（一下把整个弹窗关掉太狠了，
    // 人只是想撤掉这次搜索）
    const sq = $('srcQuery');
    if (!$('srcBox').hidden && document.activeElement === sq && sq.value) {
      sq.value = '';
      endFind();
      renderTree();
      e.preventDefault();
      return;
    }
    // 从最上层往下找，一次只关一层（配私钥弹窗开在控制台上头，得先关它）
    for (const box of ['keyBox', 'termBox', 'auditBox', 'confirmBox', 'runbox', 'logBox', 'srcBox']) {
      const el = $(box);
      if (!el || el.hidden) continue;
      if (box === 'termBox') closeTerm();
      else if (box === 'srcBox') closeSrcBox();
      else if (box === 'logBox') closeLogBox();
      else el.hidden = true;
      e.preventDefault();
      return;
    }
    return;
  }
  if ($('app').hidden || isTyping() || !$('termBox').hidden) return;
  if (e.key === '/' && !$('srcBox').hidden) {
    // / = 跳到搜索框（和多数代码浏览器的习惯一致）。放在 isTyping 之后，
    // 搜索框已经聚焦时斜杠照常输进去，不会被抢走
    e.preventDefault();
    $('srcQuery').focus();
    $('srcQuery').select();
    return;
  }
  if (e.key === 'r' || e.key === 'R') {
    e.preventDefault();
    refresh();
  }
});

document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });

fetch('api/me', { credentials: 'same-origin' })
  .then(r => r.ok ? showApp() : showLogin())
  .catch(showLogin);
