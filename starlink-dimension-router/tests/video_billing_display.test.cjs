const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '..', 'static', 'index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script, 'the management page must contain its rendering script');
const end = script.indexOf("  $('loginBtn').onclick");
assert.ok(end > 0);

const elements = {
  videoBillingStatus: { textContent: '', className: '' },
  videoBillingMode: { value: '' },
  videoBillingCards: { innerHTML: '' },
  videoBillingReason: { textContent: '' },
  videoBillingMsg: { textContent: '一次性验收已登记；只有匹配该 Key 和请求摘要的下一次视频请求可以通过。', className: 'msg ok' },
};
const source = script.slice(0, end) + '  return { renderVideoBilling };\n})();';
const { renderVideoBilling } = vm.runInNewContext(source, {
  document: { getElementById: id => elements[id] },
});

renderVideoBilling({
  mode: 'paused',
  reason: '单次验收完成',
  diagnostic: { claimed: true },
  counts: { held: 0, reconcile_required: 0, verified_settled: 5, legacy_unverified: 0 },
});

assert.match(elements.videoBillingCards.innerHTML, /<div class="value">已暂停<\/div>/);
assert.match(elements.videoBillingCards.innerHTML, /<div class="value">5<\/div>/);
assert.equal(elements.videoBillingMsg.textContent, '');
assert.match(elements.videoBillingReason.textContent, /本次一次性验收已使用/);
