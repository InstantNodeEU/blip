FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY . .
RUN cargo build --release

FROM scratch
COPY --from=build /src/target/release/blip /blip
USER 65534:65534
WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/blip"]
CMD ["/data/blip.conf"]
