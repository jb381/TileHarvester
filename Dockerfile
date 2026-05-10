# syntax=docker/dockerfile:1

# Build stage
FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim AS builder

WORKDIR /app

# Enable bytecode compilation for faster startup
ENV UV_COMPILE_BYTECODE=1
ENV UV_LINK_MODE=copy

# Install dependencies without the project itself
RUN --mount=type=cache,target=/root/.cache/uv \
    --mount=type=bind,source=uv.lock,target=uv.lock \
    --mount=type=bind,source=pyproject.toml,target=pyproject.toml \
    uv sync --frozen --no-install-project --no-dev

# Copy project and install
COPY . /app
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --frozen --no-dev

# Production stage - minimal runtime image
FROM python:3.12-slim-bookworm

WORKDIR /app

# Copy the virtual environment and source code from builder
COPY --from=builder /app/.venv /app/.venv
COPY --from=builder /app/tileharvester /app/tileharvester

# Make CLI available
ENV PATH="/app/.venv/bin:$PATH"
ENV PYTHONPATH="/app"

# Create data directory for SQLite DB and tokens
RUN mkdir -p /data
ENV TH_DATA_DIR=/data

# Default to showing help, override in compose or CLI
ENTRYPOINT ["tileharvester"]
CMD ["--help"]
