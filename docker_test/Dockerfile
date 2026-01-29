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
COPY entrypoint_stress.sh /app/entrypoint_stress.sh
RUN chmod +x /app/entrypoint_stress.sh
ENTRYPOINT ["/app/entrypoint_stress.sh"]
