import { StrictMode } from "react";
import ReactDOM from 'react-dom/client';
import { RqbitWebUI } from "./rqbit-web";
import { API } from "./http-api";

globalThis.API = API;

const torrentsContainer = document.getElementById('app') as HTMLInputElement;
ReactDOM.createRoot(torrentsContainer).render(<StrictMode><RqbitWebUI /></StrictMode >);
