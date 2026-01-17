// Mock entry point for testing UI with large number of torrents
// Run with: npm run dev:mock

import { StrictMode } from "react";
import ReactDOM from "react-dom/client";
import { RqbitWebUI } from "./rqbit-web";
import { APIContext } from "./context";
import { MockAPI } from "./mock-api";
import "./globals.css";

const RootWithMockAPI = () => {
  return (
    <APIContext.Provider value={MockAPI}>
      <RqbitWebUI title="rqbit (MOCK)" version="mock-1.0.0" />
    </APIContext.Provider>
  );
};

ReactDOM.createRoot(document.getElementById("app") as HTMLInputElement).render(
  <StrictMode>
    <RootWithMockAPI />
  </StrictMode>,
);
