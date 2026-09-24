const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '..', 'static', 'index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script);
const end = script.indexOf("  $('loginBtn').onclick");
assert.ok(end > 0);

const elements = Object.fromEntries(
  ['keyCount', 'keyList', 'allocationKey', 'trendKey', 'allocatableHint', 'grantQuota']
    .map(id => [id, { innerHTML: '', textContent: '', value: '', disabled: false }]),
);
const source = script.slice(0, end) + '  return { renderKeyList, toggleUsageDetails };\n})();';
const { renderKeyList, toggleUsageDetails } = vm.runInNewContext(source, {
  document: { getElementById: id => elements[id] },
});

const key = {
  id: 'key-a', display_name: '周', prefix: 'aw_live_demo', status: 'active',
  scopes: ['chat:invoke', 'videos:submit'], max_concurrency: 2,
  current_concurrency: 0,
  credits: { allocated: '100.000000', used: '1.000000', remaining: '99.000000', held: '0.000000' },
};

renderKeyList([key]);
assert.match(elements.keyList.innerHTML, /data-usage hidden>/);
assert.equal(toggleUsageDetails('key-a'), true);
renderKeyList([{ ...key, credits: { ...key.credits, used: '2.500000', remaining: '97.500000' } }]);
assert.match(elements.keyList.innerHTML, /data-usage>显示名称：周 · 已分配 100 · 已实扣 2\.5 · 剩余 97\.5/);
assert.equal(toggleUsageDetails('key-a'), false);
renderKeyList([key]);
assert.match(elements.keyList.innerHTML, /data-usage hidden>/);
