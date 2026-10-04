# Hub image. Not built on the development machine (no Docker there), so verify before relying on it.
FROM node:22-slim AS build
WORKDIR /app
RUN corepack enable
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml tsconfig.base.json tsconfig.json ./
COPY packages ./packages
RUN pnpm install --frozen-lockfile && pnpm --filter @claudecord/protocol --filter @claudecord/hub build

FROM node:22-slim
WORKDIR /app
COPY --from=build /app /app
ENV PORT=8787 DB_PATH=/data/hub.db
VOLUME /data
EXPOSE 8787
CMD ["node", "packages/hub/dist/index.js"]
