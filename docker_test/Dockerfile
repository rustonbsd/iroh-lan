FROM ubuntu:24.04
RUN apt-get update && apt-get install -y \
    iproute2 \
    ethtool \
    procps \
    netcat-openbsd \
    iputils-ping \
    iperf3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh
ENTRYPOINT ["/app/entrypoint.sh"]
