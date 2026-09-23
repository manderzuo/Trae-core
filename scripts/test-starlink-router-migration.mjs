import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..');
const buildScript = fs.readFileSync(path.join(root, 'scripts', 'build-starlink-router.ps1'), 'utf8');
const migrationScript = fs.readFileSync(path.join(root, 'scripts', 'migrate-starlink-router.ps1'), 'utf8');
assert.match(buildScript, /D:\\gpt/);
assert.match(buildScript, /starlink-dimension-router\.exe/);
assert.match(migrationScript, /\$Apply/);
assert.match(migrationScript, /source_hashes/);
assert.doesNotMatch(migrationScript, /api_key_value|cookie|jwt/i);
console.log('starlink router migration/build contract: PASS');
