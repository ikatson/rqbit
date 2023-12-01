import { StrictMode } from "react";
import ReactDOM from 'react-dom/client';
import { RqbitWebUI } from "./rqbit-web";
import { API } from "./api";

globalThis.API = API;

const torrentsContainer = document.getElementById('app');
ReactDOM.createRoot(torrentsContainer).render(<StrictMode><RqbitWebUI /></StrictMode >);
