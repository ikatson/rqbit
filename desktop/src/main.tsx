import { StrictMode } from "react";
import ReactDOM from "react-dom/client";
import { APIContext } from "./rqbit-webui-src/rqbit-web";
import { API } from "./api";
import { invoke } from "@tauri-apps/api";
import { CurrentDesktopState, RqbitDesktopConfig } from "./configuration";
import { RqbitDesktop } from "./rqbit-desktop";

async function get_version(): Promise<string> {
  return invoke<string>("get_version");
}

async function get_default_config(): Promise<RqbitDesktopConfig> {
  return invoke<RqbitDesktopConfig>("config_default");
}

async function get_current_config(): Promise<CurrentDesktopState> {
  return invoke<CurrentDesktopState>("config_current");
}

Promise.all([get_version(), get_default_config(), get_current_config()]).then(
  ([version, defaultConfig, currentState]) => {
    ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
      <StrictMode>
        <APIContext.Provider value={API}>
          <RqbitDesktop
            version={version}
            defaultConfig={defaultConfig}
            currentState={currentState}
          />
        </APIContext.Provider>
      </StrictMode>
    );
  }
);
