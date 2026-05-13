# Networking Python example on Hyperlight
#
# Extends the Python base runtime with networking test scripts
# and DNS configuration.

ARG BASE=ghcr.io/hyperlight-dev/hyperlight-unikraft/python-base:latest
FROM ${BASE} AS rootfs
COPY http_get.py /http_get.py
COPY echo_server.py /echo_server.py

# --- CPIO rootfs builder ---
FROM alpine:3.20 AS cpio
RUN apk add --no-cache cpio findutils
COPY --from=rootfs / /rootfs/

# DNS configuration for glibc's getaddrinfo (added in the cpio stage
# because the python-base image has no shell)
RUN mkdir -p /rootfs/etc && \
    echo "nameserver 8.8.8.8" > /rootfs/etc/resolv.conf && \
    echo "nameserver 8.8.4.4" >> /rootfs/etc/resolv.conf && \
    echo "hosts: files dns" > /rootfs/etc/nsswitch.conf && \
    echo "127.0.0.1 localhost" > /rootfs/etc/hosts

RUN cd /rootfs && find . | cpio -o -H newc > /output.cpio 2>/dev/null
