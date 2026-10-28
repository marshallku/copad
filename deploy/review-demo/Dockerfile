# The App Store review demo host for the copad iOS app: sshd with ONE password account whose only
# command is `comux proxy --stdio`, and a comux seeded with a small demo layout. Built from this
# repository so it serves the comux the app was built against:
#
#   docker build -f deploy/review-demo/Dockerfile -t copad-review-demo .
#   deploy/review-demo/run.sh            # see there for the limits it runs with
FROM rust:1.91-trixie AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p copad-mux \
    && cp target/release/comux /comux

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server procps git less vim-tiny ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -s /bin/bash review \
    && mkdir -p /run/sshd /keys
COPY --from=build /comux /usr/local/bin/comux
COPY deploy/review-demo/sshd_config /etc/ssh/sshd_config
COPY deploy/review-demo/comux-only deploy/review-demo/seed.sh deploy/review-demo/entrypoint.sh deploy/review-demo/env.sh /usr/local/lib/review-demo/
COPY deploy/review-demo/home/ /usr/local/lib/review-demo/home/
ENV LANG=C.UTF-8
EXPOSE 22
# The host key lives here so a restart (the nightly reset) keeps the key the reviewer trusted.
VOLUME /keys
ENTRYPOINT ["/usr/local/lib/review-demo/entrypoint.sh"]
