(() => {
  if (window === window.top || window.parent !== window.top || location.origin !== 'https://player.twitch.tv') return;
  const key = 'content-classification-labels-acknowledged';
  const allowed = ['DebatedSocialIssuesAndPolitics', 'DrugsIntoxication', 'Gambling', 'MatureGame', 'ProfanityVulgarity', 'SexualThemes', 'ViolentGraphic'];
  const parentOrigin = __BRICK_WRAPPER_ORIGIN__;
  const remembered = __BRICK_ACKNOWLEDGEMENTS__;
  let storage;
  try {
    storage = window.localStorage;
    if (storage.getItem(key) === null && Object.keys(remembered).length) {
      storage.setItem(key, JSON.stringify({ loggedIn: {}, loggedOut: remembered }));
    }
  } catch { return; }

  const publish = () => {
    try {
      const raw = storage.getItem(key);
      if (raw !== null && raw.length > 4096) return;
      const source = raw === null ? {} : JSON.parse(raw).loggedOut;
      if (!source || typeof source !== 'object' || Array.isArray(source)) return;
      const now = Date.now();
      const acknowledgements = {};
      for (const label of allowed) {
        const expiry = source[label];
        if (Number.isSafeInteger(expiry) && expiry > now && expiry <= now + 2592000000) {
          acknowledgements[label] = expiry;
        }
      }
      window.parent.postMessage(JSON.stringify({ kind: 'brick-twitch-consent', acknowledgements }), parentOrigin);
    } catch { /* Storage denial or a changed provider format leaves its normal prompt intact. */ }
  };

  // Capture actual provider writes after the user acknowledges a label. The
  // storage event alone excludes writes made by this same document.
  for (const operation of ['setItem', 'removeItem', 'clear']) {
    const descriptor = Object.getOwnPropertyDescriptor(Storage.prototype, operation);
    const original = descriptor.value;
    Object.defineProperty(Storage.prototype, operation, {
      ...descriptor,
      value: function (...args) {
        const result = Reflect.apply(original, this, args);
        if (this === storage && (operation === 'clear' || args[0] === key)) publish();
        return result;
      },
    });
  }
  window.addEventListener('storage', event => {
    if (event.storageArea === storage && (event.key === key || event.key === null)) publish();
  });
})();
