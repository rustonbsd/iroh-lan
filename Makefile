SHELL := /bin/bash

.PHONY: build stress-test clean-test

build:
	cargo build
	cargo build --example tcp_client
	cargo build --example tcp_server
	cargo build --example game_check

long-stress-test: build
	TOPIC="test_topic_$$RANDOM"; \
	GAME_TEST_DURATION=530; \
	echo "Using TOPIC=$$TOPIC"; \
	sudo -E TOPIC=$$TOPIC docker compose -f docker_test/compose-stress.yaml up --build --abort-on-container-exit --remove-orphans

stress-test: build
	TOPIC="test_topic_$$RANDOM"; \
	GAME_TEST_DURATION=10; \
	echo "Using TOPIC=$$TOPIC"; \
	sudo -E TOPIC=$$TOPIC docker compose -f docker_test/compose-stress.yaml up --build --abort-on-container-exit --remove-orphans

clean-test:
	sudo docker compose -f docker_test/compose-stress.yaml down -v
