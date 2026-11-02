# The commons server (docs/COMMONS.md §20). Railway builds this file; the
# service mounts a volume at /data for the database and the commons key.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p speedrun-commons

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/speedrun-commons /usr/local/bin/speedrun-commons
ENV SPEEDRUN_COMMONS_HOME=/data
EXPOSE 8080
CMD ["speedrun-commons"]
