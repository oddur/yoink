# Multi-stage build: hugo + hextra → static html → nginx:alpine.
# Yoink ships this image to the docs box via unregistry on every deploy.
# syntax=docker/dockerfile:1.7

FROM hugomods/hugo:0.160.1 AS builder
WORKDIR /src
COPY . .
RUN hugo mod tidy
RUN hugo --gc --minify

FROM nginx:alpine
COPY --from=builder /src/public /usr/share/nginx/html
# Tell Cloudflare (and browsers) to cache static responses. Without
# explicit Cache-Control, CF Free serves HTML as `cf-cache-status:
# DYNAMIC` (i.e. uncached), which puts every request on the origin.
# Inserted into the default server block via /etc/nginx/conf.d/.
COPY nginx-cache.conf /etc/nginx/conf.d/cache.conf
EXPOSE 80
