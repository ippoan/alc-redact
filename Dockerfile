# alc-redact の image。glibc の debian + pdfium (pdfium は dlopen するので musl + scratch では動かない)。
# pdfium の版は rust-alc-api の Dockerfile と同じ (bblanchon/pdfium-binaries の chromium/7825)。

FROM rust:1-slim-trixie AS build
WORKDIR /src
# 依存だけ先に build して layer に残す
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:trixie-slim AS pdfium
ARG PDFIUM_VERSION=chromium/7825
ARG PDFIUM_SHA256=ae0e276bcdf276dca2746adb4780f79949620e5c655973ca252a3994bc516a13
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /tmp/pdfium.tgz "https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_VERSION}/pdfium-linux-x64.tgz" \
    && echo "${PDFIUM_SHA256}  /tmp/pdfium.tgz" | sha256sum -c - \
    && mkdir /tmp/pdfium && tar -xzf /tmp/pdfium.tgz -C /tmp/pdfium

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=pdfium /tmp/pdfium/lib/libpdfium.so /usr/lib/libpdfium.so
RUN ldconfig
COPY --from=build /src/target/release/alc-redact /usr/local/bin/alc-redact

USER 65532:65532
ENV PORT=8080
EXPOSE 8080
CMD ["alc-redact"]
