import { StrictMode } from "react";
import ReactDOM from 'react-dom/client';
import { RqbitWebUI, APIContext } from "./rqbit-web";
import { API } from "./http-api";

ReactDOM.createRoot(document.getElementById('app') as HTMLInputElement).render(
    <StrictMode>
        <APIContext.Provider value={API}>
            <RqbitWebUI title="rqbit web UI - v5.0.0-beta.0" />
        </APIContext.Provider>
    </StrictMode>
);
