import { StrictMode } from "react";
import ReactDOM from 'react-dom/client';
import { APIContext, RqbitWebUI } from "./rqbit-webui-src/rqbit-web";
import { API } from "./api";
import { invoke } from "@tauri-apps/api";

let version = invoke<string>("get_version").then((version) => {
    ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
        <StrictMode>
            <APIContext.Provider value={API}>
                <RqbitWebUI title={`Rqbit Desktop v${version}`} />
            </APIContext.Provider>
        </StrictMode>
    );
});




