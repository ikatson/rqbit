import React, { ReactNode, useState } from "react";
import { RqbitDesktopConfig } from "./configuration";
import { ErrorComponent } from "rqbit-webui/src/components/ErrorComponent";
import { invokeAPI } from "./api";
import { ErrorDetails } from "rqbit-webui/src/api-types";
import { FormCheckbox } from "rqbit-webui/src/components/forms/FormCheckbox";
import { FormInput as FI } from "rqbit-webui/src/components/forms/FormInput";
import { ModalBody } from "rqbit-webui/src/components/modal/ModalBody";
import { Modal } from "rqbit-webui/src/components/modal/Modal";
import { Fieldset } from "rqbit-webui/src/components/forms/Fieldset";
import { ModalFooter } from "rqbit-webui/src/components/modal/ModalFooter";
import { Button } from "rqbit-webui/src/components/buttons/Button";
import { formatBytes } from "rqbit-webui/src/helper/formatBytes";

const FormCheck: React.FC<{
  label: string;
  name: string;
  checked: boolean;
  onChange: React.ChangeEventHandler<HTMLInputElement>;
  disabled?: boolean;
  help?: string;
}> = ({ label, name, checked, onChange, disabled, help }) => {
  return (
    <FormCheckbox
      label={label}
      name={name}
      checked={checked}
      onChange={onChange}
      disabled={disabled}
      help={help}
    />
  );
};

const FormInput: React.FC<{
  label: string;
  name: string;
  value: string | number;
  inputType: string;
  onChange: React.ChangeEventHandler<HTMLInputElement>;
  disabled?: boolean;
  help?: string;
}> = ({ label, name, value, inputType, onChange, disabled, help }) => {
  return (
    <FI
      inputType={inputType}
      name={name}
      value={value as string}
      onChange={onChange}
      disabled={disabled}
      label={label}
      help={help}
    />
  );
};

type TAB =
  | "Home"
  | "DHT"
  | "Session"
  | "HTTP API"
  | "Connection"
  | "UPnP Server";

const TABS: readonly TAB[] = [
  "Home",
  "DHT",
  "Session",
  "Connection",
  "HTTP API",
  "UPnP Server",
] as const;

const Tab: React.FC<{
  name: TAB;
  currentTab: TAB;
  children: ReactNode;
}> = ({ name, currentTab, children }) => {
  const show = name === currentTab;
  if (!show) {
    return;
  }
  return <div>{children}</div>;
};

export const ConfigModal: React.FC<{
  show: boolean;
  handleStartReconfigure: () => void;
  handleConfigured: (config: RqbitDesktopConfig) => void;
  handleCancel?: () => void;
  initialConfig: RqbitDesktopConfig;
  defaultConfig: RqbitDesktopConfig;
}> = ({
  show,
  handleStartReconfigure,
  handleConfigured,
  handleCancel,
  initialConfig,
  defaultConfig,
}) => {
  let [config, setConfig] = useState<RqbitDesktopConfig>(initialConfig);
  let [loading, setLoading] = useState<boolean>(false);

  let [tab, setTab] = useState<TAB>("Home");

  const [error, setError] = useState<any | null>(null);

  const handleInputChange: React.ChangeEventHandler<HTMLInputElement> = (e) => {
    const name: string = e.target.name;
    let value: string | number = e.target.value;
    if (e.target.type == "number") {
      value = e.target.valueAsNumber;
    }
    console.log(value, typeof value);
    const [mainField, subField] = name.split(".", 2);

    if (subField) {
      setConfig((prevConfig: any) => ({
        ...prevConfig,
        [mainField]: {
          ...prevConfig[mainField],
          [subField]: value,
        },
      }));
    } else {
      setConfig((prevConfig) => ({
        ...prevConfig,
        [name]: value,
      }));
    }
  };

  const handleToggleChange: React.ChangeEventHandler<HTMLInputElement> = (
    e,
  ) => {
    const name: string = e.target.name;
    const [mainField, subField] = name.split(".", 2);

    if (subField) {
      setConfig((prevConfig: any) => ({
        ...prevConfig,
        [mainField]: {
          ...prevConfig[mainField],
          [subField]: !prevConfig[mainField][subField],
        },
      }));
    } else {
      setConfig((prevConfig: any) => ({
        ...prevConfig,
        [name]: !prevConfig[name],
      }));
    }
  };

  const handleOkClick = () => {
    setError(null);
    handleStartReconfigure();
    setLoading(true);
    invokeAPI<{}>("config_change", { config }).then(
      () => {
        setLoading(false);
        handleConfigured(config);
      },
      (e: ErrorDetails) => {
        setLoading(false);
        setError({
          text: "Error saving configuration",
          details: e,
        });
      },
    );
  };

  return (
    <Modal
      title="Configure Rqbit desktop"
      isOpen={show}
      onClose={handleCancel}
      className="max-w-4xl"
    >
      <ModalBody>
        <ErrorComponent error={error}></ErrorComponent>
        <div className="mb-4 flex border-b">
          {TABS.map((t, i) => {
            const isActive = t === tab;
            let classNames = "text-slate-300";
            if (isActive) {
              classNames =
                "text-slate-800 border-b-2 border-blue-800 dark:border-blue-200 dark:text-white";
            }
            return (
              <button
                key={i}
                className={`p-2 ${classNames}`}
                onClick={() => setTab(t)}
              >
                {t}
              </button>
            );
          })}
        </div>

        <Tab name="Home" currentTab={tab}>
          <FormInput
            label="Default download folder"
            name="default_download_location"
            value={config.default_download_location}
            inputType="text"
            onChange={handleInputChange}
            help="Where to download torrents by default. You can override this per torrent."
          />

          {defaultConfig.disable_upload !== undefined &&
            config.disable_upload !== undefined && (
              <FormCheck
                label="Disable upload"
                name="disable_upload"
                checked={config.disable_upload}
                onChange={handleToggleChange}
                help="Disable uploading entirely. If this is set, rqbit won't share piece availability and will disconnect on download request.

Might be useful e.g. if rqbit upload consumes all your upload bandwidth and interferes with your other Internet usage."
              />
            )}
        </Tab>

        <Tab name="DHT" currentTab={tab}>
          <Fieldset>
            <FormCheck
              label="Enable DHT"
              name="dht.disable"
              checked={!config.dht.disable}
              onChange={handleToggleChange}
              help="DHT is required to read magnet links. There's no good reason to disable it, unless you know what you are doing."
            />

            <FormCheck
              label="Enable DHT persistence"
              name="dht.disable_persistence"
              checked={!config.dht.disable_persistence}
              onChange={handleToggleChange}
              disabled={config.dht.disable}
              help="Enable to store DHT state in a file periodically. If disabled, DHT will bootstrap from scratch on restart."
            />

            <FormInput
              label="Persistence filename"
              name="dht.persistence_filename"
              value={config.dht.persistence_filename}
              inputType="text"
              disabled={config.dht.disable}
              onChange={handleInputChange}
              help="The filename to store DHT state into"
            />
          </Fieldset>
        </Tab>

        <Tab name="Connection" currentTab={tab}>
          <Fieldset>
            <FormCheck
              label="Listen on TCP"
              name="connections.enable_tcp_listen"
              checked={config.connections.enable_tcp_listen}
              onChange={handleToggleChange}
              help="Listen for torrent requests on TCP. Required for peers to be able to connect to you, mainly for uploading."
            />

            <FormCheck
              label="Listen on uTP (over UDP)"
              name="connections.enable_utp"
              checked={config.connections.enable_utp}
              onChange={handleToggleChange}
              help="Listen for torrent requests on uTP over UDP. Required for uTP support in general, both outgoing and incoming."
            />

            <FormCheck
              label="Advertise port over UPnP"
              name="connections.enable_upnp_port_forward"
              checked={config.connections.enable_upnp_port_forward}
              onChange={handleToggleChange}
              help="Advertise your port over UPnP to your router(s). This is required for peers to be able to connect to you from the internet. Will only work if your router has a static IP."
            />

            <FormCheck
              label="[ADVANCED] Disable outgoing connections over TCP"
              name="connections.enable_tcp_outgoing"
              checked={!config.connections.enable_tcp_outgoing}
              onChange={handleToggleChange}
              help="WARNING: leave this unchecked unless you know what you are doing."
            />

            <FormInput
              inputType="text"
              label="Socks proxy"
              name="connections.socks_proxy"
              value={config.connections.socks_proxy}
              onChange={handleInputChange}
              help="Socks5 proxy for outgoing connections. Format: socks5://[username:password@]host:port"
            />

            <FormInput
              inputType="number"
              label="Port"
              name="connections.listen_port"
              value={config.connections.listen_port}
              disabled={
                !config.connections.enable_tcp_listen &&
                !config.connections.enable_utp
              }
              onChange={handleInputChange}
              help="The port to listen on for both TCP and UDP (if enabled)."
            />

            <FormInput
              label="Peer connect timeout (seconds)"
              inputType="number"
              name="connections.peer_connect_timeout"
              value={config.connections.peer_connect_timeout}
              onChange={handleInputChange}
              help="How much to wait for outgoing connections to connect. Default is low to prefer faster peers."
            />

            <FormInput
              label="Peer read/write timeout (seconds)"
              inputType="number"
              name="connections.peer_read_write_timeout"
              value={config.connections.peer_read_write_timeout}
              onChange={handleInputChange}
              help="Peer socket read/write timeout."
            />
          </Fieldset>
        </Tab>

        <Tab name="UPnP Server" currentTab={tab}>
          <Fieldset>
            <FormCheck
              label="Enable UPnP media server"
              name="upnp.enable_server"
              checked={config.upnp.enable_server}
              onChange={handleToggleChange}
              help="If enabled, rqbit will advertise the media to supported LAN devices, e.g. TVs."
            />

            <FormInput
              inputType="text"
              label="Friendly name"
              name="upnp.server_friendly_name"
              value={config.upnp.server_friendly_name}
              disabled={!config.upnp.enable_server}
              onChange={handleInputChange}
              help="The name displayed on supported devices. If not set will be generated, will look smth like <rqbit at HOSTNAME>."
            />
          </Fieldset>
        </Tab>

        <Tab name="Session" currentTab={tab}>
          <Fieldset>
            <FormCheck
              label="Enable persistence"
              name="persistence.disable"
              checked={!config.persistence.disable}
              onChange={handleToggleChange}
              help="If you disable session persistence, rqbit won't remember the torrents you had before restart."
            />

            <FormInput
              label="Persistence folder"
              name="persistence.folder"
              inputType="text"
              value={config.persistence.folder}
              onChange={handleInputChange}
              disabled={config.persistence.disable}
            />

            <FormCheck
              label="Enable fast resume (experimental)"
              name="persistence.fastresume"
              checked={config.persistence.fastresume}
              onChange={handleToggleChange}
              help="If enabled, restarting will not rehash torrents, and thus will be faster. You should not modify the downloaded files in any way if you use that."
            />

            <FormInput
              label="Download rate limit"
              name="ratelimits.download_bps"
              inputType="number"
              value={config.ratelimits.download_bps ?? ""}
              onChange={handleInputChange}
              help={`Limit total download speed to this number of bytes per second (${
                (config.ratelimits.download_bps ?? 0) > 0
                  ? "current " +
                    formatBytes(config.ratelimits.download_bps ?? 0) +
                    " per second"
                  : "currently disabled"
              })`}
            />

            <FormInput
              label="Upload rate limit"
              name="ratelimits.upload_bps"
              inputType="number"
              value={config.ratelimits.upload_bps ?? ""}
              onChange={handleInputChange}
              help={`Limit total upload speed to this number of bytes per second (${
                (config.ratelimits.upload_bps ?? 0) > 0
                  ? "current " +
                    formatBytes(config.ratelimits.upload_bps ?? 0) +
                    " per second"
                  : "currently disabled"
              })`}
            />
          </Fieldset>
        </Tab>

        <Tab name="HTTP API" currentTab={tab}>
          <Fieldset>
            <FormCheck
              label="Enable HTTP API"
              name="http_api.disable"
              checked={!config.http_api.disable}
              onChange={handleToggleChange}
              help="If enabled you can access the HTTP API at the address below"
            />

            <FormCheck
              label="Read only"
              name="http_api.read_only"
              checked={config.http_api.read_only}
              disabled={config.http_api.disable}
              onChange={handleToggleChange}
              help="If enabled, only GET requests will be allowed through the API"
            />

            <FormInput
              label="Listen address"
              inputType="text"
              name="http_api.listen_addr"
              value={config.http_api.listen_addr}
              disabled={config.http_api.disable}
              onChange={handleInputChange}
              help={`You'll access the API at http://${config.http_api.listen_addr}`}
            />
          </Fieldset>
        </Tab>
      </ModalBody>
      <ModalFooter>
        {!!handleCancel && (
          <Button variant="cancel" onClick={handleCancel}>
            Cancel
          </Button>
        )}
        <Button variant="secondary" onClick={() => setConfig(defaultConfig)}>
          Reset to defaults
        </Button>
        <Button variant="primary" onClick={handleOkClick} disabled={loading}>
          OK
        </Button>
      </ModalFooter>
    </Modal>
  );
};
