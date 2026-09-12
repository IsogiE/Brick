FROM node:22-alpine@sha256:c610fcdfb1d5b4740dd70c284ed3cb16bb857e0f7166196e36a5501df7a3aa32

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
