all:

@PHONY: webui-deps
webui-deps:
	cd desktop && npm install
	cd crates/librqbit/webui && npm install

@PHONY: webui-dev
webui-dev: webui-deps
	cd crates/librqbit/webui && \
	npm run dev

# NOTE: on LG TV using hostname is unstable for some reason, use IP address.
export RQBIT_UPNP_SERVER_ENABLE ?= true
export RQBIT_UPNP_SERVER_FRIENDLY_NAME ?= rqbit-dev
export RQBIT_HTTP_API_LISTEN_ADDR ?= [::]:3030
export RQBIT_ENABLE_PROMETHEUS_EXPORTER ?= true
export RQBIT_EXPERIMENTAL_UTP_LISTEN_ENABLE ?= true
export RQBIT_FASTRESUME = true

CARGO_RUN_FLAGS ?=
RQBIT_OUTPUT_FOLDER ?= /tmp/scratch
RQBIT_POSTGRES_CONNECTION_STRING ?= postgres:///rqbit

@PHONY: devserver-profile
devserver-profile:
	cargo run --release $(CARGO_RUN_FLAGS) -- server start $(RQBIT_OUTPUT_FOLDER)

# DEV variables (that's why defined after devserver-profile)
export RQBIT_LOG_FILE ?= /tmp/rqbit-log
export RQBIT_LOG_FILE_RUST_LOG ?= debug,librqbit=trace,upnp_serve=trace,librqbit_utp=debug
export CORS_ALLOW_REGEXP ?= '.*'

@PHONY: devserver
devserver:
	echo -n '' > $(RQBIT_LOG_FILE) && \
	cargo run $(CARGO_RUN_FLAGS) -- \
	server start $(RQBIT_OUTPUT_FOLDER)

@PHONY: devserver
devserver-postgres:
	echo -n '' > $(RQBIT_LOG_FILE) && \
	cargo run $(CARGO_RUN_FLAGS) -- \
	server start --fastresume --persistence-location $(RQBIT_POSTGRES_CONNECTION_STRING) $(RQBIT_OUTPUT_FOLDER)

@PHONY: docker-build-xx-one-platform
docker-build-xx-one-platform:
	docker build -f docker/Dockerfile.xx \
		--platform $(PLATFORM) \
		--output type=local,dest=target/cross/$(PLATFORM) . && \
	docker build \
		-t ikatson/rqbit:$(shell git describe --tags)-dev-$(shell echo $(PLATFORM) | tr '/' '-') \
		--platform $(PLATFORM) \
		-f docker/Dockerfile \
		target/cross/

@PHONY: docker-build-amd64
docker-build-amd64:
	PLATFORM=linux/amd64 $(MAKE) docker-build-xx-one-platform

@PHONY: docker-build-armv7
docker-build-armv7:
		PLATFORM=linux/arm/v7 $(MAKE) docker-build-xx-one-platform

@PHONY: clean
clean:
	rm -rf target

CARGO_RELEASE_PROFILE ?= release-github

@PHONY: release-linux-current-target
release-linux-current-target:
	CC_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-gcc \
	CXX_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-g++ \
	AR_$(TARGET_SNAKE_CASE)=$(CROSS_COMPILE_PREFIX)-ar \
	CARGO_TARGET_$(TARGET_SNAKE_UPPER_CASE)_LINKER=$(CROSS_COMPILE_PREFIX)-gcc \
	cargo build  --profile $(CARGO_RELEASE_PROFILE) --target=$(TARGET) --features=openssl-vendored

@PHONY: debug-linux-docker-x86_64
debug-linux-docker-x86_64:
	CARGO_RELEASE_PROFILE=dev \
	$(MAKE) release-linux-x86_64 && \
	cp target/x86_64-unknown-linux-musl/debug/rqbit target/cross/linux/amd64/ && \
	docker build -t ikatson/rqbit:tmp-debug -f docker/Dockerfile --platform linux/amd64 target/cross && \
	docker push ikatson/rqbit:tmp-debug

@PHONY: release-linux-x86_64
release-linux-x86_64:
	TARGET=x86_64-unknown-linux-musl \
	TARGET_SNAKE_CASE=x86_64_unknown_linux_musl \
	TARGET_SNAKE_UPPER_CASE=X86_64_UNKNOWN_LINUX_MUSL \
	CROSS_COMPILE_PREFIX=x86_64-unknown-linux-musl \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-aarch64
release-linux-aarch64:
	TARGET=aarch64-unknown-linux-musl \
	TARGET_SNAKE_CASE=aarch64_unknown_linux_musl \
	TARGET_SNAKE_UPPER_CASE=AARCH64_UNKNOWN_LINUX_MUSL \
	CROSS_COMPILE_PREFIX=aarch64-unknown-linux-musl \
	$(MAKE) release-linux-current-target

@PHONY: release-linux-armv7
release-linux-armv7:
	TARGET=armv7-unknown-linux-musleabihf \
	TARGET_SNAKE_CASE=armv7_unknown_linux_musleabihf \
	TARGET_SNAKE_UPPER_CASE=ARMV7_UNKNOWN_LINUX_MUSLEABIHF \
	CROSS_COMPILE_PREFIX=armv7-linux-musleabihf \
	$(MAKE) release-linux-current-target
