FROM scratch

USER 501:502

## Assumes you run from one of the folders with the binary already built.
COPY --chown=501:502 rqbit /bin/rqbit

WORKDIR /home/rqbit

ENV XDG_DATA_HOME=/home/rqbit/config
ENV XDG_CACHE_HOME=/home/rqbit/cache

VOLUME /home/rqbit/config
VOLUME /home/rqbit/cache
VOLUME /home/rqbit/output

EXPOSE 3000
ENTRYPOINT ["/bin/rqbit"]
