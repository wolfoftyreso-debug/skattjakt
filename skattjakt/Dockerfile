# One Dockerfile, two images, selected by the BINARY build argument.
#
# One file rather than two because the build is identical up to the last COPY:
# the same workspace, the same dependency layer, the same base. Two files would
# be two things to keep in step, and the one that drifts is the one that stops
# being distroless.
#
# The registries are pinned to mirrors that this build environment can actually
# reach. `mirror.gcr.io` is Google's Docker Hub mirror; the content is
# identical, and going through it avoids the rate limit on the Hub itself.


# ---- OCR models -----------------------------------------------------------
#
# 11.5 MB of them, and only the processes that read documents need them. The
# notification worker never sees a scan, so it gets the empty stage and stays
# the size it was. WITH_OCR picks which.
#
# Fetched by checksum inside the build, not committed: the digests are pinned
# in scripts/fetch-ocr-models.sh and a model that does not match fails the
# build rather than shipping.
FROM mirror.gcr.io/library/debian:bookworm-slim AS ocr-fetch
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-ocr-models.sh /tmp/fetch-ocr-models.sh
RUN /tmp/fetch-ocr-models.sh /models

# Scratch, holding the two files and nothing else, so the copy into the
# runtime image cannot drag a Debian filesystem along with it.
FROM scratch AS ocr-1
COPY --from=ocr-fetch /models/ /

FROM scratch AS ocr-0

ARG WITH_OCR=0
FROM ocr-${WITH_OCR} AS ocr

# ---- build ----------------------------------------------------------------
FROM mirror.gcr.io/library/rust:1.94-slim-bookworm AS build
WORKDIR /src

# Optional: an extra CA to trust while fetching crates.
#
# Build environments behind an inspecting proxy present their own certificate,
# and cargo then cannot reach crates.io at all. Passing the bundle in as a
# build context file keeps that concern out of the Dockerfile's logic and out
# of the final image — this stage is discarded, and the runtime stage below
# starts from distroless with only the binary copied across.
#
#   docker build --secret id=ca,src=/path/to/ca-bundle.crt ...
#
# Absent the secret, the mount is empty and the build uses the system trust
# store, which is the normal case.
RUN --mount=type=secret,id=ca,required=false \
    if [ -s /run/secrets/ca ]; then \
        cp /run/secrets/ca /usr/local/share/ca-certificates/build-proxy.crt \
        && update-ca-certificates; \
    fi

# The dependency layer. Manifests first, then a build against empty sources, so
# a change to one line of Rust does not re-download and re-compile 300 crates.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml       crates/core/Cargo.toml
COPY crates/telemetry/Cargo.toml  crates/telemetry/Cargo.toml
COPY crates/rules/Cargo.toml      crates/rules/Cargo.toml
COPY crates/model/Cargo.toml      crates/model/Cargo.toml
COPY crates/gateway/Cargo.toml    crates/gateway/Cargo.toml
COPY crates/extract/Cargo.toml    crates/extract/Cargo.toml
COPY crates/pipeline/Cargo.toml   crates/pipeline/Cargo.toml
COPY crates/store/Cargo.toml      crates/store/Cargo.toml
COPY crates/jobs/Cargo.toml       crates/jobs/Cargo.toml
COPY apps/api/Cargo.toml          apps/api/Cargo.toml
COPY workers/analysis-worker/Cargo.toml workers/analysis-worker/Cargo.toml

RUN mkdir -p crates/core/src crates/telemetry/src crates/rules/src crates/model/src \
             crates/gateway/src crates/extract/src crates/pipeline/src crates/store/src \
             crates/jobs/src apps/api/src workers/analysis-worker/src \
 && for c in core telemetry rules model gateway extract pipeline store jobs; do \
        : > crates/$c/src/lib.rs; \
    done \
 && : > apps/api/src/lib.rs \
 && echo "fn main() {}" > apps/api/src/main.rs \
 && echo "fn main() {}" > workers/analysis-worker/src/main.rs \
 && cargo build --release 2>/dev/null || true

COPY . .
# Touch every source file so cargo rebuilds them over the placeholder
# artefacts. Without this, cargo compares timestamps, finds the empty lib.rs
# newer, and ships an image containing nothing.
ARG BINARY=skattjakt-api
RUN find crates apps workers -name '*.rs' -exec touch {} + \
 && cargo build --release --locked --bin "$BINARY" \
 && cp "target/release/$BINARY" /src/entrypoint

# ---- runtime --------------------------------------------------------------
# Distroless: no shell, no package manager, no busybox. An attacker who reaches
# code execution in this container finds nothing to pivot with — which also
# means no `kubectl exec` debugging, and that trade is made deliberately. Use
# an ephemeral debug container instead; see SKATTJAKT_RUNBOOK.md.
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=build /src/entrypoint /app/skattjakt
# Empty when WITH_OCR=0, so this line costs nothing in those images.
COPY --from=ocr / /app/models/

# 65532 is distroless's `nonroot`. Stated numerically as well as by name,
# because `runAsNonRoot` in the pod spec is checked against the numeric UID and
# a name it cannot resolve fails the pod at admission.
USER 65532:65532
EXPOSE 8080
ENV PORT=8080 \
    RUST_LOG=skattjakt=info,sqlx=warn \
    SKATTJAKT_OCR_MODELS=/app/models

ENTRYPOINT ["/app/skattjakt"]
