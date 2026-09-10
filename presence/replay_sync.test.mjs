import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createReplaySyncLibrary, syncKey } from './replay_sync.mjs';

const key = { readerVersion: 1, provider: 'twitch', videoId: '2869611753', broadcastId: '318589298534', report: 'BQTW6P1hyY3dfmxG', pullId: 59, encounter: 3429, difficulty: 4,
  startMs: 1788986075032, endMs: 1788986463450, recordingStartMs: 1788969458000 };
const alignment = { unixSeconds: 1788986075, videoSeconds: 16615.96, uncertaintySeconds: 0.10 };
async function fixture(t) {
  const dataDir = await mkdtemp(path.join(os.tmpdir(), 'brick-sync-library-'));
  let clock = key.startMs + 3600_000;
  let library = createReplaySyncLibrary({ dataDir, now: () => clock });
  t.after(async () => { library.close(); await rm(dataDir, { recursive: true, force: true }); });
  return { get library() { return library; }, dataDir,
    advance(ms) { clock += ms; }, restart() { library.close(); library = createReplaySyncLibrary({ dataDir, now: () => clock }); } };
}
test('separate members establish consensus; repeated uploads and disagreement cannot forge it', async t => {
  const f = await fixture(t);
  assert.equal(f.library.lookup(key), null);
  for (let i = 0; i < 5; i++) assert.equal(f.library.submit('11', { key, alignment }).verified, false);
  const second = f.library.submit('22', { key, alignment: { ...alignment, videoSeconds: 16616.04 } });
  assert.equal(second.verified, true);
  assert.equal(second.confirmations, 2);
  assert.ok(Math.abs(second.videoSeconds - 16616) < 1e-8);
  assert.equal(f.library.submit('33', { key, alignment: { ...alignment, videoSeconds: 16620 } }).verified, true);
  assert.equal(f.library.submit('44', { key, alignment: { ...alignment, videoSeconds: 16620.1 } }).verified, false);
  f.restart();
  assert.equal(f.library.lookup(key).verified, false);
  assert.equal(f.library.submit('44', { key, alignment }).verified, true);
  assert.ok((await stat(path.join(f.dataDir, 'replay-sync.sqlite'))).size < 128 * 1024);
});
test('exact recording, pull, algorithm and time bounds prevent reuse of stale results', async t => {
  const f = await fixture(t);
  f.library.submit('11', { key, alignment });
  for (const change of [{ videoId: '123' }, { broadcastId: 'other' }, { report: 'abcdefghABCDEFGH' }, { pullId: 60 }, { startMs: key.startMs + 1 }, { endMs: key.endMs + 1 }, { recordingStartMs: key.recordingStartMs + 1 }]) {
    assert.equal(f.library.lookup({ ...key, ...change }), null);
  }
  for (const change of [{ readerVersion: 2 }, { difficulty: 10 }, { endMs: key.startMs }, { report: '../other' }, { startMs: NaN }]) {
    assert.throws(() => syncKey({ ...key, ...change }), /Invalid/);
  }
  f.advance(181 * 86400_000);
  assert.equal(f.library.lookup(key), null);
});
test('invalid, imprecise or implausible observations are rejected', async t => {
  const f = await fixture(t);
  for (const change of [{ videoSeconds: Infinity }, { videoSeconds: 10 }, { unixSeconds: 1788986000 }, { uncertaintySeconds: 0 }, { uncertaintySeconds: 0.3 }]) {
    assert.throws(() => f.library.submit('11', { key, alignment: { ...alignment, ...change } }), /Invalid/);
  }
  assert.equal(f.library.lookup(key), null);
});

test('one durable worker lease per recording, with trusted measurements and stale-work rejection', async t => {
  const f = await fixture(t);
  f.library.enqueue([key,key]);
  const job = f.library.claim();assert.ok(job);assert.equal(job.attempt,1);
  assert.equal(f.library.claim(),null);
  assert.equal(f.library.finish({...job,lease:'wrong'},alignment),false);
  f.restart();assert.equal(f.library.claim(),null);
  assert.equal(f.library.finish(job,alignment),true);
  assert.equal(f.library.lookup(key).verifiedBy,'server');
  assert.equal(f.library.lookup(key).verified,true);
  f.library.enqueue([key]);assert.equal(f.library.claim(),null);
  const changed = {...key,endMs:key.endMs+1};
  f.library.enqueue([changed]);const next = f.library.claim();assert.ok(next);
  assert.equal(f.library.finish(job,alignment),false);
  assert.equal(f.library.finish(next,{...alignment,videoSeconds:0}),false);
  assert.equal(f.library.lookup(changed),null);
  assert.equal(f.library.claim(),null);
  f.advance(600_001);assert.equal(f.library.claim().attempt,2);
});

test('one timestamp calibrates later pulls and new reports without rescanning', async t => {
  const f=await fixture(t);
  f.library.enqueue([key]);assert.equal(f.library.finish(f.library.claim(),alignment),true);
  const report='WKHnGLprBXJ862CM';
  const later={...key,report,pullId:1,startMs:key.startMs+1801_092,endMs:key.endMs+1801_092};
  assert.equal(f.library.lookup(later),null);
  const overlap={...key,report,pullId:9,startMs:key.startMs+1092,endMs:key.endMs+1092};
  assert.equal(f.library.lookup(overlap).videoSeconds,alignment.videoSeconds);
  const result=f.library.lookup(later);
  assert.equal(result.verified,true);assert.equal(result.source,'recording-calibration');
  assert.equal(result.videoSeconds,alignment.videoSeconds+1800);
  assert.equal(result.anchor.startMs,key.startMs);
  f.library.enqueue([later]);assert.equal(f.library.claim(),null);
  f.restart();assert.equal(f.library.lookup(later).videoSeconds,result.videoSeconds);
  for(const change of [{videoId:'123'}, {broadcastId:'other'}, {recordingStartMs:key.recordingStartMs+1}]) {
    assert.equal(f.library.lookup({...later,...change}),null);
  }
});

test('new pulls and changed bounds preserve failed recording backoff across restarts', async t => {
  const f = await fixture(t);
  f.library.enqueue([key]);
  assert.equal(f.library.finish(f.library.claim(), null), false);
  const later = { ...key, pullId: 60, startMs: key.startMs + 600_000, endMs: key.endMs + 600_000 };
  f.library.enqueue([later]);
  assert.equal(f.library.claim(), null);
  const changed = { ...later, endMs: later.endMs + 1 };
  f.library.enqueue([changed]);
  f.restart();
  assert.equal(f.library.claim(), null);
  // A different recording remains eligible while this one backs off.
  f.library.enqueue([{ ...key, videoId: '123' }]);
  const other = f.library.claim();
  assert.equal(other.key.videoId, '123');
  assert.equal(f.library.finish(other, alignment), true);
  f.advance(599_999);
  assert.equal(f.library.claim(), null);
  f.advance(2);
  const retry = f.library.claim();
  assert.deepEqual(retry.key, changed);
  assert.equal(f.library.finish(retry, { ...alignment, unixSeconds: alignment.unixSeconds + 600,
    videoSeconds: alignment.videoSeconds + 600 }), true);
});

test('one non-overlapping report check corrects the clock for other calibrated POVs', async t => {
  const f=await fixture(t);
  const other={...key,videoId:'12345',broadcastId:'other'};
  f.library.enqueue([key,other]);
  for(let i=0;i<2;i++) {const job=f.library.claim();assert.ok(job);f.library.finish(job,alignment);}
  const report='WKHnGLprBXJ862CM';
  const later={...key,report,pullId:1,startMs:key.startMs+601_092,endMs:key.endMs+601_092};
  const otherLater={...later,videoId:other.videoId,broadcastId:other.broadcastId};
  assert.equal(f.library.lookup(later),null);assert.equal(f.library.lookup(otherLater),null);
  f.library.enqueue([later,otherLater]);const job=f.library.claim();assert.ok(job);
  assert.equal(f.library.finish(job,{...alignment,unixSeconds:alignment.unixSeconds+600,videoSeconds:alignment.videoSeconds+600}),true);
  assert.equal(f.library.lookup(later).videoSeconds,alignment.videoSeconds+600);
  assert.equal(f.library.lookup(otherLater).videoSeconds,alignment.videoSeconds+600);
  assert.equal(f.library.claim(),null);
});

test('initial work coalesces across pulls and one sparse check catches a discontinuity', async t => {
  const f=await fixture(t);
  const pulls=Array.from({length:20},(_,i)=>({...key,pullId:100+i,startMs:key.startMs+i*60_000,endMs:key.endMs+i*60_000}));
  f.library.enqueue(pulls);
  const first=f.library.claim();assert.ok(first);assert.equal(f.library.claim(),null);
  assert.equal(f.library.finish(first,alignment),true);
  f.library.enqueue(pulls);assert.equal(f.library.claim(),null);
  const later={...key,pullId:999,startMs:key.startMs+3700_000,endMs:key.endMs+3700_000};
  assert.equal(f.library.lookup(later).verified,true);
  f.library.enqueue([later,{...later,pullId:1000,startMs:later.startMs+60_000,endMs:later.endMs+60_000}]);
  const check=f.library.claim();assert.ok(check);assert.equal(f.library.claim(),null);
  assert.equal(f.library.finish(check,{...alignment,unixSeconds:alignment.unixSeconds+3700,videoSeconds:alignment.videoSeconds+3702}),true);
  assert.equal(f.library.lookup(later).source,undefined);
  assert.equal(f.library.lookup(pulls[1]),null);
  assert.equal(f.library.lookup(first.key).verified,true);
});

test('peer calibration requires continuing independent agreement', async t => {
  const f=await fixture(t);
  const later={...key,pullId:60,startMs:key.startMs+600_000,endMs:key.endMs+600_000};
  f.library.enqueue([key]);
  f.library.submit('11',{key,alignment});assert.equal(f.library.lookup(later),null);
  f.library.submit('22',{key,alignment});assert.equal(f.library.lookup(later).verified,true);
  assert.equal(f.library.claim(),null);
  f.library.submit('33',{key,alignment:{...alignment,videoSeconds:alignment.videoSeconds+2}});
  f.library.submit('44',{key,alignment:{...alignment,videoSeconds:alignment.videoSeconds+2}});
  assert.equal(f.library.lookup(later),null);
});
test('expired leases recover and queues are bounded across metadata refreshes', async t => {
  const f = await fixture(t);
  f.library.enqueue([key]);const first = f.library.claim();
  f.advance(120_001);const retry=f.library.claim();assert.ok(retry);assert.equal(retry.attempt,2);
  assert.equal(f.library.finish(first,alignment),false);
  const changed={...key,endMs:key.endMs+1};f.library.enqueue([changed]);
  assert.equal(f.library.finish(retry,alignment),false);
  assert.equal(f.library.claim().attempt,1);
  for(let i=0;i<5;i++)f.library.enqueue(Array.from({length:64},(_,n)=>({...key,pullId:100+i*64+n})));
  let count=0;while(f.library.claim())count++;
  assert.ok(count<=255);
});

test('verified YouTube media clocks correct replay metadata and work with existing client bounds', async t => {
  const f=await fixture(t);
  const original={...key,provider:'youtube',videoId:'904VO_G5QoQ',broadcastId:'904VO_G5QoQ'};
  const estimate=(original.startMs-original.recordingStartMs)/1000;
  const measured={...alignment,videoSeconds:estimate-270.818};
  const replay={provider:'youtube',videoId:original.videoId,broadcastId:original.broadcastId,
    startedAt:new Date(original.recordingStartMs).toISOString(),availableSeconds:20000};
  assert.deepEqual(f.library.correctReplay(replay,true),replay);
  assert.throws(()=>f.library.submit('11',{key:original,alignment:measured}),/Invalid/);
  f.library.enqueue([original]);assert.equal(f.library.finish(f.library.claim(),measured),true);
  const corrected=f.library.correctReplay(replay,true);
  const adjusted={...original,recordingStartMs:Date.parse(corrected.startedAt)};
  assert.equal(adjusted.recordingStartMs,original.recordingStartMs+270818);
  assert.equal(corrected.availableSeconds,19729);
  assert.deepEqual(f.library.correctReplay(corrected,true),corrected);
  assert.equal(f.library.correctReplay(replay,false).availableSeconds,replay.availableSeconds);
  for(const pull of [adjusted,{...adjusted,pullId:60,startMs:adjusted.startMs+600000,endMs:adjusted.endMs+600000}]){
    const result=f.library.lookup(pull);
    assert.equal(result.verified,true);
    assert.ok(Math.abs(result.videoSeconds-(pull.startMs-pull.recordingStartMs)/1000)<0.001);
    assert.ok(Math.abs(result.unixSeconds-Math.floor(pull.startMs/1000))<=3);
    assert.ok(result.uncertaintySeconds<=0.35);
  }
  assert.equal(f.library.lookup({...adjusted,endMs:adjusted.endMs+1}),null);
  assert.equal(f.library.lookup({...adjusted,recordingStartMs:adjusted.recordingStartMs+1}),null);
  assert.deepEqual(f.library.correctReplay({...replay,broadcastId:'other'},true),{...replay,broadcastId:'other'});
  f.restart();assert.deepEqual(f.library.correctReplay(replay,true),corrected);
  // A later independent reading must update the original model, not create an
  // unrelated model merely because clients now use the corrected media origin.
  const later={...adjusted,pullId:70,startMs:adjusted.startMs+3601000,endMs:adjusted.endMs+3601000};
  f.library.enqueue([later]);assert.equal(f.library.finish(f.library.claim(),{...measured,unixSeconds:measured.unixSeconds+3601,videoSeconds:measured.videoSeconds+3601}),true);
  assert.equal(f.library.lookup(later).verified,true);
  assert.deepEqual(f.library.correctReplay(replay,true),corrected);
});

test('large clock corrections remain exclusive to precise, leased YouTube worker observations', async t => {
  const f=await fixture(t);
  f.library.enqueue([key]);const job=f.library.claim();
  assert.equal(f.library.finish(job,{...alignment,videoSeconds:alignment.videoSeconds-270}),false);
  const youtube={...key,provider:'youtube',videoId:'904VO_G5QoQ',broadcastId:'904VO_G5QoQ'};
  for(const change of [{videoSeconds:alignment.videoSeconds-4000},{unixSeconds:alignment.unixSeconds+10},{uncertaintySeconds:.3}]){
    f.library.enqueue([youtube]);
    const next=f.library.claim();assert.ok(next);
    assert.equal(f.library.finish(next,{...alignment,...change}),false);
    f.advance(600001);
  }
  const replay={provider:'youtube',videoId:youtube.videoId,broadcastId:youtube.broadcastId,startedAt:new Date(key.recordingStartMs).toISOString(),availableSeconds:20000};
  assert.deepEqual(f.library.correctReplay(replay,true),replay);
});
