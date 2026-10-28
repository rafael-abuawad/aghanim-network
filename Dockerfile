# aghanim-relay: WebRTC signaling, persistent sync peer and WebSocket fallback.
#   docker build -t aghanim-relay .
#   docker run -p 8787:8787 -v aghanim-data:/data aghanim-relay
FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p aghanim-relay --locked

FROM debian:stable-slim
RUN useradd --system --uid 10001 aghanim && mkdir /data && chown aghanim /data
COPY --from=build /src/target/release/aghanim-relay /usr/local/bin/aghanim-relay
USER aghanim
VOLUME /data
EXPOSE 8787
ENV AGHANIM_LISTEN=0.0.0.0:8787 AGHANIM_DATA_DIR=/data RUST_LOG=info
HEALTHCHECK CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8787"]
ENTRYPOINT ["aghanim-relay"]
