# Hub image. Not built on the development machine (no Docker there), so verify before relying on it.
#
# The hub reads its configuration from a private file, never from environment variables. Create it once with
# `claudecord-hub setup` on any machine, then mount it into the container (for example as a Docker or Fly secret
# file) and point --config at it. The file must be mode 0600.
FROM node:22-slim AS build
WORKDIR /app
RUN corepack enable
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml tsconfig.base.json tsconfig.json ./
COPY packages ./packages
RUN pnpm install --frozen-lockfile && pnpm --filter @claudecord/protocol --filter @claudecord/hub build

FROM node:22-slim
WORKDIR /app
COPY --from=build /app /app
USER node
EXPOSE 8787
CMD ["node", "packages/hub/dist/index.js", "run", "--config", "/run/secrets/hub.json"]
