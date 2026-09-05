import assert from 'node:assert/strict';
import { formatMicrounits } from './src/format.ts';

assert.equal(formatMicrounits('0', 'CNY'), 'CNY 0.00');
assert.equal(formatMicrounits('1', 'CNY'), 'CNY 0.00000001');
assert.equal(formatMicrounits('1000000', 'CNY'), 'CNY 0.01');
assert.equal(formatMicrounits('-125000000', 'CNY'), 'CNY −1.25');
assert.equal(formatMicrounits('9223372036854775807', 'CNY'), 'CNY 92,233,720,368.54775807');
assert.equal(formatMicrounits('-9223372036854775808', 'CNY'), 'CNY −92,233,720,368.54775808');
assert.equal(formatMicrounits('1000000', 'JPY'), 'JPY 1');
assert.equal(formatMicrounits('1000000', 'KWD'), 'KWD 0.001');
console.log('PASS exact microunit display, negative balances, i64 extremes and currency minor units');
