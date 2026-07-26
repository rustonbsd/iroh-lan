FROM ubuntu:24.04
RUN apt-get update && apt-get install -y \
    iproute2 \
    ethtool \
    procps \
    netcat-openbsd \
    iputils-ping \
    iperf3 \
    gdb \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY entrypoint_stress.sh /app/entrypoint_stress.sh
COPY entrypoint_reliability.sh /app/entrypoint_reliability.sh
COPY check_reliability_logs.sh /app/check_reliability_logs.sh
RUN chmod +x /app/entrypoint_stress.sh /app/entrypoint_reliability.sh /app/check_reliability_logs.sh
ENTRYPOINT ["/app/entrypoint_stress.sh"]
