FROM rust:1.85-bookworm AS builder
WORKDIR /app

ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    wget \
    unzip \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    arch="${TARGETARCH:-}"; \
    if [ -z "$arch" ]; then \
    case "$(dpkg --print-architecture)" in \
    amd64) arch="amd64" ;; \
    arm64) arch="arm64" ;; \
    armhf) arch="arm" ;; \
    *) echo "Unsupported dpkg architecture: $(dpkg --print-architecture)"; exit 1 ;; \
    esac; \
    fi; \
    case "$arch" in \
    amd64) vosk_pkg="vosk-linux-x86_64-0.3.45" ;; \
    arm64) vosk_pkg="vosk-linux-aarch64-0.3.45" ;; \
    arm) vosk_pkg="vosk-linux-armv7l-0.3.45" ;; \
    *) echo "Unsupported TARGETARCH: $arch"; exit 1 ;; \
    esac; \
    wget -q -O /tmp/vosk.zip "https://github.com/alphacep/vosk-api/releases/download/v0.3.45/${vosk_pkg}.zip" && \
    unzip -q /tmp/vosk.zip -d /tmp && \
    cp "/tmp/${vosk_pkg}/libvosk.so" /usr/local/lib/libvosk.so && \
    rm -rf /tmp/vosk.zip "/tmp/${vosk_pkg}"

COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libasound2 \
    alsa-utils \
    espeak-ng \
    libstdc++6 \
    wget \
    unzip \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/wakeword-listener /usr/local/bin/wakeword-listener
COPY --from=builder /usr/local/lib/libvosk.so /usr/local/lib/libvosk.so

# Default model path in app: /models/vosk-model-small-de-0.15
RUN mkdir -p /models && \
    wget -q -O /tmp/model.zip https://alphacephei.com/vosk/models/vosk-model-small-de-0.15.zip && \
    unzip -q /tmp/model.zip -d /models && \
    rm /tmp/model.zip

ENV WAKE_WORD=marvin
ENV VOSK_MODEL_PATH=/models/vosk-model-small-de-0.15
ENV LD_LIBRARY_PATH=/usr/local/lib

CMD ["wakeword-listener"]
