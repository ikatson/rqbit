# Depends on either outputs of Dockerfile.xx or the binaries compiled elsewhere.
FROM scratch

# Create /etc/ssl first: ADD --chmod also applies its mode to the parent
# directories it creates, and 0644 directories can't be entered by non-root.
WORKDIR /etc/ssl
ADD --chmod=0644 https://curl.se/ca/cacert.pem /etc/ssl/cacerts.pem

ARG TARGETPLATFORM

## Assumes you are running this from respective context folder.
COPY --chmod=755 ${TARGETPLATFORM}/rqbit /bin/rqbit

WORKDIR /home/rqbit

ENV HOME=/home/rqbit
ENV XDG_DATA_HOME=/home/rqbit/db
ENV XDG_CACHE_HOME=/home/rqbit/cache
ENV SSL_CERT_FILE=/etc/ssl/cacerts.pem

ENV RQBIT_HTTP_API_LISTEN_ADDR=0.0.0.0:3030
ENV RQBIT_LISTEN_PORT=4240

VOLUME /home/rqbit/db
VOLUME /home/rqbit/cache
VOLUME /home/rqbit/downloads

EXPOSE 3030
EXPOSE 4240
CMD ["server", "start", "/home/rqbit/downloads"]
ENTRYPOINT ["/bin/rqbit"]
