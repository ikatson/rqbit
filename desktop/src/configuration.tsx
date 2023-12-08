type PathLike = string;
type Duration = string;
type SocketAddr = string;

interface RqbitDesktopConfigDht {
  disable: boolean;
  disable_persistence: boolean;
  persistence_filename: PathLike;
}

interface RqbitDesktopConfigTcpListen {
  disable: boolean;
  min_port: number;
  max_port: number;
}

interface RqbitDesktopConfigPersistence {
  disable: boolean;
  filename: PathLike;
}

interface RqbitDesktopConfigPeerOpts {
  connect_timeout: Duration;
  read_write_timeout: Duration;
}

interface RqbitDesktopConfigHttpApi {
  disable: boolean;
  listen_addr: SocketAddr;
  read_only: boolean;
  cors_enable_all: boolean;
}

interface RqbitDesktopConfigUpnp {
  disable: boolean;
}

export interface RqbitDesktopConfig {
  default_download_location: PathLike;
  dht: RqbitDesktopConfigDht;
  tcp_listen: RqbitDesktopConfigTcpListen;
  upnp: RqbitDesktopConfigUpnp;
  persistence: RqbitDesktopConfigPersistence;
  peer_opts: RqbitDesktopConfigPeerOpts;
  http_api: RqbitDesktopConfigHttpApi;
}

export interface CurrentDesktopState {
  config: RqbitDesktopConfig | null;
  configured: boolean;
}
