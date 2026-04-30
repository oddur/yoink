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
# Clear the default nginx welcome page before copying our build output,
# otherwise Docker's merge-copy leaves index.html from the base image in
# place (our SPA build has no root index.html — it uses _shell.html).
RUN rm -rf /usr/share/nginx/html/*
COPY --from=builder /src/.output/public /usr/share/nginx/html
# SPA routing + Cloudflare cache headers.
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY nginx-cache.conf /etc/nginx/conf.d/cache.conf
EXPOSE 80
