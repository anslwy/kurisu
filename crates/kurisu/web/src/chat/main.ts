import "./chat.css";
import { mount } from "svelte";
import Chat from "./Chat.svelte";

mount(Chat, { target: document.getElementById("app")! });
