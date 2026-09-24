const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '..', 'static', 'index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script, 'the management page must contain its rendering script');
const end = script.indexOf("  $('loginBtn').onclick");
assert.ok(end > 0, 'the test must stop before binding UI actions or booting the page');

const elements = {
  trendChart: { innerHTML: '' },
  trendMeta: { textContent: '' },
};
const source = script.slice(0, end) + '  return { renderTrend };\n})();';
const { renderTrend } = vm.runInNewContext(source, {
  document: { getElementById: id => elements[id] },
});

renderTrend({
  window: '24h',
  points: [
    { bucket_start_ms: 0, credits: '0.075600' },
    { bucket_start_ms: 3600000, credits: '45.059200' },
  ],
});

assert.equal(elements.trendMeta.textContent, '区间实扣 45.1348 积分 · 2 个时间点');
assert.match(elements.trendChart.innerHTML, />45\.0592<\/text>/);
assert.doesNotMatch(elements.trendChart.innerHTML, /45,059,200/);
