import { constants, promises as fs } from "node:fs";
import path from "node:path";
import { createCipheriv, createDecipheriv, randomBytes, randomUUID } from "node:crypto";
import { HttpError } from "./security.mjs";

const MAX_BYTES = 2 * 1024 * 1024;
const MAX_PROFILES = 10_000;
const AAD = Buffer.from("Brick guild profiles v1");
const roles = new Set(["dps", "healer", "tank"]);

export function profilePatch(body, { roleOnly = false } = {}) {
  if (!body || typeof body !== "object" || Array.isArray(body)
    || !Object.keys(body).length || Object.keys(body).some(key => !["customName", "raidRole"].includes(key))
    || (roleOnly && (Object.keys(body).length !== 1 || !("raidRole" in body)))) {
    throw new HttpError(400, "Invalid profile settings.");
  }
  const patch = {};
  if ("customName" in body) {
    if (body.customName !== null && typeof body.customName !== "string") throw new HttpError(400, "Invalid display name.");
    const name = body.customName?.trim().normalize("NFC") || null;
    if (name && ([...name].length > 64 || /[\p{Cc}\p{Cf}]/u.test(name))) {
      throw new HttpError(400, "Use a display name of up to 64 characters without control characters.");
    }
    patch.customName = name;
  }
  if ("raidRole" in body) {
    if (body.raidRole !== null && !roles.has(body.raidRole)) throw new HttpError(400, "Choose DPS, Healer, or Tank.");
    patch.raidRole = body.raidRole;
  }
  return patch;
}

async function readPrivateFile(filePath, limit) {
  const file = await fs.open(filePath, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat = await file.stat();
    if (!stat.isFile() || !Number.isSafeInteger(stat.size) || stat.size < 0 || stat.size > limit) {
      throw new Error("Invalid profile storage file");
    }
    // Read one byte beyond the observed size to catch growth without allowing
    // readFile() to allocate an unbounded buffer after the initial stat check.
    const bytes = Buffer.alloc(stat.size + 1);
    let total = 0;
    while (total < bytes.length) {
      const { bytesRead } = await file.read(bytes, total, bytes.length - total, total);
      if (!bytesRead) break;
      total += bytesRead;
    }
    const after = await file.stat();
    if (total !== stat.size || after.size !== stat.size || after.mtimeMs !== stat.mtimeMs || after.ctimeMs !== stat.ctimeMs) {
      throw new Error("Profile storage changed while reading");
    }
    return bytes.subarray(0, total);
  } finally { await file.close(); }
}

// The key is supplied separately through the service secret mount; never create
// an adjacent key or fall back to cleartext files. The desktop keeps no copy.
export async function createProfileStore({ dataDir, env = process.env }) {
  const keyText = env.PROFILE_ENCRYPTION_KEY_FILE
    ? (await readPrivateFile(env.PROFILE_ENCRYPTION_KEY_FILE, 1024)).toString("utf8").trim()
    : env.PROFILE_ENCRYPTION_KEY?.trim();
  if (keyText && !/^[a-fA-F0-9]{64}$/.test(keyText)) throw new Error("PROFILE_ENCRYPTION_KEY must be a 32-byte hex key");
  const key = keyText ? Buffer.from(keyText, "hex") : null;
  const filePath = path.join(dataDir, "profiles.enc");
  let users = new Map();
  let writes = Promise.resolve();
  try {
    const payload = await readPrivateFile(filePath, MAX_BYTES);
    if (!key) throw new Error("Profile encryption key is required to open existing profiles");
    const envelope = JSON.parse(payload.toString("utf8"));
    if (envelope.version !== 1 || !/^[a-f0-9]{24}$/.test(envelope.iv)
      || !/^[a-f0-9]{32}$/.test(envelope.tag) || typeof envelope.data !== "string") throw new Error("Invalid encrypted profiles");
    const decipher = createDecipheriv("aes-256-gcm", key, Buffer.from(envelope.iv, "hex"));
    decipher.setAAD(AAD);
    decipher.setAuthTag(Buffer.from(envelope.tag, "hex"));
    const cleartext = Buffer.concat([decipher.update(Buffer.from(envelope.data, "base64")), decipher.final()]);
    let parsed;
    try { parsed = JSON.parse(cleartext.toString("utf8")); } finally { cleartext.fill(0); }
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed) || Object.keys(parsed).length > MAX_PROFILES) throw new Error("Invalid profiles");
    for (const [id, value] of Object.entries(parsed)) {
      if (!/^[0-9]{1,20}$/.test(id)) throw new Error("Invalid profile member");
      users.set(id, { customName: null, raidRole: null, ...profilePatch(value) });
    }
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }

  function get(id) {
    return { customName: null, raidRole: null, ...users.get(id) };
  }

  async function save(id, body, options) {
    if (!key) throw new HttpError(503, "Profile settings are not available yet.");
    if (!/^[0-9]{1,20}$/.test(id)) throw new HttpError(400, "Invalid member.");
    const patch = profilePatch(body, options);
    const run = writes.catch(() => {}).then(async () => {
      if (!users.has(id) && users.size >= MAX_PROFILES) throw new HttpError(503, "Profile settings are full.");
      const next = new Map(users);
      const value = { ...get(id), ...patch };
      if (value.customName === null && value.raidRole === null) next.delete(id);
      else next.set(id, value);
      const iv = randomBytes(12);
      const cipher = createCipheriv("aes-256-gcm", key, iv);
      cipher.setAAD(AAD);
      const cleartext = Buffer.from(JSON.stringify(Object.fromEntries(next)));
      let encrypted;
      try { encrypted = Buffer.concat([cipher.update(cleartext), cipher.final()]); } finally { cleartext.fill(0); }
      const encoded = JSON.stringify({ version: 1, iv: iv.toString("hex"), tag: cipher.getAuthTag().toString("hex"), data: encrypted.toString("base64") });
      if (Buffer.byteLength(encoded) > MAX_BYTES) throw new HttpError(503, "Profile settings are full.");
      await fs.mkdir(dataDir, { recursive: true, mode: 0o700 });
      const temporary = `${filePath}.${randomUUID()}.tmp`;
      let file;
      try {
        file = await fs.open(temporary, "wx", 0o600);
        await file.writeFile(encoded);
        await file.sync();
        await file.close();
        file = null;
        await fs.rename(temporary, filePath);
        users = next;
      } finally {
        await file?.close();
        await fs.unlink(temporary).catch(error => { if (error.code !== "ENOENT") throw error; });
      }
      return get(id);
    });
    writes = run.catch(() => {});
    return run;
  }

  function member(member) {
    const profile = get(member.userId);
    return { ...member, name: profile.customName || member.name, raidRole: profile.raidRole };
  }

  return { get, save, member, available: Boolean(key) };
}
