import "./fonts.css";
import "./app.css";
import { mount } from "svelte";
import App from "./App.svelte";

const app = mount(App, {
  target: document.getElementById("app")!,
});

document.getElementById("boot")?.remove();

export default app;
