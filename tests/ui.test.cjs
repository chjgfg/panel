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
