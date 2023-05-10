# install dependencies
FROM rust:slim-bullseye AS compiler
RUN apt update \
    && apt install -y libclang-dev clang \
        build-essential tcl protobuf-compiler file \
        libssl-dev pkg-config \
    && apt clean

RUN cargo install cargo-chef
WORKDIR /sqld

# prepare recipe
FROM compiler AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# build sqld
FROM compiler AS builder
COPY --from=planner sqld/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build -p sqld --release

# runtime
FROM debian:bullseye-slim
COPY --from=builder /sqld/target/release/sqld /bin/sqld
RUN adduser --system --home /var/lib/sqld --uid 666 sqld 
RUN apt-get update && apt-get install -y ca-certificates
COPY docker-entrypoint.sh /usr/local/bin
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
VOLUME [ "/var/lib/sqld" ]
WORKDIR /var/lib/sqld
USER sqld
EXPOSE 5001 5432 8080
CMD ["/bin/sqld"]
