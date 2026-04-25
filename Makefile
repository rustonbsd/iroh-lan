SHELL := /bin/bash

.PHONY: build stress-test clean-test reliability-test clean-reliability

build:
	cargo build
	cargo build --example tcp_client
	cargo build --example tcp_server
	cargo build --example game_check
	cargo build --example mesh_check

long-stress-test: build
	TOPIC="test_topic_$$RANDOM"; \
	GAME_TEST_DURATION=530; \
	echo "Using TOPIC=$$TOPIC"; \
	sudo -E TOPIC=$$TOPIC docker compose -f docker_test/compose-stress.yaml up --build --abort-on-container-exit --remove-orphans

stress-test: build
	TOPIC="test_topic_$$RANDOM"; \
	echo "Using TOPIC=$$TOPIC"; \
	sudo -E WIFI_SIM_DELAY=100 TOPIC=$$TOPIC docker compose -f docker_test/compose-stress.yaml up --build --abort-on-container-exit --remove-orphans; \
	echo ""; \
	./docker_test/check_logs.sh

clean-test:
	sudo docker compose -f docker_test/compose-stress.yaml down -v

reliability-test: build
	mkdir -p docker_test/results_reliability/node0 docker_test/results_reliability/node1 docker_test/results_reliability/node2 docker_test/results_reliability/node3 docker_test/results_reliability/node4; \
	TOPIC="reliability_$$RANDOM"; \
	RUN_ID=$$(date +%Y%m%d_%H%M%S); \
	echo "Using TOPIC=$$TOPIC"; \
	echo "Using RUN_ID=$$RUN_ID"; \
	sudo -E TOPIC=$$TOPIC RUN_ID=$$RUN_ID RECONNECT_MAX_SEC=60 docker compose -f docker_test/compose-reliability.yaml up --build --abort-on-container-exit --remove-orphans; \
	echo ""; \
	chmod +x ./docker_test/check_reliability_logs.sh; \
	./docker_test/check_reliability_logs.sh

clean-reliability:
	sudo docker compose -f docker_test/compose-reliability.yaml down -v
