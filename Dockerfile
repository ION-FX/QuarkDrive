# Multi-stage build: compile the server, then copy the single binary and the
# static UI into a small runtime image. The data directory is a volume, so
# upgrading the container never touches the vaults.
#
#   docker build -t quarkdrive .
#   docker run -p 8787:8787 -v quarkdrive-data:/data quarkdrive
#
# The first run creates no users: either open http://localhost:8787 and use
# the first-run sign-up, or
#   docker compose run --rm quarkdrive create-user --data /data \
#       --username ada --password '…'

FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p quarkdrive-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/quarkdrive-server /usr/local/bin/quarkdrive-server
COPY web /srv/quarkdrive/web

VOLUME /data
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/quarkdrive-server"]
CMD ["serve", "--data", "/data", "--web", "/srv/quarkdrive/web", "--listen", "0.0.0.0:8787"]
