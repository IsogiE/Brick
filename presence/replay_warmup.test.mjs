import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createReplayWarmup} from './replay_warmup.mjs';
const key={readerVersion:1,provider:'twitch',videoId:'123',broadcastId:'456',recordingStartMs:1788969458000,report:'BQTW6P1hyY3dfmxG',pullId:59,encounter:3429,difficulty:4,startMs:1788986075032,endMs:1788986463450};
test('live replay authorization keeps exact provider identity, broadcast and current guild eligibility',()=>{
 let now=0;const warm=createReplayWarmup({streams:{},library:{},now:()=>now});
 warm.remember({userId:'11'},{provider:'twitch',videoId:'123',broadcastId:'456',startedAt:new Date(key.recordingStartMs).toISOString()});
 assert.equal(warm.allows(key,new Set(['11'])),true);
 assert.equal(warm.allows(key,new Set()),false);
 for(const change of [{videoId:'124'},{broadcastId:'457'},{recordingStartMs:key.recordingStartMs+1},{provider:'youtube'}])assert.equal(warm.allows({...key,...change},new Set(['11'])),false);
 now=900_001;assert.equal(warm.allows(key,new Set(['11'])),false);
});
test('one background round warms covered POVs with at most two provider requests and backs off',async()=>{
 let active=0,peak=0,calls=0;const jobs=[];
 const streams={snapshot:async()=>({streams:Array.from({length:10},(_,n)=>({userId:String(11+n)}))}),replay:async stream=>{
  calls++;active++;peak=Math.max(peak,active);await new Promise(resolve=>setTimeout(resolve,1));active--;
  return {provider:'twitch',videoId:stream.userId,broadcastId:stream.userId,startedAt:new Date(key.recordingStartMs).toISOString(),availableSeconds:20000};
 }};
 const warm=createReplayWarmup({streams,library:{enqueue:rows=>jobs.push(...rows)},now:()=>1000});
 await Promise.all([warm.prepare([key],{},[]),warm.prepare([key],{},[])]);
 assert.equal(peak,2);assert.equal(calls,10);assert.equal(jobs.length,10);
 await warm.prepare([key],{},[]);assert.equal(calls,10);
});
