// Sample the owned provider's media element; Twitch's public SDK clock is cached.
(() => {
  const wrapperOrigin = __BRICK_WRAPPER_ORIGIN__;
  const providers = new Set(['https://player.twitch.tv', 'https://www.youtube.com', 'https://www.youtube-nocookie.com']);
  const requestType = 'brick-replay-clock-request';
  const replyType = 'brick-replay-clock-reply';
  if (window === window.top) {
    if (location.origin !== wrapperOrigin) return;
    let serial = 0, pending = null, latest = null;
    window.addEventListener('message', event => {
      if (!pending || event.source !== pending.frame.contentWindow || event.origin !== pending.origin) return;
      const data = event.data;
      if (!data || data.type !== replyType || data.serial !== pending.serial) return;
      const now = performance.now();
      const elapsed = now - pending.sent;
      latest = elapsed <= 250 && Number.isFinite(data.seconds) && data.seconds >= 0 && data.seconds <= 604800
        && typeof data.playing === 'boolean' && data.rate === 1
        ? { seconds: data.seconds, height: data.height, playing: data.playing,
          paused: data.paused === true, seeking: data.seeking === true,
          decoded: data.decoded === true, ended: data.ended === true,
          readyState: data.readyState, frames: data.frames, at: now, elapsed } : null;
      pending = null;
    });
    setInterval(() => {
      const now = performance.now();
      if (pending && now - pending.sent < 250) return;
      pending = null;
      const frames = [...document.querySelectorAll('iframe')].filter(frame => {
        try { return providers.has(new URL(frame.src).origin); } catch (_) { return false; }
      });
      if (frames.length !== 1) { latest = null; return; }
      const frame = frames[0];
      const origin = new URL(frame.src).origin;
      pending = { frame, origin, serial: ++serial, sent: now };
      frame.contentWindow?.postMessage({ type: requestType, serial }, origin);
    }, 100);
    window.brickReplayMedia = () => latest && performance.now()-latest.at<=500 ? latest : null;
    window.brickPlaybackState = () => {
      const state = window.brickMedia?.state();
      if (!state) return null;
      const media = window.brickReplayMedia();
      if (!media) return { ...state, playing: false, buffering: state.ready && !state.blocked, decoded: false };
      return { ...state, seconds: media.seconds, decoded: media.decoded,
        playing: state.playing && media.playing && !media.seeking,
        buffering: !state.blocked && (state.buffering || media.seeking || !media.decoded || (!media.paused && !media.ended && !media.playing)),
        diagnostics: { ...state.diagnostics, media_seconds: media.seconds, ready_state: media.readyState,
          decoded_frames: media.frames, media_seeking: media.seeking } };
    };
    window.brickReplaySourceHeight = () => latest && performance.now()-latest.at<=100 ? latest.height : 0;
    window.brickReplayClock = () => {
      const state = window.brickMedia?.state();
      const age = latest ? performance.now() - latest.at : Infinity;
      if (!state?.ready || !state.playing || state.buffering || state.blocked || !latest?.playing || age > 100
          || Math.abs(state.seconds - latest.seconds) > 2) return null;
      // Interpolate only the bounded message age; paused/seeking media cannot
      // supply a sample. Native capture retains a conservative timing margin.
      return { ...state, seconds: latest.seconds + (age + latest.elapsed / 2) / 1000 };
    };
    return;
  }
  if (window.parent !== window.top || !providers.has(location.origin)) return;
  let previousVideo = null, previousSeconds = null, progressed = -Infinity;
  window.addEventListener('message', event => {
    if (event.source !== window.parent || event.origin !== wrapperOrigin) return;
    const data = event.data;
    if (!data || data.type !== requestType || !Number.isSafeInteger(data.serial)) return;
    const videos = [...document.querySelectorAll('video')].filter(video => {
      const bounds = video.getBoundingClientRect();
      return bounds.width >= 160 && bounds.height >= 90;
    });
    const video = videos.length === 1 ? videos[0] : null;
    const now = performance.now();
    if (video !== previousVideo) { previousSeconds = null; progressed = -Infinity; previousVideo = video; }
    if (video && previousSeconds !== null && video.currentTime > previousSeconds && !video.seeking) progressed = now;
    previousSeconds = video?.currentTime ?? null;
    const frames = video?.getVideoPlaybackQuality?.().totalVideoFrames;
    const decoded = !!video && video.videoWidth > 0 && video.videoHeight > 0 && video.readyState >= 2
      && (frames === undefined || frames > 0);
    window.parent.postMessage({ type: replyType, serial: data.serial,
      seconds: video?.currentTime, height: video?.videoHeight, rate: video?.playbackRate,
      decoded, frames, readyState: video?.readyState, paused: video?.paused, ended: video?.ended, seeking: video?.seeking,
      playing: decoded && !video.paused && !video.ended && !video.seeking && video.readyState >= 3 && now-progressed <= 1000,
    }, wrapperOrigin);
  });
})();
