FROM node:26-alpine@sha256:ef24c5053d50fdc3e4e56eb4e7ddb7861874ab0fdc797046ba897581deb8e868

ENV NODE_ENV=production
WORKDIR /app

# Runtime has no npm dependencies. Remove package managers and their unused
# dependency trees, and apply the distribution's signed security updates.
RUN apk upgrade --no-cache \
    && rm -rf /usr/local/lib/node_modules /opt/yarn-v* \
    && rm -f /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack /usr/local/bin/yarn /usr/local/bin/yarnpkg \
    && mkdir -p /data && chown node:node /data

COPY server.mjs timestamp_queue.mjs security.mjs profiles.mjs cooldown_catalog.mjs cooldown-catalog.json streams.mjs stream_vods.mjs stream_review.mjs player_control.mjs replay_sync.mjs replay_warmup.mjs ./

USER node
EXPOSE 8080

CMD ["node", "server.mjs"]
