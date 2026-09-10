import path from 'node:path';
import { mkdirSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
import { HttpError } from './security.mjs';

const VERSION = 1;
const RETENTION_MS = 180 * 86400_000;
const LIMIT = 100_000;
const integer = (value, min, max) => Number.isSafeInteger(value) && value >= min && value <= max;
const finite = (value, min, max) => Number.isFinite(value) && value >= min && value <= max;
const hash = value => createHash('sha256').update(value).digest('hex');
const recordingId = key => 'recording/'+hash(JSON.stringify([key.readerVersion,key.provider,key.videoId,key.broadcastId,key.recordingStartMs]));
const CHECK_INTERVAL_MS = 3600_000;

// Only small numeric observations are stored. No images, OAuth tokens or log
// events enter this database; the authenticated route checks VOD visibility.
export function syncKey(value) {
  if (!value || value.readerVersion !== VERSION
      || !['twitch', 'youtube'].includes(value.provider)
      || typeof value.videoId !== 'string'
      || !(value.provider === 'twitch' ? /^[0-9]{1,32}$/ : /^[a-zA-Z0-9_-]{11}$/).test(value.videoId)
      || typeof value.broadcastId !== 'string' || !/^[a-zA-Z0-9_-]{1,128}$/.test(value.broadcastId)
      || typeof value.report !== 'string' || !/^[a-zA-Z0-9]{16}$/.test(value.report)
      || !integer(value.pullId, 1, 100_000) || !integer(value.encounter, 1, 100_000)
      || ![3, 4, 5].includes(value.difficulty)
      || !integer(value.startMs, 1_500_000_000_000, 4_000_000_000_000)
      || !integer(value.endMs, value.startMs + 1, value.startMs + 3600_000)
      || !integer(value.recordingStartMs, value.startMs - 604800_000, value.startMs)) {
    throw new HttpError(400, 'Invalid raid timestamp key.');
  }
  const key = Object.fromEntries(['readerVersion', 'provider', 'videoId', 'broadcastId', 'report', 'pullId', 'encounter', 'difficulty', 'startMs', 'endMs', 'recordingStartMs'].map(name => [name, value[name]]));
  return { key, id: hash(JSON.stringify(key)) };
}

export function createReplaySyncLibrary({ dataDir, now = Date.now }) {
  let database;
  let statements;
  let pruned = -Infinity;
  function open() {
    if (database) return;
    mkdirSync(dataDir, { recursive: true, mode: 0o700 });
    database = new DatabaseSync(path.join(dataDir, 'replay-sync.sqlite'));
    database.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=500;
      PRAGMA cache_size=-1024; PRAGMA journal_size_limit=1048576;
      CREATE TABLE IF NOT EXISTS observations (
        key TEXT NOT NULL, member TEXT NOT NULL, unix_seconds INTEGER NOT NULL,
        video_seconds REAL NOT NULL, uncertainty REAL NOT NULL, updated_at INTEGER NOT NULL,
        PRIMARY KEY (key, member)
      ) STRICT;
      CREATE INDEX IF NOT EXISTS observations_age ON observations(updated_at);
      CREATE TABLE IF NOT EXISTS timestamp_jobs (
        id TEXT PRIMARY KEY, identity TEXT NOT NULL UNIQUE, payload TEXT NOT NULL,
        priority INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
        next_at INTEGER NOT NULL, lease TEXT, lease_until INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL
      ) STRICT;
      CREATE TABLE IF NOT EXISTS server_alignments (
        key TEXT PRIMARY KEY, unix_seconds INTEGER NOT NULL, video_seconds REAL NOT NULL,
        uncertainty REAL NOT NULL, updated_at INTEGER NOT NULL
      ) STRICT;
      CREATE TABLE IF NOT EXISTS recording_calibrations (
        id TEXT PRIMARY KEY, source_key TEXT NOT NULL, source_type TEXT NOT NULL,
        payload TEXT NOT NULL, unix_seconds INTEGER NOT NULL, video_seconds REAL NOT NULL,
        uncertainty REAL NOT NULL, checked_min INTEGER NOT NULL, checked_max INTEGER NOT NULL,
        stable INTEGER NOT NULL, updated_at INTEGER NOT NULL
      ) STRICT;
      CREATE INDEX IF NOT EXISTS recording_media_clock ON recording_calibrations (json_extract(payload,'$.provider'),json_extract(payload,'$.videoId'),json_extract(payload,'$.broadcastId'),updated_at);
      CREATE TABLE IF NOT EXISTS report_clocks (
        first_report TEXT NOT NULL, second_report TEXT NOT NULL, shift REAL NOT NULL,
        updated_at INTEGER NOT NULL, PRIMARY KEY(first_report,second_report)
      ) STRICT;
      DELETE FROM timestamp_jobs WHERE identity NOT LIKE 'recording/%';`);
    // Rebuild only derived models when introducing report-clock correction;
    // retain all independently measured timestamps and peer observations.
    if(database.prepare('PRAGMA user_version').get().user_version<2) {
      database.exec('DELETE FROM recording_calibrations; DELETE FROM report_clocks; PRAGMA user_version=2;');
    }
    statements = {
      read: database.prepare('SELECT unix_seconds AS unixSeconds, video_seconds AS videoSeconds, uncertainty AS uncertaintySeconds FROM observations WHERE key = ? AND updated_at >= ? ORDER BY video_seconds'),
      member: database.prepare('SELECT 1 FROM observations WHERE key = ? AND member = ?'),
      countKey: database.prepare('SELECT count(*) AS count FROM observations WHERE key = ?'),
      countAll: database.prepare('SELECT count(*) AS count FROM observations'),
      prune: database.prepare('DELETE FROM observations WHERE updated_at < ?'),
      insert: database.prepare(`INSERT INTO observations VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(key, member) DO UPDATE SET unix_seconds=excluded.unix_seconds,
        video_seconds=excluded.video_seconds, uncertainty=excluded.uncertainty, updated_at=excluded.updated_at`),
    };
  }
  function measured(id) {
    const measured = database.prepare('SELECT unix_seconds AS unixSeconds, video_seconds AS videoSeconds, uncertainty AS uncertaintySeconds FROM server_alignments WHERE key = ? AND updated_at >= ?').get(id, now() - RETENTION_MS);
    if (measured) return { ...measured, verified: true, verifiedBy: 'server', confirmations: 0 };
    const rows = statements.read.all(id, now() - RETENTION_MS);
    if (!rows.length) return null;
    // Each row is a distinct authenticated guild member. Use a bounded cluster,
    // not a transitive chain of pairwise-near readings. Disagreement cannot
    // silently turn into a verified shared alignment.
    const clusters = rows.map(first => rows.filter(row => row.unixSeconds === first.unixSeconds
      && row.videoSeconds >= first.videoSeconds && row.videoSeconds - first.videoSeconds <= 0.25));
    clusters.sort((a, b) => b.length - a.length || a[0].uncertaintySeconds - b[0].uncertaintySeconds);
    const best = clusters[0];
    const seconds = best.map(row => row.videoSeconds).sort((a, b) => a - b);
    const videoSeconds = (seconds[Math.floor((seconds.length - 1) / 2)] + seconds[Math.floor(seconds.length / 2)]) / 2;
    const uncertaintySeconds = Math.max(...best.map(row => Math.abs(row.videoSeconds - videoSeconds) + row.uncertaintySeconds));
    return { videoSeconds, unixSeconds: best[0].unixSeconds, uncertaintySeconds,
      confirmations: best.length, verified: best.length >= 2 && best.length * 2 > rows.length && uncertaintySeconds <= 0.35 };
  }
  function youtubeClock(value) {
    if (value.provider !== 'youtube') return null;
    const start = value.recordingStartMs ?? Date.parse(value.startedAt);
    const rows = database.prepare(`SELECT * FROM recording_calibrations WHERE source_type='server' AND stable=1
      AND json_extract(payload,'$.provider')='youtube' AND json_extract(payload,'$.videoId')=?
      AND json_extract(payload,'$.broadcastId')=? AND updated_at>=? ORDER BY updated_at DESC LIMIT 8`)
      .all(value.videoId, value.broadcastId, now()-RETENTION_MS);
    for (const row of rows) {
      const anchor = JSON.parse(row.payload);
      const origin = Math.round(anchor.startMs-row.video_seconds*1000);
      const shift = origin-anchor.recordingStartMs;
      if (anchor.readerVersion === VERSION && Math.abs(shift)>60_000 && Math.abs(shift)<=3600_000
          && (start===anchor.recordingStartMs || start===origin)) return {row, origin, shift};
    }
    return null;
  }
  function calibration(key) {
    return database.prepare('SELECT * FROM recording_calibrations WHERE id=? AND updated_at>=?').get(recordingId(key),now()-RETENTION_MS) || youtubeClock(key)?.row;
  }
  function reportShift(from,to) {
    if(from===to)return 0;
    const links=database.prepare('SELECT * FROM report_clocks WHERE updated_at>=? ORDER BY updated_at DESC LIMIT 128').all(now()-RETENTION_MS);
    const pending=[[from,0]],seen=new Set([from]);
    for(let i=0;i<pending.length&&i<64;i++) {
      const [report,shift]=pending[i];
      for(const link of links) {
        const next=link.first_report===report?link.second_report:link.second_report===report?link.first_report:null;
        if(!next||seen.has(next))continue;
        const value=shift+(link.first_report===report?link.shift:-link.shift);
        if(next===to)return value;
        seen.add(next);pending.push([next,value]);
      }
    }
    return null;
  }
  function linkReports(from,to,shift) {
    if(from===to||!finite(shift,-5000,5000))return;
    if(from>to){[from,to]=[to,from];shift=-shift;}
    const old=reportShift(from,to);
    if(old!==null)return;
    database.prepare('INSERT OR IGNORE INTO report_clocks VALUES (?,?,?,?)').run(from,to,shift,now());
  }
  function overlap(anchor,key) {
    return anchor.report!==key.report && anchor.encounter===key.encounter && anchor.difficulty===key.difficulty
      && key.endMs-key.startMs>=10000 && Math.abs(anchor.startMs-key.startMs)<=3000
      && Math.abs(anchor.endMs-key.endMs)<=3000
      && Math.abs((anchor.endMs-anchor.startMs)-(key.endMs-key.startMs))<=250;
  }
  function remember(key, id, alignment) {
    const old = calibration(key);
    if (!old) {
      database.prepare('INSERT OR REPLACE INTO recording_calibrations VALUES (?,?,?,?,?,?,?,?,?,?,?)')
        .run(recordingId(key),id,alignment.verifiedBy==='server'?'server':'peers',JSON.stringify(key),alignment.unixSeconds,
          alignment.videoSeconds,alignment.uncertaintySeconds,key.startMs,key.startMs,1,now());
    } else {
      const anchor = JSON.parse(old.payload);
      let shift=reportShift(anchor.report,key.report);
      if(shift===null && Math.abs((alignment.videoSeconds-old.video_seconds)-(alignment.unixSeconds-old.unix_seconds))<=1.25) {
        linkReports(anchor.report,key.report,key.startMs-anchor.startMs-(alignment.videoSeconds-old.video_seconds)*1000);
        shift=reportShift(anchor.report,key.report);
      }
      const predicted = shift===null?NaN:old.video_seconds+(key.startMs-anchor.startMs-shift)/1000;
      const stable = old.stable && Math.abs(predicted-alignment.videoSeconds)<=0.5;
      // A discontinuity disables extrapolation for this recording. Exact
      // measured pulls remain usable, and subsequent pulls are scanned.
      database.prepare('UPDATE recording_calibrations SET stable=?,checked_min=min(checked_min,?),checked_max=max(checked_max,?),updated_at=? WHERE id=?')
        .run(stable?1:0,key.startMs,key.startMs,now(),old.id);
    }
  }
  function lookup(value) {
    const {key,id} = syncKey(value);open();
    const direct = measured(id);
    if (direct?.verified) { remember(key,id,direct);return direct; }
    const row = calibration(key);
    if (!row?.stable || (row.source_type==='peers' && !measured(row.source_key)?.verified)) return direct;
    const anchor = JSON.parse(row.payload);
    // Changed bounds for the original pull invalidate that exact observation.
    if (anchor.report===key.report && anchor.pullId===key.pullId && (anchor.startMs!==key.startMs || anchor.endMs!==key.endMs || anchor.encounter!==key.encounter || anchor.difficulty!==key.difficulty)) return direct;
    if(overlap(anchor,key))linkReports(anchor.report,key.report,key.startMs-anchor.startMs);
    const shift=reportShift(anchor.report,key.report);
    if(shift===null)return direct;
    const delta = (key.startMs-anchor.startMs-shift)/1000;
    const videoSeconds = row.video_seconds+delta;
    const estimate = (key.startMs-key.recordingStartMs)/1000;
    if (Math.abs(delta)>86400 || !finite(videoSeconds,0,604800) || Math.abs(videoSeconds-estimate)>(key.provider==='youtube' && row.source_type==='server'?3600:60)) return direct;
    return {videoSeconds,unixSeconds:Math.floor(row.unix_seconds+delta),
      uncertaintySeconds:Math.min(0.35,row.uncertainty+0.1),verified:true,verifiedBy:'server',confirmations:0,
      source:'recording-calibration',anchor:{report:anchor.report,pullId:anchor.pullId,startMs:anchor.startMs,
        unixSeconds:row.unix_seconds,videoSeconds:row.video_seconds}};
  }
  function needsCheck(key, alignment) {
    if (!alignment?.verified) return true;
    if (alignment.source!=='recording-calibration') return false;
    const row=calibration(key);
    return key.startMs<row.checked_min-CHECK_INTERVAL_MS || key.startMs>row.checked_max+CHECK_INTERVAL_MS;
  }
  return {
    lookup,
    correctReplay(replay, live = false) {
      open();
      const clock = youtubeClock(replay);
      if (!clock || Date.parse(replay.startedAt)===clock.origin) return replay;
      // A verified visual anchor establishes media second zero. Existing
      // clients can then validate and seek the native YouTube timeline.
      return {...replay, startedAt:new Date(clock.origin).toISOString(),
        availableSeconds:live?Math.max(0,Math.floor(replay.availableSeconds-clock.shift/1000)):replay.availableSeconds};
    },
    // These methods have no HTTP route. Only the bounded worker with the
    // shared data volume can lease work and publish server measurements.
    enqueue(values) {
      open();
      database.prepare('DELETE FROM timestamp_jobs WHERE updated_at < ?').run(now() - 7 * 86400_000);
      database.prepare('DELETE FROM server_alignments WHERE updated_at < ?').run(now() - RETENTION_MS);
      database.prepare('DELETE FROM recording_calibrations WHERE updated_at < ?').run(now() - RETENTION_MS);
      database.prepare('DELETE FROM report_clocks WHERE updated_at < ?').run(now() - RETENTION_MS);
      let count = database.prepare('SELECT count(*) AS n FROM timestamp_jobs').get().n;
      for (const value of values.slice(0, 64)) {
        const { key, id } = syncKey(value);
        if (!needsCheck(key,lookup(key))) continue;
        const identity = recordingId(key);
        const existing = database.prepare('SELECT * FROM timestamp_jobs WHERE identity = ?').get(identity);
        if (existing?.id === id || (!existing && count >= 256)) continue;
        if (existing) {
          const pending=JSON.parse(existing.payload);
          const changedBounds=pending.report===key.report && pending.pullId===key.pullId;
          if (!changedBounds && (existing.attempts===0 || existing.lease_until>now())) continue;
        }
        // Browsing another pull must not bypass a failed recording's backoff.
        const nextAt = Math.max(now(), existing?.next_at ?? 0);
        database.prepare(`INSERT INTO timestamp_jobs(id, identity, payload, priority, next_at, updated_at)
          VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(identity) DO UPDATE SET id=excluded.id,
          payload=excluded.payload, priority=excluded.priority, attempts=0, next_at=excluded.next_at,
          lease=NULL, lease_until=0, updated_at=excluded.updated_at`).run(id, identity, JSON.stringify(key), key.startMs, nextAt, now());
        if (!existing) count++;
      }
    },
    claim() {
      open();
      database.exec('BEGIN IMMEDIATE');
      try {
        for (;;) {
          const row = database.prepare(`SELECT * FROM timestamp_jobs WHERE attempts < 3 AND next_at <= ?
            AND lease_until <= ? ORDER BY priority DESC, updated_at DESC LIMIT 1`).get(now(), now());
          if (!row) { database.exec('COMMIT'); return null; }
          const key=JSON.parse(row.payload);
          if (!needsCheck(key,lookup(key))) { database.prepare('DELETE FROM timestamp_jobs WHERE id=?').run(row.id);continue; }
          const lease = randomUUID();
          database.prepare('UPDATE timestamp_jobs SET lease=?, lease_until=?, attempts=attempts+1 WHERE id=?').run(lease, now()+120_000, row.id);
          database.exec('COMMIT');
          return { id: row.id, lease, key, attempt: row.attempts+1 };
        }
      } catch (error) { database.exec('ROLLBACK'); throw error; }
    },
    finish(job, alignment) {
      open();
      const { key, id } = syncKey(job.key);
      const row = database.prepare('SELECT id FROM timestamp_jobs WHERE id=? AND lease=? AND lease_until>?').get(id, job.lease, now());
      if (!row || id !== job.id) return false;
      const estimate = (key.startMs-key.recordingStartMs)/1000;
      const tolerance = key.provider==='youtube'?3600:60;
      const valid = alignment && integer(alignment.unixSeconds, Math.floor(key.startMs/1000)-3, Math.floor(key.startMs/1000)+3)
        && finite(alignment.videoSeconds, Math.max(0,estimate-tolerance), Math.min(604800,estimate+tolerance))
        && finite(alignment.uncertaintySeconds,0.05,0.225);
      if (valid) {
        database.prepare(`INSERT INTO server_alignments VALUES (?,?,?,?,?) ON CONFLICT(key) DO UPDATE SET
          unix_seconds=excluded.unix_seconds, video_seconds=excluded.video_seconds,
          uncertainty=excluded.uncertainty, updated_at=excluded.updated_at`).run(id, alignment.unixSeconds, alignment.videoSeconds, alignment.uncertaintySeconds, now());
        remember(key,id,{...alignment,verifiedBy:'server'});
        database.prepare('DELETE FROM timestamp_jobs WHERE id=?').run(id);
      } else {
        database.prepare('UPDATE timestamp_jobs SET lease=NULL, lease_until=0, next_at=?, updated_at=? WHERE id=?').run(now()+600_000,now(),id);
      }
      return !!valid;
    },
    submit(memberId, value) {
      const { id, key } = syncKey(value?.key);
      const observation = value?.alignment;
      const estimate = (key.startMs - key.recordingStartMs) / 1000;
      if (typeof memberId !== 'string' || !/^[0-9]{1,20}$/.test(memberId)
          || !observation || !integer(observation.unixSeconds, Math.floor(key.startMs / 1000) - 3, Math.floor(key.startMs / 1000) + 3)
          || !finite(observation.videoSeconds, Math.max(0, estimate - 60), Math.min(604800, estimate + 60))
          || !finite(observation.uncertaintySeconds, 0.05, 0.225)) {
        throw new HttpError(400, 'Invalid raid timestamp observation.');
      }
      open();
      const member = hash(memberId);
      if (now() - pruned > 3600_000) { statements.prune.run(now() - RETENTION_MS); pruned = now(); }
      const existing = statements.member.get(id, member);
      if (!existing && (statements.countKey.get(id).count >= 10 || statements.countAll.get().count >= LIMIT)) {
        throw new HttpError(429, 'The timestamp library is full.');
      }
      statements.insert.run(id, member, observation.unixSeconds, observation.videoSeconds, observation.uncertaintySeconds, now());
      return lookup(key);
    },
    close() { database?.close(); database = undefined; statements = undefined; },
  };
}
