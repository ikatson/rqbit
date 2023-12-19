import { StrictMode, useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { RqbitWebUI } from "./rqbit-web";
import { customSetInterval } from "./helper/customSetInterval";
import { APIContext } from "./context";
import { API } from "./http-api";
import "./globals.css";

const RootWithVersion = () => {
  let [title, setTitle] = useState<string>("rqbit web UI");
  useEffect(() => {
    const refreshVersion = () =>
      API.getVersion().then(
        (version) => {
          const title = `rqbit web UI - v${version}`;
          setTitle(title);
          document.title = title;
          return 10000;
        },
        (e) => {
          return 1000;
        }
      );
    return customSetInterval(refreshVersion, 0);
  }, []);

  return (
    <APIContext.Provider value={API}>
      <RqbitWebUI title={title} />
    </APIContext.Provider>
  );
};

ReactDOM.createRoot(document.getElementById("app") as HTMLInputElement).render(
  <StrictMode>
    <RootWithVersion />
  </StrictMode>
);
