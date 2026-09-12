// Discord supplies heartbeat timing, but cannot choose a new credential
// destination, create unbounded timers, or inject text into operational logs.
export function startDiscordGateway({ lookup, token, WebSocket = globalThis.WebSocket, logger = console }) {
  let stopped = false;
  let reconnectDelay = 5000;
  let reconnectTimer;
  let disconnect = () => {};
  const scheduleReconnect = () => {
    if (stopped || reconnectTimer) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      void connect();
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 60_000);
  };

  async function connect() {
    try {
      const gateway = await lookup();
      if (stopped) return;
      const url = new URL(gateway?.url);
      if (url.protocol !== 'wss:' || url.hostname !== 'gateway.discord.gg' || url.port
          || url.username || url.password || url.pathname !== '/' || url.search || url.hash) {
        throw new Error('Unexpected gateway address');
      }
    } catch {
      if (!stopped) logger.error('Discord Gateway lookup rejected');
      scheduleReconnect();
      return;
    }

    let socket;
    try { socket = new WebSocket('wss://gateway.discord.gg/?v=10&encoding=json'); }
    catch {
      logger.error('Discord Gateway connection failed');
      scheduleReconnect();
      return;
    }
    let closed = false;
    let helloReceived = false;
    let heartbeatTimer;
    let firstHeartbeat;
    let sequence = null;
    let acknowledged = true;
    const cleanup = () => {
      clearInterval(heartbeatTimer);
      clearTimeout(firstHeartbeat);
    };
    const close = () => {
      if (closed) return;
      closed = true;
      cleanup();
      try { socket.close(4000, 'reconnecting'); } catch { /* already closed */ }
    };
    const reconnect = () => {
      if (stopped || closed) return;
      close();
      scheduleReconnect();
    };
    disconnect = close;
    const send = payload => {
      if (!stopped && !closed && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(payload));
    };
    const heartbeat = () => {
      if (!acknowledged) { reconnect(); return; }
      acknowledged = false;
      send({ op: 1, d: sequence });
    };

    socket.addEventListener('open', () => { reconnectDelay = 5000; });
    socket.addEventListener('message', event => {
      if (stopped || closed) return;
      // Gateway JSON is text. Reject binary/oversized data before parsing it.
      if (typeof event.data !== 'string' || Buffer.byteLength(event.data) > 64 * 1024) { reconnect(); return; }
      let payload;
      try { payload = JSON.parse(event.data); } catch { reconnect(); return; }
      if (!payload || typeof payload !== 'object' || Array.isArray(payload)) { reconnect(); return; }
      if (Number.isSafeInteger(payload.s) && payload.s >= 0) sequence = payload.s;
      if (payload.op === 10) {
        const requestedInterval = payload.d?.heartbeat_interval;
        if (helloReceived || !Number.isSafeInteger(requestedInterval) || requestedInterval < 1000 || requestedInterval > 120_000) {
          reconnect();
          return;
        }
        helloReceived = true;
        // Keep the bound explicit on the value captured by the timer callback.
        const interval = Math.min(120_000, Math.max(1000, requestedInterval));
        firstHeartbeat = setTimeout(() => {
          heartbeat();
          if (!stopped && !closed) heartbeatTimer = setInterval(heartbeat, interval);
        }, Math.floor(Math.random() * interval));
        send({ op: 2, d: {
          token, intents: 1,
          properties: { os: process.platform, browser: 'brick-presence', device: 'brick-presence' },
          presence: { since: null, activities: [], status: 'online', afk: false },
        } });
      } else if (payload.op === 11) {
        acknowledged = true;
      } else if (payload.op === 1) {
        heartbeat();
      } else if (payload.op === 7 || payload.op === 9) {
        reconnect();
      } else if (payload.op === 0 && payload.t === 'READY') {
        logger.log('Discord Gateway ready');
      }
    });
    socket.addEventListener('close', reconnect);
    socket.addEventListener('error', () => { logger.error('Discord Gateway socket error'); reconnect(); });
  }
  void connect();
  return () => { stopped = true; clearTimeout(reconnectTimer); disconnect(); };
}
