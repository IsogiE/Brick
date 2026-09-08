(() => {
  if (window !== window.top || location.href !== __BRICK_WRAPPER_URL__) return;
  // This nonce remains in the protected top-level document's closure. The
  // provider script receives neither it nor a general native API capability.
  const nonce = __BRICK_NONCE__;
  window.addEventListener('message', event => {
    if (event.origin !== 'https://player.twitch.tv' ||
        event.source !== document.querySelector('iframe')?.contentWindow ||
        typeof event.data !== 'string' || event.data.length > 4096) return;
    try {
      const message = JSON.parse(event.data);
      if (message.kind !== 'brick-twitch-consent') return;
      const body = JSON.stringify({ nonce, acknowledgements: message.acknowledgements });
      if (body.length > 4096) return;
      if (window.webkit?.messageHandlers?.brickConsent) {
        window.webkit.messageHandlers.brickConsent.postMessage(body);
      } else {
        window.ipc?.postMessage(body);
      }
    } catch { /* Invalid provider messages cannot invoke native operations. */ }
  });
})();
