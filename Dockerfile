# Shopping Sessions: native Rust engine, one static binary in a scratch image.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY engine/ /src/
RUN set -eux; \
    if [ "$(uname -m)" = "x86_64" ]; then CPU="-C target-cpu=x86-64-v3"; else CPU=""; fi; \
    RUSTFLAGS="-C target-feature=+crt-static $CPU" cargo build --release --target "$(uname -m)-unknown-linux-gnu"; \
    cp target/*/release/engine /engine; /engine --selftest-noop 2>/dev/null || true

FROM scratch
COPY --from=build /engine /engine
USER 1000:1000
CMD ["/engine"]
