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
    state: () => ({ ready, seconds: ready ? player.getCurrentTime() : pending,
      playing: ready && !priming && player.getPlayerState() === 1,
      buffering: ready && (priming !== null || player.getPlayerState() === 3) })
  };
};`;
export const youtubeScriptPolicy = `script-src 'sha256-${createHash("sha256").update(youtubeControls).digest("base64")}' https://www.youtube.com/iframe_api https://www.youtube.com/s/player/; `;
export const youtubeControlTags = `<script>${youtubeControls}</script><script src="https://www.youtube.com/iframe_api"></script>`;

export const twitchControls = `(() => {
  const url = new URL(document.getElementById("media").dataset.src);
  let pending = Number((url.searchParams.get("time") || "0s").replace(/s$/, ""));
  let ready = false;
  let playing = false;
  let resume = url.searchParams.get("autoplay") !== "false";
  let decoded = false;
  let priming = null;
  const video = url.searchParams.get("video");
  const player = new Twitch.Player("media", {width:"100%", height:"100%",
    video, parent:[url.searchParams.get("parent")],
    time:Math.floor(pending) + "s", autoplay:resume, muted:true});
  const load = () => {
    playing = false;
    priming = resume ? null : "starting";
    player.seek(pending);
    player.play();
  };
  const apply = () => {
    playing = false;
    if (!resume) player.pause();
    player.seek(pending);
    if (resume) player.play(); else player.pause();
  };
  player.addEventListener(Twitch.Player.READY, () => {
    ready = true;
    player.setMuted(true);
    load();
  });
  for (const event of [Twitch.Player.PLAY, Twitch.Player.SEEK, Twitch.Player.PAUSE, Twitch.Player.ENDED]) {
    player.addEventListener(event, () => { playing = false; });
  }
  player.addEventListener(Twitch.Player.PLAYING, () => {
    const first = !decoded;
    playing = true; decoded = true;
    if (first && resume) player.seek(pending);
    if (!resume && priming === "starting") {
      playing = false; priming = "pausing"; player.pause();
    }
  });
  player.addEventListener(Twitch.Player.PAUSE, () => {
    if (!resume && priming === "pausing") {
      priming = null; player.seek(pending);
    }
  });
  window.brickMedia = {
    seek: (seconds, shouldResume = true) => {
      if (!Number.isFinite(seconds) || seconds < 0 || seconds > 604800 || typeof shouldResume !== "boolean") return;
      pending = seconds;
      resume = shouldResume;
      if (ready) { if (!decoded) load(); else apply(); }
    },
    pause: () => {
      resume = false; playing = false;
      if (ready) { if (!decoded) load(); else if (!priming) player.pause(); }
    },
    play: () => {
      resume = true; priming = null;
      if (ready) { if (!decoded) load(); else player.play(); }
    },
    state: () => ({ ready, seconds: ready ? player.getCurrentTime() : pending,
      playing: ready && playing && !player.isPaused() && !player.getEnded(),
      buffering: ready && (priming !== null || (!playing && !player.isPaused() && !player.getEnded())) })
  };
})();`;
export const playerScriptPolicy = youtubeScriptPolicy.replace("; ", ` 'sha256-${createHash("sha256").update(twitchControls).digest("base64")}' https://player.twitch.tv/js/embed/v1.js; `);
export const twitchControlTags = `<script src="https://player.twitch.tv/js/embed/v1.js"></script><script>${twitchControls}</script>`;
