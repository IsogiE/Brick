FROM node:22-alpine

ENV NODE_ENV=production
WORKDIR /app

RUN mkdir -p /data && chown node:node /data

COPY server.mjs security.mjs ./

USER node
EXPOSE 8080

CMD ["node", "server.mjs"]
