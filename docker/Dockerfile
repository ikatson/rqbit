FROM scratch

ARG TARGETPLATFORM

ADD https://curl.se/ca/cacert.pem /etc/ssl/cacerts.pem

## Assumes you are running this from respective context folder.
COPY --chmod=755 ${TARGETPLATFORM}/rqbit /bin/rqbit

WORKDIR /home/rqbit

ENV XDG_DATA_HOME=/home/rqbit/db
ENV XDG_CACHE_HOME=/home/rqbit/cache
ENV SSL_CERT_FILE=/etc/ssl/cacerts.pem

ENV RQBIT_HTTP_API_LISTEN_ADDR=0.0.0.0:3030
ENV RQBIT_TCP_LISTEN_MIN_PORT=4240
ENV RQBIT_TCP_LISTEN_MAX_PORT=4260

VOLUME /home/rqbit/db
VOLUME /home/rqbit/cache
VOLUME /home/rqbit/downloads

EXPOSE 3030
EXPOSE 4240
CMD ["server", "start", "/home/rqbit/downloads"]
ENTRYPOINT ["/bin/rqbit"]
