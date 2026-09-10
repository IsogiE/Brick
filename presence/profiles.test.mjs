import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, writeFile, rm, stat, readdir, symlink } from "node:fs/promises";
import { promises as fs } from "node:fs";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import os from "node:os";
import path from "node:path";
import { createProfileStore, profilePatch } from "./profiles.mjs";

async function fixture(t) {
  const dataDir = await mkdtemp(path.join(os.tmpdir(), "brick-profiles-test-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  return { dataDir, env: { PROFILE_ENCRYPTION_KEY: "d7".repeat(32) } };
}

test("profile persistence encrypts names and IDs, preserves concurrent edits, and reloads", async t => {
  const options = await fixture(t);
  const store = await createProfileStore(options);
  await Promise.all([
    store.save("123456789", { customName: "Private Raider Name", raidRole: "healer" }),
    store.save("234567890", { customName: "Private Tank Name" }),
    store.save("123456789", { raidRole: "dps" }, { roleOnly: true }),
  ]);
  const file = path.join(options.dataDir, "profiles.enc");
  const encrypted = await readFile(file, "utf8");
  for (const value of ["Private Raider Name", "Private Tank Name", "123456789", "customName", "raidRole"]) assert(!encrypted.includes(value));
  assert.equal((await stat(file)).mode & 0o777, 0o600);
  assert.deepEqual(await readdir(options.dataDir), ["profiles.enc"]);
  const reloaded = await createProfileStore(options);
  assert.deepEqual(reloaded.get("123456789"), { customName: "Private Raider Name", raidRole: "dps" });
  assert.equal(reloaded.get("234567890").customName, "Private Tank Name");
  assert.deepEqual(reloaded.member({ userId: "123456789", name: "Discord", role: "Raider" }), {
    userId: "123456789", name: "Private Raider Name", role: "Raider", raidRole: "dps",
  });
  await reloaded.save("123456789", { customName: "", raidRole: null });
  assert.equal(reloaded.member({ userId: "123456789", name: "Discord" }).name, "Discord");
  assert.notEqual(await readFile(file, "utf8"), encrypted);
});

test("profile persistence fails closed with missing or incorrect keys and tampered data", async t => {
  const options = await fixture(t);
  const disabled = await createProfileStore({ ...options, env: {} });
  assert.equal(disabled.available, false);
  await assert.rejects(disabled.save("123", { customName: "Secret" }), { status: 503 });
  assert.deepEqual(await readdir(options.dataDir), []);
  const store = await createProfileStore(options);
  await store.save("123", { customName: "Secret" });
  await assert.rejects(createProfileStore({ ...options, env: {} }));
  await assert.rejects(createProfileStore({ ...options, env: { PROFILE_ENCRYPTION_KEY: "11".repeat(32) } }));
  const file = path.join(options.dataDir, "profiles.enc");
  const envelope = JSON.parse(await readFile(file, "utf8"));
  envelope.tag = "00".repeat(16);
  await writeFile(file, JSON.stringify(envelope));
  await assert.rejects(createProfileStore(options));
});

test("profile input rejects impersonation fields, control characters, oversized names and invalid roles", () => {
  for (const body of [null, [], {}, { userId: "123" }, { role: "Officer" }, { customName: 5 },
    { customName: "a".repeat(65) }, { customName: "name\nother" }, { customName: "name\u202e" },
    { raidRole: "Officer" }, { raidRole: {} }]) assert.throws(() => profilePatch(body), { status: 400 });
  assert.deepEqual(profilePatch({ customName: "  Se\u0301p  ", raidRole: "healer" }), { customName: "Sép", raidRole: "healer" });
  assert.throws(() => profilePatch({ customName: "Other" }, { roleOnly: true }), { status: 400 });
  assert.throws(() => profilePatch({ customName: "Other", raidRole: "tank" }, { roleOnly: true }), { status: 400 });
});

test("profile reads reject symlinks and oversized files", async t => {
  const options = await fixture(t);
  const target = path.join(options.dataDir, "target");
  await writeFile(target, "{}");
  const file = path.join(options.dataDir, "profiles.enc");
  await symlink(target, file);
  await assert.rejects(createProfileStore(options));
  await rm(file);
  await writeFile(file, Buffer.alloc(2 * 1024 * 1024 + 1));
  await assert.rejects(createProfileStore(options));
});

test("profile reads remain bounded if a regular file grows after its initial stat", async t => {
  const options = await fixture(t);
  const store = await createProfileStore(options);
  await store.save("123", { customName: "Private name" });
  const filePath = path.join(options.dataDir, "profiles.enc");
  const initialSize = (await stat(filePath)).size;
  const originalOpen = fs.open.bind(fs);
  let readCapacity = 0;
  let reads = 0;
  t.mock.method(fs, "open", async (target, ...args) => {
    const handle = await originalOpen(target, ...args);
    if (target !== filePath) return handle;
    let initial = true;
    return {
      async stat() {
        const snapshot = await handle.stat();
        if (initial) {
          initial = false;
          await writeFile(filePath, Buffer.alloc(3 * 1024 * 1024));
        }
        return snapshot;
      },
      async read(buffer, offset, length, position) {
        readCapacity = Math.max(readCapacity, buffer.length);
        reads++;
        return handle.read(buffer, offset, length, position);
      },
      close: () => handle.close(),
    };
  });
  await assert.rejects(createProfileStore(options), /changed while reading/);
  assert(reads > 0);
  assert(readCapacity <= initialSize + 1);
});

test("profile and key FIFOs reject promptly without blocking startup", { skip: process.platform !== "linux" }, async t => {
  const options = await fixture(t);
  const fifo = path.join(options.dataDir, "profiles.enc");
  await promisify(execFile)("mkfifo", [fifo]);
  const moduleUrl = new URL("./profiles.mjs", import.meta.url).href;
  // A child-process deadline makes this regression fail safely even if a
  // blocking FIFO open is accidentally reintroduced into the helper.
  for (const config of [options, { ...options, env: { PROFILE_ENCRYPTION_KEY_FILE: fifo } }]) {
    const source = `import assert from 'node:assert/strict';
      import { createProfileStore } from ${JSON.stringify(moduleUrl)};
      await assert.rejects(createProfileStore(${JSON.stringify(config)}), /Invalid profile storage file/);`;
    await promisify(execFile)(process.execPath, ["--input-type=module", "-e", source], { timeout: 3000 });
  }
});
