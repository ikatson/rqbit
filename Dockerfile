FROM curlimages/curl:latest AS downloader

RUN ARCH=$(uname -m) \
    && curl -L \
        https://github.com/ikatson/rqbit/releases/latest/download/rqbit-linux-static-$ARCH > /tmp/rqbit \
    && chmod +x /tmp/rqbit

FROM debian:bullseye-slim AS final

ARG UID=1000
ARG GID=1000

RUN groupadd -g "${GID}" appuser \
  && useradd --create-home --no-log-init -u "${UID}" -g "${GID}" appuser

USER appuser

COPY --from=downloader /tmp/rqbit /home/appuser/rqbit

EXPOSE 3030

ENTRYPOINT ["/home/appuser/rqbit"]