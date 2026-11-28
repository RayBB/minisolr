# Static musl build in Alpine, then a scratch image holding only the binary.
#   docker build -t minisolr:dev .
#   docker run -v /path/to/openlibrary/conf/solr/conf:/conf:ro -v minisolr-data:/data -p 8983:8983 minisolr:dev
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
LABEL org.opencontainers.image.source="https://github.com/RayBB/minisolr" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.description="Solr-compatible search server for local Open Library development (Rust, Tantivy, scratch image)"
COPY --from=build /src/target/release/minisolr /minisolr
EXPOSE 8983
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --start-interval=1s CMD ["/minisolr", "ping"]
ENTRYPOINT ["/minisolr"]
CMD ["serve", "/conf", "/data", "0.0.0.0:8983"]
