import { constants, promises as fs } from "node:fs";

export const MAX_CATALOG_BYTES = 256 * 1024;
const MAX_SPELLS = 512;
const RELOAD_MS = 60_000;
const CATEGORIES = new Set(["personal", "external", "healing", "damageReduction", "utility"]);
const objectWithKeys = (value, keys) => value && typeof value === "object" && !Array.isArray(value)
  && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));

export function validateCooldownCatalog(value) {
  if (!objectWithKeys(value, ["schemaVersion", "revision", "spells"]) || value.schemaVersion !== 1
    || typeof value.revision !== "string" || !/^[A-Za-z0-9._-]{1,64}$/.test(value.revision)
    || !Array.isArray(value.spells) || value.spells.length < 1 || value.spells.length > MAX_SPELLS) {
    throw new Error("Invalid cooldown catalogue");
  }
  const ids = new Set();
  const spells = value.spells.map(spell => {
    if (!objectWithKeys(spell, ["id", "name", "category", "defaultEnabled"])
      || !Number.isSafeInteger(spell.id) || spell.id < 1 || spell.id > 9_999_999 || ids.has(spell.id)
      || typeof spell.name !== "string" || !spell.name.length || spell.name !== spell.name.trim()
      || [...spell.name].length > 100 || /[\p{Cc}\p{Cf}]/u.test(spell.name)
      || !CATEGORIES.has(spell.category) || typeof spell.defaultEnabled !== "boolean") {
      throw new Error("Invalid cooldown spell");
    }
    ids.add(spell.id);
    return Object.freeze({ id: spell.id, name: spell.name, category: spell.category, defaultEnabled: spell.defaultEnabled });
  });
  const catalog = Object.freeze({ schemaVersion: 1, revision: value.revision, spells: Object.freeze(spells) });
  if (Buffer.byteLength(JSON.stringify(catalog)) > MAX_CATALOG_BYTES) throw new Error("Cooldown catalogue is too large");
  return catalog;
}

async function readCatalog(filePath) {
  const file = await fs.open(filePath, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const before = await file.stat();
    if (!before.isFile() || !Number.isSafeInteger(before.size) || before.size < 1 || before.size > MAX_CATALOG_BYTES) {
      throw new Error("Invalid cooldown catalogue file");
    }
    // One extra byte detects growth without an unbounded read or allocation.
    const bytes = Buffer.alloc(before.size + 1);
    let total = 0;
    while (total < bytes.length) {
      const { bytesRead } = await file.read(bytes, total, bytes.length - total, total);
      if (!bytesRead) break;
      total += bytesRead;
    }
    const after = await file.stat();
    if (total !== before.size || after.size !== before.size || after.mtimeMs !== before.mtimeMs || after.ctimeMs !== before.ctimeMs) {
      throw new Error("Cooldown catalogue changed while reading");
    }
    return validateCooldownCatalog(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(0, total))));
  } finally { await file.close(); }
}

// The mount contains public spell metadata only. It is read-only inside the API;
// operators replace its JSON atomically and increment revision. There is no timer
// or write endpoint, and concurrent requests share one bounded reload per minute.
export async function createCooldownCatalog({ filePath, now = Date.now, warn = console.warn } = {}) {
  let current = await readCatalog(new URL("./cooldown-catalog.json", import.meta.url));
  let nextRead = -Infinity;
  let inFlight = null;
  async function snapshot() {
    if (!filePath) return current;
    if (inFlight) return inFlight;
    if (now() < nextRead) return current;
    nextRead = now() + RELOAD_MS;
    inFlight = (async () => {
      try {
        const next = await readCatalog(filePath);
        if (next.revision === current.revision && JSON.stringify(next) !== JSON.stringify(current)) {
          throw new Error("Changed cooldown catalogue requires a new revision");
        }
        current = next;
      } catch {
        warn("Cooldown catalogue reload rejected; keeping the last valid catalogue.");
      }
      return current;
    })().finally(() => { inFlight = null; });
    return inFlight;
  }
  return { snapshot };
}
