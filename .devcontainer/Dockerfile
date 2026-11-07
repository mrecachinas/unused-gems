FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# libclang: ruby-prism-sys generates its bindings with bindgen at build time.
# Ruby + Bundler: run unused-gems against real projects with installed gems.
RUN apt-get update \
    && export DEBIAN_FRONTEND=noninteractive \
    && apt-get install -y --no-install-recommends \
        clang \
        libclang-dev \
        ruby-full \
        ruby-bundler \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
