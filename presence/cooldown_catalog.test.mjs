import assert from "node:assert/strict";
import { test } from "node:test";
import { promises as fs } from "node:fs";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import os from "node:os";
import path from "node:path";
import { createCooldownCatalog, validateCooldownCatalog, MAX_CATALOG_BYTES } from "./cooldown_catalog.mjs";

const entry = { id: 200183, name: "Apotheosis", category: "healing", defaultEnabled: true };
const catalog = (revision = "test.1", spells = [entry]) => ({ schemaVersion: 1, revision, spells });
async function fixture(t) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "brick-catalog-test-"));
  t.after(() => fs.rm(dir, { recursive: true, force: true }));
  const filePath = path.join(dir, "catalog.json");
  const warnings = [];
  let time = 0;
  return { filePath, warnings, advance: () => { time += 60_000; }, options: { filePath, now: () => time, warn: value => warnings.push(value) } };
}

test("bundled catalogue preserves all 99 identities and the 71 enabled / 28 optional policy", async () => {
  const store = await createCooldownCatalog();
  const value = await store.snapshot();
  assert.equal(value.schemaVersion, 1);
  assert.equal(value.revision, "midnight-2026-09-10.1");
  assert.equal(value.spells.length, 99);
  assert.equal(value.spells.filter(s => s.defaultEnabled).length, 71);
  assert.equal(value.spells.filter(s => !s.defaultEnabled).length, 28);
  assert.deepEqual(value.spells.find(s => s.id === entry.id), entry);
  for (const id of [47536, 370537, 197908]) assert.equal(value.spells.find(s => s.id === id).defaultEnabled, false);
  assert.equal(new Set(value.spells.map(s => s.id)).size, 99);
  assert(Object.isFrozen(value) && Object.isFrozen(value.spells) && Object.isFrozen(value.spells[0]));
});

test("catalogue validation rejects unknown schema, fields, duplicate identities and out-of-bounds metadata", () => {
  const invalid = [null, [], {}, { ...catalog(), schemaVersion: 2 }, { ...catalog(), extra: true },
    catalog(""), catalog("a".repeat(65)), catalog("bad\nrevision"), catalog("v1", []),
    catalog("v1", Array.from({ length: 513 }, (_, i) => ({ ...entry, id: i + 1 }))), catalog("v1", [entry, entry])];
  for (const change of [{ id: 0 }, { id: 10_000_000 }, { id: 1.5 }, { id: "123" },
    { name: "" }, { name: " padded" }, { name: "x".repeat(101) }, { name: "bad\u202e" }, { name: "bad\n" },
    { category: "raid" }, { defaultEnabled: "true" }, { authorization: "never" }]) {
    invalid.push(catalog("v1", [{ ...entry, ...change }]));
  }
  for (const value of invalid) assert.throws(() => validateCooldownCatalog(value));
  assert.deepEqual(validateCooldownCatalog(catalog()), catalog());
});

test("atomic config replacements reload once per minute and invalid revisions retain the last valid snapshot", async t => {
  const f = await fixture(t);
  await fs.writeFile(f.filePath, JSON.stringify(catalog()));
  const store = await createCooldownCatalog(f.options);
  assert.deepEqual(await store.snapshot(), catalog());
  const next = catalog("test.2", [{ ...entry, category: "utility", defaultEnabled: false }]);
  await fs.writeFile(f.filePath + ".next", JSON.stringify(next));
  await fs.rename(f.filePath + ".next", f.filePath);
  assert.equal((await store.snapshot()).revision, "test.1");
  f.advance();
  assert.deepEqual(await store.snapshot(), next);
  await fs.writeFile(f.filePath, JSON.stringify(catalog("test.2")));
  f.advance();
  assert.deepEqual(await store.snapshot(), next, "changed metadata needs a new revision");
  await fs.writeFile(f.filePath, "not JSON");
  f.advance();
  assert.deepEqual(await store.snapshot(), next);
  await fs.rm(f.filePath);
  f.advance();
  assert.deepEqual(await store.snapshot(), next);
  assert.equal(f.warnings.length, 3);
});

test("concurrent readers share one bounded reload and a missing initial config uses bundled defaults", async t => {
  const f = await fixture(t);
  const store = await createCooldownCatalog(f.options);
  const originalOpen = fs.open.bind(fs);
  let reads = 0;
  t.mock.method(fs, "open", async (target, ...args) => {
    if (target === f.filePath) reads++;
    return originalOpen(target, ...args);
  });
  const responses = await Promise.all(Array.from({ length: 50 }, () => store.snapshot()));
  assert(responses.every(value => value.spells.length === 99));
  assert.equal(reads, 1);
  assert.equal(f.warnings.length, 1);
  await fs.writeFile(f.filePath, JSON.stringify(catalog()));
  f.advance();
  const updated = await Promise.all(Array.from({ length: 50 }, () => store.snapshot()));
  assert(updated.every(value => value.revision === "test.1"));
  assert.equal(reads, 2);
});

test("config reads reject symlinks, oversized input, invalid UTF-8 and growing files without replacing defaults", async t => {
  const f = await fixture(t);
  const store = await createCooldownCatalog(f.options);
  const target = f.filePath + ".target";
  await fs.writeFile(target, JSON.stringify(catalog()));
  await fs.symlink(target, f.filePath);
  assert.equal((await store.snapshot()).spells.length, 99);
  await fs.rm(f.filePath);
  await fs.writeFile(f.filePath, Buffer.alloc(MAX_CATALOG_BYTES + 1));
  f.advance();
  assert.equal((await store.snapshot()).spells.length, 99);
  const invalidUtf8 = Buffer.from(JSON.stringify(catalog()));
  invalidUtf8[invalidUtf8.indexOf("Apotheosis")] = 0xff;
  await fs.writeFile(f.filePath, invalidUtf8);
  f.advance();
  assert.equal((await store.snapshot()).spells.length, 99);
  await fs.writeFile(f.filePath, JSON.stringify(catalog()));
  const initialSize = (await fs.stat(f.filePath)).size;
  const originalOpen = fs.open.bind(fs);
  let capacity = 0;
  t.mock.method(fs, "open", async (target, ...args) => {
    const handle = await originalOpen(target, ...args);
    if (target !== f.filePath) return handle;
    let initial = true;
    return {
      async stat() {
        const value = await handle.stat();
        if (initial) { initial = false; await fs.writeFile(f.filePath, Buffer.alloc(MAX_CATALOG_BYTES * 2)); }
        return value;
      },
      async read(buffer, ...args) { capacity = Math.max(capacity, buffer.length); return handle.read(buffer, ...args); },
      close: () => handle.close(),
    };
  });
  f.advance();
  assert.equal((await store.snapshot()).spells.length, 99);
  assert(capacity > 0 && capacity <= initialSize + 1);
  assert.equal(f.warnings.length, 4);
});

test("a configured FIFO is rejected without blocking API reads", { skip: process.platform !== "linux" }, async t => {
  const f = await fixture(t);
  await promisify(execFile)("mkfifo", [f.filePath]);
  const moduleUrl = new URL("./cooldown_catalog.mjs", import.meta.url).href;
  const code = `import assert from 'node:assert/strict';
    import { createCooldownCatalog } from ${JSON.stringify(moduleUrl)};
    const store=await createCooldownCatalog({filePath:${JSON.stringify(f.filePath)},warn:()=>{}});
    assert.equal((await store.snapshot()).spells.length,99);`;
  await promisify(execFile)(process.execPath, ["--input-type=module", "-e", code], { timeout: 3000 });
});
