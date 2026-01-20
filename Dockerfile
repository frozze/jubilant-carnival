# ==========================================
# Stage 1: Build
# ==========================================
FROM rust:1.83-bookworm AS builder

WORKDIR /app

# Install dependencies for native-tls
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock* ./

# Create dummy src to cache dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy actual source code
COPY src ./src

# Build for real (touch main.rs to force rebuild)
RUN touch src/main.rs && cargo build --release

# ==========================================
# Stage 2: Runtime (minimal image)
# ==========================================
FROM debian:bookworm-slim AS runtime

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /app/target/release/bybit-scalper-bot /app/bybit-scalper-bot

# Create non-root user
RUN useradd -r -s /bin/false botuser && \
    chown -R botuser:botuser /app
USER botuser

# Environment defaults
ENV RUST_LOG=info

ENTRYPOINT ["/app/bybit-scalper-bot"]
