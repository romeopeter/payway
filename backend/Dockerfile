# syntax=docker/dockerfile:1.6

# ---------- builder ----------
FROM rust:1.83-bookworm AS builder
WORKDIR /app

# Step 1: build dependencies in isolation so they cache when only src/ changes.
# We create a stub main.rs, build, then throw the stub away.
COPY backend/Cargo.toml ./backend/
RUN mkdir -p backend/src && \
    echo 'fn main() { println!("payway dep-cache stub"); }' > backend/src/main.rs && \
    cd backend && \
    cargo build --release && \
    rm -rf src target/release/deps/payway_backend* target/release/payway-backend

# Step 2: copy real sources and migrations, then build for real.
# `sqlx::migrate!("../migrations")` reads the SQL at compile time, so the
# migrations dir must exist alongside the crate during build.
COPY backend/src ./backend/src
COPY migrations ./migrations
RUN cd backend && \
    touch src/main.rs && \
    cargo build --release

# ---------- runtime ----------
FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/backend/target/release/payway-backend /usr/local/bin/payway-backend

EXPOSE 8080
CMD ["/usr/local/bin/payway-backend"]
