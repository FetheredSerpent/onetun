FROM rust:1.55 as cargo-build
RUN apt-get update

WORKDIR /usr/src/onetun
COPY Cargo.toml Cargo.toml

# Placeholder to download dependencies and cache them using layering
RUN mkdir src/
RUN echo "fn main() {println!(\"if you see this, the build broke\")}" > src/main.rs
RUN cargo build --release
RUN rm -f target/x86_64-unknown-linux-musl/release/deps/myapp*

# Build the actual project
COPY . .
RUN cargo build --release

FROM debian:11-slim

COPY --from=cargo-build /usr/src/onetun/target/release/onetun /usr/local/bin/onetun

# Run as non-root
RUN chown 1000 /usr/local/bin/onetun
USER 1000

ENTRYPOINT ["/usr/local/bin/onetun"]
