// 直接执行页面的状态和预览函数，验证响应语义及参数边界。
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../static/app.js'), 'utf8');

function stateContext() {
  const classes = new Set();
  const banner = { textContent: '', hidden: true, classList: {
    toggle(name, on) { if (on) classes.add(name); else classes.delete(name); },
  } };
  const context = vm.createContext({ $: () => banner });
  vm.runInContext(source.slice(source.indexOf('function banner('), source.indexOf('// 弹窗滚动锁')), context);
  return { context, banner, classes };
}

test('HTTP 202 retains an amber unconfirmed message; 204 clears it', async () => {
  const { context, banner, classes } = stateContext();
  await context.actionResult({ status: 202, text: async () => '状态未确认：查询失败' });
  assert.equal(banner.textContent, '状态未确认：查询失败');
  assert.equal(banner.hidden, false);
  assert.equal(classes.has('pending'), true);
  assert.equal(classes.has('ok'), false);
  await context.actionResult({ status: 204 });
  assert.equal(banner.hidden, true);
  assert.equal(classes.has('pending'), false);
});

test('unknown and transitional states are never displayed as stopped', () => {
  const { context } = stateContext();
  for (const state of ['', 'unknown', 'activating', 'deactivating', 'reloading']) {
    const [label, tone] = context.serviceState(state);
    assert.equal(tone, 'pending');
    assert.notEqual(label, '已停止');
  }
  assert.equal(context.serviceState('inactive')[0], '已停止');
  assert.equal(context.serviceState('active')[0], '运行中');
  assert.equal(context.serviceState('failed')[1], 'bad');
});

function previewContext(req) {
  const nodes = new Map();
  const node = () => ({ textContent: '', value: '', disabled: true, children: [],
    replaceChildren() { this.children = []; }, append(...items) { this.children.push(...items); } });
  const $ = id => { if (!nodes.has(id)) nodes.set(id, node()); return nodes.get(id); };
  const context = vm.createContext({ $, req, document: { createElement: node }, setTimeout, clearTimeout });
  vm.runInContext("let runKey = 'project';\n" + source.slice(source.indexOf('let runBin = null;'), source.indexOf('function openRun(')), context);
  vm.runInContext("runBin='first'; runPreviewVersion=1", context);
  return { context, $, nodes };
}

test('preview renders literal values as text and preserves JSON argument boundaries', () => {
  const { context, $ } = previewContext();
  const extra = '  <img src=x> "two words"\nnext\t ';
  context.renderRunPreview({ program: '/usr/bin/cargo', args: ['run', '--features', 'a,b', '--bin', 'web', '--', extra],
    source: '<b>Cargo.toml</b>', features: ['a', 'b'], working_dir: '/project with spaces' });
  const tokens = $('runPreviewCommand').textContent.split('\n').map(JSON.parse);
  assert.equal(tokens.at(-1), extra);
  assert.equal(tokens.at(-2), '--');
  assert.equal($('runPreviewMeta').children[1].textContent, '<b>Cargo.toml</b>');
});

test('Java previews distinguish the build command from the eventual artifact', () => {
  const { context, $ } = previewContext();
  const extra = '  literal\n\t ';
  context.renderRunPreview({ program: '/jdk/bin/java', args: [extra], source: 'pom.xml', features: [], working_dir: '/project',
    build: { program: '/maven/bin/mvn', args: ['package'], artifact: '构建后选择唯一 JAR' } });
  assert.match($('runPreviewStatus').textContent, /构建成功后确定/);
  assert.match($('runPreviewCommand').textContent, /构建命令/);
  assert.equal($('runPreviewCommand').textContent.split('\n').at(-1), JSON.stringify(extra));
  assert.doesNotMatch($('runPreviewCommand').textContent, /--panel-java-build/);
});

test('an older preview cannot overwrite a new bin or re-enable its button', async () => {
  let finish;
  let signalReady;
  const ready = new Promise(resolve => { signalReady = resolve; });
  const { context, $ } = previewContext(async (_url, request) => {
    assert.equal(JSON.parse(request.body).bins[0].args, '  literal\n\t ');
    return { json: () => new Promise(resolve => { finish = resolve; signalReady(); }) };
  });
  $('runArgs').value = '  literal\n\t ';
  const pending = context.loadRunPreview(1);
  await ready;
  vm.runInContext("runBin='second'; runPreviewVersion=2", context);
  finish([{ program: 'old', args: [], source: 'old', features: [], working_dir: 'old' }]);
  await pending;
  assert.equal($('runPreviewCommand').textContent, '');
  assert.equal($('runGo').disabled, true);
});

test('restart previews the original whole argument before preserving the bin action protocol', async () => {
  const calls = []; let confirmation;
  const extra = '  --label "two words"\nnext\t ';
  const context = vm.createContext({
    req: async (url, request) => { calls.push({ url, request }); return { json: async () => [{ program: 'java', args: [extra], source: 'pom.xml', features: [], working_dir: '/project' }] }; },
    openConfirm: (_title, message, callback) => { confirmation = { message, callback }; },
    actionResult: async () => {}, refresh() {}, banner() {},
  });
  vm.runInContext(source.slice(source.indexOf('async function binAction('),source.indexOf('// ---------- 拉取代码')),context);
  await context.binAction({ key: 'project' },{ bin: 'worker', args: extra },'restart');
  assert.equal(calls.length,1);
  assert.equal(JSON.parse(calls[0].request.body).bins[0].args,extra);
  assert.match(confirmation.message,/pom.xml/);
  assert.ok(confirmation.message.includes(JSON.stringify(extra)));
  await confirmation.callback();
  assert.equal(calls[1].url,'api/units/project/bins/worker/restart');
  assert.equal(calls[1].request.body,'{}');
});

test('a valid new preview becomes runnable after an older submission finishes', async () => {
  const { context, $ } = previewContext(async () => ({ json: async () => [{ program: 'java', args: [], source: 'new', features: [], working_dir: '/new' }] }));
  vm.runInContext('runSubmitting=true',context);
  await context.loadRunPreview(1);
  assert.equal($('runGo').disabled,true);
  context.finishRunSubmission();
  assert.equal($('runGo').disabled,false);
  vm.runInContext('runPreviewVersion=2',context);
  context.finishRunSubmission();
  assert.equal($('runGo').disabled,true);
});
