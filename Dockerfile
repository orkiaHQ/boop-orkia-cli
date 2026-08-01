FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p orkia-server

FROM debian:bookworm-slim
RUN useradd --system --create-home orkia
COPY --from=build /src/target/release/orkia-server /usr/local/bin/orkia-server
USER orkia
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/orkia-server"]
