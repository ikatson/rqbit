import { useState } from "react";
import { RqbitWebUI } from "./rqbit-webui-src/rqbit-web";
import { CurrentDesktopState, RqbitDesktopConfig } from "./configuration";
import { ConfigModal } from "./configure";
import { IconButton } from "./rqbit-webui-src/components/IconButton";
import { BsSliders2 } from "react-icons/bs";

export const RqbitDesktop: React.FC<{
  version: string;
  defaultConfig: RqbitDesktopConfig;
  currentState: CurrentDesktopState;
}> = ({ version, defaultConfig, currentState }) => {
  let [configured, setConfigured] = useState<boolean>(currentState.configured);
  let [config, setConfig] = useState<RqbitDesktopConfig>(
    currentState.config ?? defaultConfig
  );
  let [configurationOpened, setConfigurationOpened] = useState<boolean>(false);

  return (
    <>
      {configured && (
        <RqbitWebUI title={`Rqbit Desktop v${version}`}></RqbitWebUI>
      )}
      {configured && (
        <IconButton
          className="position-absolute top-0 start-0 p-3 text-primary"
          onClick={() => {
            setConfigurationOpened(true);
          }}
        >
          <BsSliders2 />
        </IconButton>
      )}
      <ConfigModal
        show={!configured || configurationOpened}
        handleStartReconfigure={() => {
          setConfigured(false);
        }}
        handleCancel={() => {
          setConfigurationOpened(false);
        }}
        handleConfigured={(config) => {
          setConfig(config);
          setConfigurationOpened(false);
          setConfigured(true);
        }}
        initialConfig={config}
        defaultConfig={defaultConfig}
      />
    </>
  );
};
