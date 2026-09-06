# Presence service

Node.js 22 service for Brick's guild roster, online status, and Discord OAuth callback handoff. It has no npm dependencies.

| Route | Purpose |
| --- | --- |
| `GET /health` | Health check |
| `GET /discord/callback` | Browser redirect after Discord authorization |
| `GET /v1/auth/callback` | Retrieve the pending authorization code |
| `POST /v1/heartbeat` | Update the signed-in user's presence |
| `GET /v1/roster` | Read the guild roster and online status |

For a local instance, copy `.env.example` to `.env` and fill in your own Discord application and guild configuration. Roster access needs the bot's Server Members Intent enabled. Keep the bot token on the server.

```sh
node --env-file=.env server.mjs
```

The service listens on port 8080 by default. `compose.yaml` provides an alternative deployment with Caddy for HTTPS.
