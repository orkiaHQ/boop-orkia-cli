FROM rust:1.93-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p orkia-server

FROM debian:bookworm-slim
# libgit2/reqwest need the platform trust store when the self-hosted server
# talks to a Git forge over HTTPS.  The image otherwise stays non-root.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home orkia
COPY --from=build /src/target/release/orkia-server /usr/local/bin/orkia-server
USER orkia
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/orkia-server"]
