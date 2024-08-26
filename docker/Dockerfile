FROM scratch

## Assumes you are running this from respective context folder.
COPY --chmod=755 rqbit /bin/rqbit

WORKDIR /home/rqbit

ENV XDG_DATA_HOME=/home/rqbit/config
ENV XDG_CACHE_HOME=/home/rqbit/cache

ENV RQBIT_HTTP_API_LISTEN_ADDR=0.0.0.0:3000
ENV RQBIT_TCP_LISTEN_MIN_PORT=4240
ENV RQBIT_TCP_LISTEN_MAX_PORT=4241

VOLUME /home/rqbit/config
VOLUME /home/rqbit/cache
VOLUME /home/rqbit/output

EXPOSE 3000
EXPOSE 4240
ENTRYPOINT ["/bin/rqbit"]
