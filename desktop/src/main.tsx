import { StrictMode, useState } from "react";
import ReactDOM from 'react-dom/client';
import { APIContext, RqbitWebUI } from "./rqbit-webui-src/rqbit-web";
import { API } from "./api";
import { invoke } from "@tauri-apps/api";
import { RqbitDesktopConfig } from "./configuration";
import { RqbitDesktop } from "./rqbit-desktop";

async function get_version(): Promise<string> {
    return invoke<string>("get_version");
}

async function get_default_config(): Promise<RqbitDesktopConfig> {
    return invoke<RqbitDesktopConfig>("config_default");
}

Promise.all([get_version(), get_default_config()]).then(([version, config]) => {
    ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
        <StrictMode>
            <APIContext.Provider value={API}>
                <RqbitDesktop version={version} defaultConfig={config} />
            </APIContext.Provider>
        </StrictMode>
    );
})