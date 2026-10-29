# Sandbox the agent runs file-analysis commands in (`run_command`). One
# long-lived container per session; the session's workspace is bind-mounted at
# /work. Build: docker build -t scrape-sandbox:0.1 sandbox
#
# The backend runs the container as the uid that owns the workspace directory
# (so files the agent writes are readable/writable by the backend on Linux too),
# which has no home in /etc/passwd — HOME points at /tmp so `pip install --user`
# and `bun add` still work for the life of the container.
FROM oven/bun:1.3.14-slim AS bun

FROM python:3.13.13-slim-bookworm
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
RUN ln -s /usr/local/bin/bun /usr/local/bin/bunx \
 && apt-get update \
 && apt-get install -y --no-install-recommends jq ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && pip install --no-cache-dir pandas==3.1.0 duckdb==2.0.0
ENV HOME=/tmp \
    PATH="/tmp/.local/bin:/tmp/.bun/bin:${PATH}" \
    PYTHONDONTWRITEBYTECODE=1
WORKDIR /work
CMD ["sleep", "infinity"]
