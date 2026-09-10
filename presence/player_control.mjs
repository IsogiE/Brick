import { createHash } from "node:crypto";

// Only playback controls live in the wrapper. Private reports and credentials
// never enter this page or the provider's player API.
export const youtubeControls = `window.onYouTubeIframeAPIReady = () => {
  const frame = document.getElementById("media");
  const url = new URL(frame.src);
  const replay = frame.dataset.start !== undefined;
  const start = Number(frame.dataset.start || url.searchParams.get("start") || 0);
  let ready = false;
  let pending = start;
  let resume = url.searchParams.get("autoplay") !== "0";
  let priming = null;
  let decoded = false;
  let syncQuality = false, previousStyle = null;
  const fitSync = () => {
    if (!syncQuality || !(window.innerWidth > 0 && window.innerHeight > 0)) return;
    const scale=Math.min(1,window.innerWidth/1920,window.innerHeight/1080);
    frame.style.width=window.innerWidth/scale+"px";
    frame.style.height=window.innerHeight/scale+"px";
    frame.style.transformOrigin="top left";
    frame.style.transform=scale<1?"scale("+scale+")":"";
  };
  window.addEventListener?.("resize",fitSync);
  const load = () => {
    priming = resume ? null : "starting";
    player.loadVideoById({videoId: url.pathname.split("/").pop(), startSeconds: pending});
  };
  const apply = () => {
    // A never-played paused iframe can report its target while painting black.
    // Decode its first frame muted, then seek again once the SDK is paused.
    if (!resume && priming === "starting") {
      player.seekTo(pending, true);
      player.playVideo();
      return;
    }
    if (!resume) player.pauseVideo();
    player.seekTo(pending, true);
    if (resume) player.playVideo(); else player.pauseVideo();
  };
  const player = new YT.Player("media", {events: {onReady: event => {
    ready = true;
    event.target.mute();
    if (replay) load(); else event.target.playVideo();
    // WebKit may announce readiness before its initial media setup accepts
    // playback. Retry preparation once after that setup, without seeking again.
    if (replay) setTimeout(() => {
      if (!decoded && [-1, 5].includes(player.getPlayerState())) load();
    }, 500);
  }, onStateChange: event => {
    if (event.data === 1) decoded = true;
    if (resume) { priming = null; return; }
    if (priming === "starting" && event.data === 1) {
      priming = "pausing";
      player.pauseVideo();
    } else if (priming === "pausing" && event.data === 2) {
      priming = null;
      player.seekTo(pending, true);
    }
  }}});
  window.brickMedia = {
    prepareSync: enabled => {
      if(typeof enabled!=="boolean" || !replay)return;
      if(enabled && !syncQuality)previousStyle={width:frame.style.width,height:frame.style.height,transform:frame.style.transform,transformOrigin:frame.style.transformOrigin};
      if(!enabled && syncQuality && previousStyle)Object.assign(frame.style,previousStyle);
      syncQuality=enabled;fitSync();
    },
    seek: (seconds, shouldResume = true) => {
      if (!Number.isFinite(seconds) || seconds < 0 || seconds > 604800 || typeof shouldResume !== "boolean") return;
      pending = seconds;
      resume = shouldResume;
      if (ready) { if (!decoded) load(); else apply(); }
    },
    pause: () => {
      resume = false;
      if (ready && !decoded) load();
      else if (ready && !priming) player.pauseVideo();
    },
    play: () => {
      resume = true; priming = null;
      if (ready) { if (!decoded) load(); else player.playVideo(); }
    },
    state: () => ({ sync_ready: !syncQuality || (window.brickReplaySourceHeight?.() || 0)>=720, ready, seconds: ready ? player.getCurrentTime() : pending,
      playing: ready && !priming && player.getPlayerState() === 1,
      buffering: ready && (priming !== null || player.getPlayerState() === 3) })
  };
};`;
export const youtubeScriptPolicy = `script-src 'sha256-${createHash("sha256").update(youtubeControls).digest("base64")}' https://www.youtube.com/iframe_api https://www.youtube.com/s/player/; `;
export const youtubeControlTags = `<script>${youtubeControls}</script><script src="https://www.youtube.com/iframe_api"></script>`;

export const twitchControls = `(() => {
  const container = document.getElementById("media");
  const url = new URL(container.dataset.src);
  // Keep Twitch's consent UI inside its supported viewport even in a compact
  // native pane. Scale the complete embed so every provider control stays visible.
  const fit = () => {
    const width=window.innerWidth, height=window.innerHeight;
    if (!(width > 0 && height > 0)) return;
    const scale=Math.min(1,width/400,height/300);
    container.style.width=width/scale + "px";
    container.style.height=height/scale + "px";
    container.style.transformOrigin="top left";
    container.style.transform=scale<1 ? "scale(" + scale + ")" : "";
  };
  window.addEventListener?.("resize",fit);
  fit();
  let pending = Number((url.searchParams.get("time") || "0s").replace(/s$/, ""));
  let ready = false;
  let playing = false;
  let blocked = false;
  let pauseIntent = false;
  let playIntent = false;
  let expectedPause = false;
  let resume = url.searchParams.get("autoplay") !== "false";
  let decoded = false;
  let priming = null;
  let pauseThenSeek = false;
  let blockedSeek = false;
  let syncQuality = false, previousQuality = null, requestedQuality = null;
  const video = url.searchParams.get("video");
  const replay = !!video;
  const player = new Twitch.Player("media", {width:"100%", height:"100%",
    ...(replay ? {video, time:Math.floor(pending) + "s"} : {channel:url.searchParams.get("channel")}),
    parent:[url.searchParams.get("parent")], autoplay:resume, muted:true});
  const pause = () => {
    // Preserve command provenance until its event, even if a native Play is
    // submitted first. Actual PLAY/PLAYING starts a new provider interaction.
    expectedPause = true;
    player.pause();
  };
  const load = () => {
    if (replay) priming = resume ? null : "starting";
    if (blocked) return;
    if (!replay) { if (resume) player.play(); else pause(); return; }
    playing = false;
    priming = resume ? null : "starting";
    // Decode once before the precise seek. Seeking at READY can abort the
    // pending first play while the provider prepares its media pipeline.
    player.play();
  };
  const apply = () => {
    if (blocked || !replay) return;
    if (!resume) {
      playing = false;
      if (!player.isPaused()) {
        pauseThenSeek = true;
        pause();
        return;
      }
    }
    pauseThenSeek = false;
    // A redundant pause around a backwards seek can discard that seek in
    // Twitch. Seek only after pause is acknowledged, without pausing again.
    player.seek(pending);
    if (resume) { priming = null; player.play(); }
  };
  player.addEventListener(Twitch.Player.READY, () => {
    ready = true;
    player.setMuted(true);
    load();
  });
  player.addEventListener(Twitch.Player.PLAYBACK_BLOCKED, () => {
    blocked = true;
    playing = false;
    const notice = document.getElementById("playback-notice");
    if (notice) { notice.textContent = "Playback didn’t start. Use Play or any prompt in the Twitch player."; notice.hidden = false; }
  });
  // SEEK is a position notification, not a playback-state transition. Twitch
  // need not emit PLAYING again when a seek continues an already playing video.
  for (const event of [Twitch.Player.PLAY, Twitch.Player.PAUSE, Twitch.Player.ENDED]) {
    player.addEventListener(event, () => {
      playing = false;
      if (blocked) return;
      pauseIntent = event === Twitch.Player.PAUSE && !expectedPause;
      expectedPause = false;
      if (event === Twitch.Player.PLAY) {
        if (decoded && !priming && !resume) playIntent = true;
      } else playIntent = false;
    });
  }
  player.addEventListener(Twitch.Player.PLAYING, () => {
    const wasBlocked = blocked;
    blocked = false;
    const notice = document.getElementById("playback-notice");
    if (notice) notice.hidden = true;
    const first = !decoded;
    pauseIntent = false;
    expectedPause = false;
    playing = true; decoded = true;
    if (replay && first && resume) player.seek(pending);
    if (replay && wasBlocked && !first && (blockedSeek || pauseThenSeek)) apply();
    blockedSeek = false;
    if (!resume && priming === "starting") {
      playing = false; priming = "pausing"; pause();
    }
  });
  player.addEventListener(Twitch.Player.PAUSE, () => {
    if (!blocked && replay && !pauseThenSeek && !resume && priming === "pausing") {
      priming = null; player.seek(pending);
    }
  });
  window.brickMedia = {
    prepareSync: enabled => {
      if (typeof enabled !== "boolean" || !replay) return;
      if (enabled && !syncQuality) { previousQuality = ready ? player.getQuality() : "auto"; requestedQuality = null; }
      if (!enabled && syncQuality && ready && previousQuality) player.setQuality(previousQuality);
      syncQuality = enabled;
    },
    seek: (seconds, shouldResume = true) => {
      if (!replay || !Number.isFinite(seconds) || seconds < 0 || seconds > 604800 || typeof shouldResume !== "boolean") return;
      pauseIntent = false;
      playIntent = false;
      pending = seconds;
      blockedSeek = blocked;
      resume = shouldResume;
      if (ready) { if (!decoded) load(); else if (!pauseThenSeek) apply(); }
    },
    pause: () => {
      pauseIntent = false;
      playIntent = false;
      resume = false; playing = false;
      if (replay && !decoded) priming = "starting";
      if (ready && !blocked) { if (!decoded) load(); else if (!priming && !pauseThenSeek) pause(); }
    },
    play: () => {
      pauseIntent = false;
      playIntent = false;
      resume = true; priming = null;
      // An explicit native Play is a new attempt, even after AbortError.
      // Keep the blocked state until Twitch confirms playback; polling does
      // not retry and provider consent still uses its own visible controls.
      if (ready && (blocked || !pauseThenSeek)) player.play();
    },
    state: () => {
      // Twitch can emit PAUSE before the transition accepts a new seek. Wait
      // for a later SDK read, outside that callback, without another pause or
      // a timer. This also handles a missed event when the SDK is already paused.
      if (ready && !blocked && replay && pauseThenSeek && player.isPaused()) {
        pauseThenSeek = false;
        player.seek(pending);
        if (resume) { priming = null; player.play(); }
      }
      const stats = ready ? player.getPlaybackStats?.() : null;
      let syncReady = !syncQuality;
      if (syncQuality && ready && !blocked) {
        const choices = player.getQualities().map(quality => typeof quality === "string" ? quality : quality?.group)
          .filter(quality => typeof quality === "string" && (quality === "chunked" || /^[0-9]+p[0-9]*$/.test(quality)));
        const rank = quality => quality === "chunked" ? 10000 : Number.parseInt(quality, 10);
        const source = choices.sort((a,b) => rank(b)-rank(a))[0];
        if (source && rank(source) >= 720) {
          if (!requestedQuality) { player.setQuality(source); requestedQuality = source; }
          const height = Number((stats?.videoResolution || "").split("x")[1]);
          syncReady = player.getQuality() === requestedQuality && height >= 720;
        }
      }
      return { sync_ready: syncReady, diagnostics: {source: replay ? video : url.searchParams.get("channel"), buffer_seconds: stats?.bufferSize, fps: stats?.fps}, ready, blocked, seconds: replay && ready ? player.getCurrentTime() : pending,
        pause_intent: pauseIntent,
        play_intent: playIntent,
        playing: ready && !blocked && !pauseThenSeek && playing && !player.isPaused() && !player.getEnded(),
        buffering: ready && !blocked && (pauseThenSeek || priming !== null || (!playing && !player.isPaused() && !player.getEnded())) };
    }
  };
})();`;
export const playerScriptPolicy = youtubeScriptPolicy.replace("; ", ` 'sha256-${createHash("sha256").update(twitchControls).digest("base64")}' https://player.twitch.tv/js/embed/v1.js; `);
export const twitchControlTags = `<script src="https://player.twitch.tv/js/embed/v1.js"></script><script>${twitchControls}</script>`;
