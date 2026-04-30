# Multi-stage build: Node + pnpm + vite → static html → nginx:alpine.
# Yoink ships this image to the docs box via unregistry on every deploy.
# syntax=docker/dockerfile:1.7

FROM node:22-alpine AS builder
WORKDIR /src
RUN corepack enable && corepack prepare pnpm@10.28.0 --activate
COPY package.json pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY . .
RUN pnpm build

FROM nginx:alpine
COPY --from=builder /src/.output/public /usr/share/nginx/html
# Tell Cloudflare (and browsers) to cache static responses. Without
# explicit Cache-Control, CF Free serves HTML as `cf-cache-status:
# DYNAMIC` (i.e. uncached), which puts every request on the origin.
# Inserted into the default server block via /etc/nginx/conf.d/.
COPY nginx-cache.conf /etc/nginx/conf.d/cache.conf
EXPOSE 80
