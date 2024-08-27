@PHONY: all
all:
	mkdir -p output && \
	docker build --progress=plain --cache-from=type=local,src=target/docker-cache --cache-to=type=local,dest=target/docker-cache --platform linux/arm/v7 --output=type=local,dest=output/ -t testf .
