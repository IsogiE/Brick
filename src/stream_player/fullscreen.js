// Linux media expands inside Brick. Do not enter WebKit's native fullscreen
// state: the 2.50.4 runtime shipped for Ubuntu 22.04 aborts in that transition.
(() => {
  const providers = new Set(['https://player.twitch.tv', 'https://www.youtube.com', 'https://www.youtube-nocookie.com']);
  const topLevel = window === window.top;
  if (!topLevel && (window.parent !== window.top || !providers.has(location.origin))) return;
  // Twitch can consume WebKit's activation while handling a trusted click.
  // This only changes Brick's layout, so retain that gesture briefly for it.
  let lastGesture = -Infinity;
  const gesture = event => { if (event.isTrusted) lastGesture = Date.now(); };
  window.addEventListener('click', gesture, true);
  let element = null;
  let savedStyle = null;
  let child = null;
  const properties = { position: 'fixed', top: '0', right: '0', bottom: '0', left: '0', width: '100vw', height: '100vh', 'max-width': 'none', 'max-height': 'none', 'z-index': '2147483647' };

  function changed(previous) {
    const target = element || (previous?.isConnected ? previous : document);
    // Native fullscreen changes arrive after the request returns. Providers
    // finish registering their state handlers during that request.
    setTimeout(() => {
      target.dispatchEvent(new Event('fullscreenchange', { bubbles: true }));
      target.dispatchEvent(new Event('webkitfullscreenchange', { bubbles: true }));
    }, 0);
  }
  function notify() {
    if (topLevel) {
      window.webkit?.messageHandlers?.brickFullscreen?.postMessage(element ? 'enter' : 'exit');
    } else {
      // The parent checks both the browser-supplied origin and frame identity.
      window.parent.postMessage(element ? 'brick-fullscreen-enter' : 'brick-fullscreen-leave', '*');
    }
  }
  function clear() {
    const previous = element;
    if (!previous) return;
    if (savedStyle) {
      for (const [name, value, priority] of savedStyle) {
        if (value) previous.style.setProperty(name, value, priority);
        else previous.style.removeProperty(name);
      }
    }
    element = null;
    savedStyle = null;
    child = null;
    changed(previous);
    notify();
  }
  function exit() {
    if (child) child.frame.contentWindow?.postMessage('brick-fullscreen-exit', child.origin);
    clear();
    return Promise.resolve();
  }
  function enter() {
    if (!(this instanceof Element) || !this.isConnected || (!navigator.userActivation?.isActive && Date.now() - lastGesture > 1000)) {
      return Promise.reject(new TypeError('Fullscreen requires an active user gesture.'));
    }
    if (element === this) return Promise.resolve();
    exit();
    element = this;
    savedStyle = Object.keys(properties).map(name => [name, this.style.getPropertyValue(name), this.style.getPropertyPriority(name)]);
    for (const [name, value] of Object.entries(properties)) this.style.setProperty(name, value, 'important');
    changed(null);
    notify();
    return Promise.resolve();
  }
  // Disabling native fullscreen also removes WebKit's event-handler slots.
  // Provider feature detection needs these before it subscribes to changes.
  for (const type of ['fullscreenchange', 'webkitfullscreenchange', 'fullscreenerror', 'webkitfullscreenerror']) {
    const name = `on${type}`;
    if (name in document) continue;
    let handler = null;
    Object.defineProperty(document, name, {
      configurable: true,
      get: () => handler,
      set: next => {
        if (handler) document.removeEventListener(type, handler);
        handler = typeof next === 'function' ? next : null;
        if (handler) document.addEventListener(type, handler);
      },
    });
  }
  for (const name of ['requestFullscreen', 'webkitRequestFullscreen', 'webkitRequestFullScreen']) {
    Object.defineProperty(Element.prototype, name, { configurable: true, writable: true, value: enter });
  }
  for (const name of ['exitFullscreen', 'webkitExitFullscreen', 'webkitCancelFullScreen']) {
    Object.defineProperty(document, name, { configurable: true, value: exit });
  }
  for (const name of ['fullscreenElement', 'webkitFullscreenElement', 'webkitCurrentFullScreenElement']) {
    Object.defineProperty(document, name, { configurable: true, get: () => element });
  }
  for (const name of ['fullscreenEnabled', 'webkitFullscreenEnabled']) {
    Object.defineProperty(document, name, { configurable: true, get: () => true });
  }
  for (const name of ['fullscreen', 'webkitIsFullScreen']) {
    Object.defineProperty(document, name, { configurable: true, get: () => !!element });
  }
  window.addEventListener('keydown', event => {
    gesture(event);
    if (event.key === 'Escape') exit();
  }, true);
  window.addEventListener('pagehide', exit);
  window.addEventListener('message', event => {
    if (!topLevel) {
      if (event.source === window.parent && event.data === 'brick-fullscreen-exit') clear();
      return;
    }
    if (!providers.has(event.origin)) return;
    const frame = [...document.querySelectorAll('iframe')].find(frame => frame.contentWindow === event.source);
    if (!frame) return;
    if (event.data === 'brick-fullscreen-enter') {
      if (element !== frame) {
        exit();
        element = frame;
        child = { frame, origin: event.origin };
        changed(null);
        notify();
      }
    } else if (event.data === 'brick-fullscreen-leave' && element === frame) {
      clear();
    }
  });
})();
