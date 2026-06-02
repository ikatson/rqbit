@echo off
setlocal

set "RQBIT_UPNP_SERVER_ENABLE=true"
set "RQBIT_UPNP_SERVER_FRIENDLY_NAME=rqbit-dev"
set "RQBIT_HTTP_API_LISTEN_ADDR=[::]:3030"
set "RQBIT_ENABLE_PROMETHEUS_EXPORTER=true"
set "RQBIT_EXPERIMENTAL_UTP_LISTEN_ENABLE=true"
set "RQBIT_HTTP_API_ALLOW_CREATE=true"
set "RQBIT_FASTRESUME=true"

set "RQBIT_OUTPUT_FOLDER=%TEMP%\scratch"
set "RQBIT_LOG_FILE=%TEMP%\rqbit-log"
set "RQBIT_LOG_FILE_RUST_LOG=debug,librqbit=trace,upnp_serve=trace,librqbit_utp=debug"
set "CORS_ALLOW_REGEXP=.*"

type nul > "%RQBIT_LOG_FILE%"
cargo run -- server start "%RQBIT_OUTPUT_FOLDER%"
