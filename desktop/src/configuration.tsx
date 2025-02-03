type PathLike = string;
type Duration = string;
type SocketAddr = string;

interface RqbitDesktopConfigDht {
  disable: boolean;
  disable_persistence: boolean;
  persistence_filename: PathLike;
}

interface RqbitDesktopConfigConnections {
  enable_tcp_listen: boolean;
  enable_tcp_outgoing: boolean;
  enable_utp: boolean;
  enable_upnp_port_forward: boolean;
  socks_proxy: string;
  listen_port: number;
  peer_connect_timeout: Duration;
  peer_read_write_timeout: Duration;
}

interface RqbitDesktopConfigPersistence {
  disable: boolean;
  folder: PathLike;
  fastresume: boolean;
}

interface RqbitDesktopConfigHttpApi {
  disable: boolean;
  listen_addr: SocketAddr;
  read_only: boolean;
  cors_enable_all: boolean;
}

interface RqbitDesktopConfigUpnp {
  disable: boolean;

  enable_server: boolean;
  server_friendly_name: string;
}

export interface LimitsConfig {
  upload_bps?: number | null;
  download_bps?: number | null;
}

export interface RqbitDesktopConfig {
  default_download_location: PathLike;
  disable_upload?: boolean;
  dht: RqbitDesktopConfigDht;
  connections: RqbitDesktopConfigConnections;
  upnp: RqbitDesktopConfigUpnp;
  persistence: RqbitDesktopConfigPersistence;
  http_api: RqbitDesktopConfigHttpApi;
  ratelimits: LimitsConfig;
}

export interface CurrentDesktopState {
  config: RqbitDesktopConfig | null;
  configured: boolean;
}
