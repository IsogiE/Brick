(() => {
  const wrapperOrigin = __BRICK_WRAPPER_ORIGIN__;
  const origins = new Set(['https://player.twitch.tv','https://www.youtube.com','https://www.youtube-nocookie.com']);
  const requestType='brick-video-frame-request', replyType='brick-video-frame-reply';
  const limit=12*1024*1024;
  if (window === window.top) {
    if (location.origin !== wrapperOrigin) return;
    let serial=0, pending=null, result=null;
    const playable=()=>{const s=window.brickMedia?.state();return s?.ready && s.playing && !s.buffering && !s.blocked;};
    window.brickReplayRequestFrame=generation=>{
      pending=null;result=null;
      if (!Number.isSafeInteger(generation) || !playable()) return;
      const frames=[...document.querySelectorAll('iframe')].filter(f=>{try{return origins.has(new URL(f.src).origin);}catch(_){return false;}});
      if(frames.length!==1)return;
      const frame=frames[0],origin=new URL(frame.src).origin;
      pending={generation,serial:++serial,frame,origin,sent:performance.now()};
      frame.contentWindow.postMessage({type:requestType,serial},origin);
    };
    window.addEventListener('message',event=>{
      const p=pending,d=event.data;
      if(!p || event.source!==p.frame.contentWindow || event.origin!==p.origin || d?.type!==replyType || d.serial!==p.serial)return;
      pending=null;
      if(performance.now()-p.sent>2000 || !playable())return;
      if(typeof d.png!=='string' || d.png.length>limit || !d.png.startsWith('data:image/png;base64,')
        || !Number.isInteger(d.width) || !Number.isInteger(d.height) || d.width<1 || d.width>2048 || d.height<1 || d.height>1024
        || !Number.isFinite(d.before) || !Number.isFinite(d.after) || d.before<0 || d.after>604800 || d.after<d.before || d.after-d.before>0.15)return;
      const seconds=window.brickMedia.state().seconds;
      if(!Number.isFinite(seconds) || Math.abs(seconds-d.after)>2)return;
      result={generation:p.generation,width:d.width,height:d.height,before:d.before,after:d.after,png:d.png,at:performance.now()};
    });
    window.brickReplayTakeFrame=generation=>{
      if(!result || result.generation!==generation)return null;
      const value=result;result=null;
      return performance.now()-value.at<=500 && playable()?value:null;
    };
    setInterval(()=>{
      if(pending && performance.now()-pending.sent>2000)pending=null;
      if(result && performance.now()-result.at>500)result=null;
    },500);
    return;
  }
  if(window.parent!==window.top || !origins.has(location.origin))return;
  let busy=false,last=-Infinity,canvas=null;
  window.addEventListener('message',event=>{
    const d=event.data;
    if(event.source!==window.parent || event.origin!==wrapperOrigin || d?.type!==requestType || !Number.isSafeInteger(d.serial)
      || busy || performance.now()-last<80)return;
    const videos=[...document.querySelectorAll('video')].filter(v=>{const r=v.getBoundingClientRect();return r.width>=160 && r.height>=90 && v.videoWidth>0 && v.videoHeight>0;});
    const v=videos.length===1?videos[0]:null;
    if(!v || v.paused || v.ended || v.seeking || v.readyState<3 || v.playbackRate!==1
      || v.videoWidth>8192 || v.videoHeight>8192 || v.videoWidth*v.videoHeight>8847360)return;
    busy=true;last=performance.now();
    try {
      canvas ||= document.createElement('canvas');
      canvas.width=Math.min(2048,Math.ceil(v.videoWidth/2));
      canvas.height=Math.min(1024,Math.ceil(v.videoHeight/3));
      const before=v.currentTime;
      canvas.getContext('2d',{alpha:false}).drawImage(v,0,0,v.videoWidth,v.videoHeight);
      const after=v.currentTime;
      const png=canvas.toDataURL('image/png');
      if(png.length<=limit && !v.seeking && !v.paused)parent.postMessage({type:replyType,serial:d.serial,width:canvas.width,height:canvas.height,before,after,png},wrapperOrigin);
    } catch (_) {
      // A provider may deny canvas access. An unreadable frame never aligns.
    } finally { if(canvas){canvas.width=0;canvas.height=0;}busy=false; }
  });
})();
