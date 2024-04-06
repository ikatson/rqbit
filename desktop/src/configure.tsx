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
  | "Peer options"
  | "HTTP API"
  | "TCP Listen";

const TABS: readonly TAB[] = [
  "Home",
  "DHT",
  "Session",
  "TCP Listen",
  "Peer options",
  "HTTP API",
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

        <Tab name="TCP Listen" currentTab={tab}>
          <Fieldset>
            <FormCheck
              label="Listen on TCP"
              name="tcp_listen.disable"
              checked={!config.tcp_listen.disable}
              onChange={handleToggleChange}
              help="Listen for torrent requests on TCP. Required for peers to be able to connect to you, mainly for uploading."
            />

            <FormCheck
              label="Advertise over UPnP"
              name="tcp_listen.disable"
              checked={!config.tcp_listen.disable}
              onChange={handleToggleChange}
              help="Advertise your port over UPnP. This is required for peers to be able to connect to you from the internet. Will only work if your router has a static IP."
            />

            <FormInput
              inputType="number"
              label="Min port"
              name="tcp_listen.min_port"
              value={config.tcp_listen.min_port}
              disabled={config.tcp_listen.disable}
              onChange={handleInputChange}
              help="The min port to try to listen on. First successful is taken."
            />

            <FormInput
              inputType="number"
              label="Max port"
              name="tcp_listen.max_port"
              value={config.tcp_listen.max_port}
              disabled={config.tcp_listen.disable}
              onChange={handleInputChange}
              help="The max port to try to listen on."
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
              label="Persistence filename"
              name="persistence.filename"
              inputType="text"
              value={config.persistence.filename}
              onChange={handleInputChange}
              disabled={config.persistence.disable}
            />
          </Fieldset>
        </Tab>

        <Tab name="Peer options" currentTab={tab}>
          <Fieldset>
            <FormInput
              label="Connect timeout (seconds)"
              inputType="number"
              name="peer_opts.connect_timeout"
              value={config.peer_opts.connect_timeout}
              onChange={handleInputChange}
              help="How much to wait for outgoing connections to connect. Default is low to prefer faster peers."
            />

            <FormInput
              label="Read/write timeout (seconds)"
              inputType="number"
              name="peer_opts.read_write_timeout"
              value={config.peer_opts.read_write_timeout}
              onChange={handleInputChange}
              help="Peer socket read/write timeout."
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
