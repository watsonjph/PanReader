import { mount } from "svelte";
import "./themes.css";
import "./tokens.css";
import App from "./App.svelte";

export default mount(App, { target: document.getElementById("app") });
