# Brick Presence

Small HTTPS API for Brick's roster tab.

It does a few jobs:

- receives the Discord browser callback and hands the one-time code to Brick
- verifies Brick users with their Discord OAuth access token
- keeps short-lived client heartbeats so the roster can show online/offline state
- keeps the Discord bot connected to the Gateway so the bot appears online

Verified Discord access tokens are cached briefly server-side so roster refreshes
and heartbeats do not hammer Discord.

The Discord bot token lives only on the VPS. Do not put it in the desktop app.

## DNS

Create this record before starting the Caddy container:

```text
Type: A
Name: brick
Content: 2.28.118.132
Proxy: DNS only
TTL: Auto
```

That points `brick.lusaggo.com` to the VPS.

## Required Discord Setting

In the Discord Developer Portal for the bot application, enable the privileged
`Server Members Intent`. The roster endpoint needs it to list guild members.

On the OAuth2 page, add this redirect:

```text
https://brick.lusaggo.com/discord/callback
```

## Deploy

Copy `.env.example` to `.env`, set `DISCORD_BOT_TOKEN`, then run:

```sh
docker compose up -d --build
```

Health check:

```sh
curl https://brick.lusaggo.com/health
```
