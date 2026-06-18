FROM golang:1.21.3-bookworm AS build

WORKDIR /src

COPY ./main.go /src/main.go

RUN set -xe; \
    CGO_ENABLED=1 \
    GOARCH=amd64 \
    go build \
      -ldflags='-linkmode external -extldflags "-static-pie" -s -w' \
      -o /server main.go \
    ;

FROM scratch AS rootfs

COPY --from=build /server /bin/server

# --- CPIO rootfs builder (used by: docker build --target cpio) ---
FROM alpine:3.20 AS cpio
RUN apk add --no-cache cpio findutils
COPY --from=rootfs / /rootfs/
RUN cd /rootfs && find . | cpio -o -H newc > /output.cpio 2>/dev/null
