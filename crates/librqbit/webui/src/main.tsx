import { StrictMode, useEffect, useState } from "react";
import ReactDOM from 'react-dom/client';
import { RqbitWebUI, APIContext, customSetInterval } from "./rqbit-web";
import { API } from "./http-api";

const RootWithVersion = () => {
    let [title, setTitle] = useState<string>("rqbit web UI");
    useEffect(() => {
        const refreshVersion = () => API.getVersion().then((version) => {
            setTitle(`rqbit web UI - v${version}`);
            return 10000;
        }, (e) => {
            return 1000;
        });
        return customSetInterval(refreshVersion, 0)
    }, [])

    return <StrictMode>
        <APIContext.Provider value={API}>
            <RqbitWebUI title={title} />
        </APIContext.Provider>
    </StrictMode>;
}

ReactDOM.createRoot(document.getElementById('app') as HTMLInputElement).render(
    <RootWithVersion />
);
