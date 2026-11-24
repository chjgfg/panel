const assert = require('node:assert/strict');
assert.deepEqual(process.argv.slice(2), [process.env.PANEL_EXPECT_ARGUMENT]);
console.log('NODE_SERVICE_OK');
if (process.env.PANEL_PROBE_SLEEP) setTimeout(() => {}, 30000);
