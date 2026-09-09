FROM node:22-bookworm-slim

ARG ARENA0_VERSION=latest
ARG CODEX_VERSION=0.153.4

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        locales \
        procps \
        python3 \
        tmux \
    && rm -rf /var/lib/apt/lists/*

# Install from the public registries. The repository is copied only after npm
# installation so this image cannot accidentally exercise workspace packages.
RUN npm install --global \
        "@0xff-ai/arena0@${ARENA0_VERSION}" \
        "@openai/codex@${CODEX_VERSION}"

COPY scripts/run-npm-agent-demo.sh /usr/local/bin/run-npm-agent-demo
RUN chmod 0755 /usr/local/bin/run-npm-agent-demo

ENV LANG=C.UTF-8
ENV TERM=xterm-256color
WORKDIR /work

ENTRYPOINT ["/usr/local/bin/run-npm-agent-demo"]
