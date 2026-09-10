FROM node:22-alpine

ENV NODE_ENV=production
WORKDIR /app

RUN mkdir -p /data && chown node:node /data

COPY server.mjs security.mjs profiles.mjs streams.mjs stream_vods.mjs stream_review.mjs player_control.mjs replay_sync.mjs replay_warmup.mjs ./

USER node
EXPOSE 8080

CMD ["node", "server.mjs"]
