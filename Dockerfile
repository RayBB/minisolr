# Static musl build in Alpine, then a scratch image holding only the binary.
#   docker build -t minisolr-rs:dev .
#   docker run -v /path/to/openlibrary/conf/solr/conf:/conf:ro -v minisolr-rs-data:/data -p 8983:8983 minisolr-rs:dev
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
ENV CARGO_BUILD_JOBS=2 CARGO_TERM_COLOR=never
WORKDIR /src
# Dependencies first, against placeholder sources, so code changes reuse this layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src src
# Make the real sources newer than the placeholder build's fingerprints.
RUN touch src/lib.rs src/main.rs && cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/minisolr-rs /minisolr-rs
EXPOSE 8983
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --start-interval=1s CMD ["/minisolr-rs", "ping"]
ENTRYPOINT ["/minisolr-rs"]
CMD ["serve", "/conf", "/data", "0.0.0.0:8983"]
