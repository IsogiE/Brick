import assert from 'node:assert/strict';
import {test} from 'node:test';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
const source=readFileSync(new URL('../src/stream_player/clock.js',import.meta.url),'utf8').replace('__BRICK_WRAPPER_ORIGIN__',JSON.stringify('https://brick.example'));
function parentFixture(){
 let now=1000, listener, timer, sent;
 const child={postMessage:(data,origin)=>sent={data,origin}};
 const frame={src:'https://player.twitch.tv/?video=v1',contentWindow:child};
 const state={ready:true,playing:true,buffering:false,blocked:false,seconds:123};
 const window={brickMedia:{state:()=>state},addEventListener:(t,f)=>listener=f};window.top=window;
 const frames=[frame];
 runInNewContext(source,{window,location:{origin:'https://brick.example'},URL,performance:{now:()=>now},setInterval:f=>timer=f,document:{querySelectorAll:()=>frames}});
 return {window,state,frames,advance:n=>now+=n,request:()=>{timer();return sent;},reply:(data={},origin='https://player.twitch.tv',from=child)=>listener({source:from,origin,data:{type:'brick-replay-clock-reply',serial:sent.data.serial,seconds:123.25,playing:true,rate:1,...data}})};
}
test('media clock requires the owned iframe, matching request and affirmative playback',()=>{
 const f=parentFixture();f.request();
 f.reply({},'https://evil.example');assert.equal(f.window.brickReplayClock(),null);
 f.reply({},'https://player.twitch.tv',{});assert.equal(f.window.brickReplayClock(),null);
 f.reply({serial:999});assert.equal(f.window.brickReplayClock(),null);
 f.advance(10);f.reply();
 assert.equal(f.window.brickReplayClock().seconds,123.255);
 f.state.blocked=true;assert.equal(f.window.brickReplayClock(),null);f.state.blocked=false;
 f.state.playing=false;assert.equal(f.window.brickReplayClock(),null);f.state.playing=true;
 f.advance(101);assert.equal(f.window.brickReplayClock(),null);
 f.request();f.reply({playing:false});assert.equal(f.window.brickReplayClock(),null);
 f.request();f.advance(251);f.reply();assert.equal(f.window.brickReplayClock(),null);
 f.frames.push({...f.frames[0]});f.request();assert.equal(f.window.brickReplayClock(),null);
});
test('provider clock rejects seeking, paused and ambiguous video and foreign parents',()=>{
 let listener, sent, now=1000;
 const parent={postMessage:(data,origin)=>sent={data,origin}};
 const window={parent,top:parent,addEventListener:(t,f)=>listener=f};
 const video={addEventListener:()=>{},removeEventListener:()=>{},getBoundingClientRect:()=>({width:1920,height:1080}),videoWidth:1920,videoHeight:1080,currentTime:42.123,playbackRate:1,paused:false,ended:false,seeking:false,readyState:4};
 const videos=[video];
 runInNewContext(source,{window,performance:{now:()=>now},location:{origin:'https://player.twitch.tv'},document:{querySelectorAll:()=>videos}});
 const request=(origin='https://brick.example',from=parent)=>listener({source:from,origin,data:{type:'brick-replay-clock-request',serial:4}});
 request('https://evil.example');assert.equal(sent,undefined);request('https://brick.example',{});assert.equal(sent,undefined);
 request();assert.equal(sent.origin,'https://brick.example');assert.equal(sent.data.seconds,42.123);assert.equal(sent.data.playing,false);
 video.currentTime+=0.1;now+=100;request();assert.equal(sent.data.playing,true);
 now+=1001;request();assert.equal(sent.data.playing,false,'a frozen media clock cannot remain playing');
 video.currentTime+=0.1;request();assert.equal(sent.data.playing,true);
 for(const key of ['paused','ended','seeking']){video[key]=true;request();assert.equal(sent.data.playing,false);video[key]=false;}
 video.readyState=2;request();assert.equal(sent.data.playing,false);video.readyState=4;
 videos.push({...video});request();assert.equal(sent.data.playing,false);
});

const frameSource=readFileSync(new URL('../src/stream_player/frame.js',import.meta.url),'utf8').replace('__BRICK_WRAPPER_ORIGIN__',JSON.stringify('https://brick.example'));
test('source pixels remain native size in a small viewport and honor playback and origin guards',()=>{
 let callback,reply,draw,now=100;
 const parent={postMessage:(data,origin)=>reply={data,origin}};
 const window={parent,top:parent,addEventListener:(_,f)=>callback=f};
 const video={getBoundingClientRect:()=>({width:800,height:450}),videoWidth:3840,videoHeight:2160,currentTime:120.625,playbackRate:1,paused:false,ended:false,seeking:false,readyState:4};
 const canvas={width:0,height:0,getContext:()=>({drawImage:(...args)=>draw={args,width:canvas.width,height:canvas.height}}),toDataURL:()=> 'data:image/png;base64,AA=='};
 runInNewContext(frameSource,{window,parent,location:{origin:'https://player.twitch.tv'},performance:{now:()=>now},document:{querySelectorAll:()=>[video],createElement:()=>canvas}});
 const request=(origin='https://brick.example')=>callback({source:parent,origin,data:{type:'brick-video-frame-request',serial:1}});
 request('https://evil.example');assert.equal(reply,undefined);
 request();assert.equal(reply.origin,'https://brick.example');assert.equal(reply.data.width,1920);assert.equal(reply.data.height,720);
 assert.equal(draw.args[3],3840);assert.equal(draw.args[4],2160);
 assert.equal(reply.data.before,120.625);assert.equal(canvas.width,0,'release pixel storage after encoding');
 reply=null;now+=100;video.seeking=true;request();assert.equal(reply,null);
 video.seeking=false;video.videoWidth=10000;request();assert.equal(reply,null);
});
test('source frame replies require the matching provider, request, dimensions and fresh playback',()=>{
 let callback,sent,now=0;const child={postMessage:d=>sent=d};
 const frame={src:'https://player.twitch.tv/',contentWindow:child};const state={ready:true,playing:true,buffering:false,blocked:false,seconds:10};
 const window={brickMedia:{state:()=>state},addEventListener:(_,f)=>callback=f};window.top=window;
 runInNewContext(frameSource,{window,URL,location:{origin:'https://brick.example'},performance:{now:()=>now},setInterval:()=>{},document:{querySelectorAll:()=>[frame]}});
 window.brickReplayRequestFrame(7);
 const reply=(extra={},origin='https://player.twitch.tv',from=child)=>callback({source:from,origin,data:{type:'brick-video-frame-reply',serial:sent.serial,width:960,height:360,before:10,after:10.01,png:'data:image/png;base64,AA==',...extra}});
 reply({},'https://evil.example');assert.equal(window.brickReplayTakeFrame(7),null);
 reply({},'https://player.twitch.tv',{});assert.equal(window.brickReplayTakeFrame(7),null);
 reply({serial:sent.serial+1});assert.equal(window.brickReplayTakeFrame(7),null);
 reply();assert.equal(window.brickReplayTakeFrame(8),null);assert.equal(window.brickReplayTakeFrame(7).before,10);assert.equal(window.brickReplayTakeFrame(7),null);
 window.brickReplayRequestFrame(8);reply({width:4096});assert.equal(window.brickReplayTakeFrame(8),null);
 window.brickReplayRequestFrame(9);state.blocked=true;reply();assert.equal(window.brickReplayTakeFrame(9),null);state.blocked=false;
 window.brickReplayRequestFrame(10);reply();now=501;assert.equal(window.brickReplayTakeFrame(10),null);
});

test('native playback requires decoded and progressing media even when SDK PLAYING stays latched',()=>{
 const f=parentFixture();f.request();
 f.reply({seconds:145,decoded:false,readyState:1,frames:0,playing:false,seeking:true});
 assert.equal(f.window.brickPlaybackState().playing,false);
 assert.equal(f.window.brickPlaybackState().buffering,true);
 assert.equal(f.window.brickPlaybackState().decoded,false);
 f.state.seconds=145;f.request();
 f.reply({seconds:145.25,decoded:true,readyState:4,frames:25,playing:true,seeking:false});
 assert.equal(f.window.brickPlaybackState().playing,true);
 assert.equal(f.window.brickPlaybackState().buffering,false);
 f.request();f.reply({seconds:145.25,decoded:true,readyState:2,frames:25,playing:false,paused:false});
 assert.equal(f.window.brickPlaybackState().playing,false);
 assert.equal(f.window.brickPlaybackState().buffering,true);
 f.advance(501);assert.equal(f.window.brickPlaybackState().playing,false);
});
