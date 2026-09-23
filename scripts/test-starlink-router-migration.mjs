import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..');
const buildScript = fs.readFileSync(path.join(root, 'scripts', 'build-starlink-router.ps1'), 'utf8');
const startScript = fs.readFileSync(path.join(root, 'scripts', 'start-starlink-core.ps1'), 'utf8');
const migrationScript = fs.readFileSync(path.join(root, 'scripts', 'migrate-starlink-router.ps1'), 'utf8');
const mainSource = fs.readFileSync(path.join(root, 'starlink-dimension-router', 'src', 'main.rs'), 'utf8');
const adminRoutes = fs.readFileSync(path.join(root, 'starlink-dimension-router', 'src', 'admin_routes.rs'), 'utf8');
const machinePathPattern = /D:\\gpt|C:\\Users\\StarLink/;
function sourceFiles(directory) {
  return fs.readdirSync(directory, {withFileTypes:true}).flatMap(entry => {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) return sourceFiles(file);
    return /\.(?:ps1|rs)$/.test(entry.name) ? [file] : [];
  });
}
for (const directory of ['scripts', 'src-core/src', 'src-core/tests', 'starlink-dimension-router/src', 'starlink-dimension-router/tests']) {
  for (const file of sourceFiles(path.join(root, directory))) {
    assert.doesNotMatch(fs.readFileSync(file, 'utf8'), machinePathPattern, `${path.relative(root, file)} contains a machine-specific path`);
  }
}
assert.match(buildScript, /CARGO_TARGET_DIR/);
assert.doesNotMatch(startScript, /D:\\gpt|C:\\Users\\StarLink/);
assert.match(startScript, /STARLINK_ROUTER_DATA_DIR/);
assert.match(mainSource, /current_exe\(\)/);
assert.doesNotMatch(mainSource, /D:\\gpt/);
assert.doesNotMatch(adminRoutes, /D:\\gpt/);
assert.match(buildScript, /starlink-dimension-router\.exe/);
assert.match(migrationScript, /\$Apply/);
assert.match(migrationScript, /source_hashes/);
assert.doesNotMatch(migrationScript, /api_key_value|cookie|jwt/i);
console.log('starlink router migration/build contract: PASS');
