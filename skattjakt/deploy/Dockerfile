# ---- build ----------------------------------------------------------------
FROM rust:1.83-slim-bookworm AS build
WORKDIR /src

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config \
 && rm -rf /var/lib/apt/lists/*

# Dependencies first, so a source-only change does not rebuild the world.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml       crates/core/Cargo.toml
COPY crates/rules/Cargo.toml      crates/rules/Cargo.toml
COPY crates/model/Cargo.toml      crates/model/Cargo.toml
COPY crates/extract/Cargo.toml    crates/extract/Cargo.toml
COPY crates/pipeline/Cargo.toml   crates/pipeline/Cargo.toml
COPY crates/api/Cargo.toml        crates/api/Cargo.toml
RUN mkdir -p crates/{core,rules,model,extract,pipeline,api}/src \
 && echo "" > crates/core/src/lib.rs \
 && echo "" > crates/rules/src/lib.rs \
 && echo "" > crates/model/src/lib.rs \
 && echo "" > crates/extract/src/lib.rs \
 && echo "" > crates/pipeline/src/lib.rs \
 && echo "" > crates/api/src/lib.rs \
 && echo "fn main() {}" > crates/api/src/main.rs \
 && cargo build --release --bin skattjakt-api 2>/dev/null || true

COPY . .
# Touch the entry points so cargo rebuilds them over the placeholder artefacts.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --bin skattjakt-api

# ---- runtime --------------------------------------------------------------
# Distroless: no shell, no package manager, nothing for an attacker to pivot to.
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=build /src/target/release/skattjakt-api /app/skattjakt-api

USER nonroot:nonroot
EXPOSE 8080
ENV PORT=8080 RUST_LOG=skattjakt=info

ENTRYPOINT ["/app/skattjakt-api"]
