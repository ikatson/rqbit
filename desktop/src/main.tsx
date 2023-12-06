import { StrictMode } from "react";
import ReactDOM from 'react-dom/client';
import { APIContext, RqbitWebUI } from "./rqbit-webui-src/rqbit-web";
import { API } from "./api";

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
    <StrictMode>
        <APIContext.Provider value={API}>
            <RqbitWebUI title="Rqbit Desktop v5.0.0-beta.0" />
        </APIContext.Provider>
    </StrictMode>
);


