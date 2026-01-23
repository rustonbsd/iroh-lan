.PHONY: test build clean-test

build:
	cargo build
	cargo build --example tcp_client
	cargo build --example tcp_server
	cargo build --example game_check

test: build
	sudo docker compose -f docker_test/compose.yaml up --build --abort-on-container-exit

stress-test: build
	sudo docker compose -f docker_test/compose.yaml -f docker_test/compose-stress.yaml up --build --abort-on-container-exit
test-fast:
	sudo docker compose -f docker_test/compose.yaml up --abort-on-container-exit

clean-test:
	sudo docker compose -f docker_test/compose.yaml down -v
