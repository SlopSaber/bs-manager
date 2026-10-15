import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { IpcStreamMessage } from "shared/models/ipc/ipc-stream-message.interface";

type Listener = (...args: unknown[]) => void;

const subscriptions = new Map<string, Map<Listener, Promise<UnlistenFn>>>();
const pathSeparator = "\\";

function listenTo(channel: string, listener: Listener, once = false): void {
    let channelListeners = subscriptions.get(channel);
    if (!channelListeners) {
        channelListeners = new Map();
        subscriptions.set(channel, channelListeners);
    }

    const subscription = listen<unknown>(channel, event => {
        listener(event.payload);
        if (once) void unlisten(channel, listener);
    });
    channelListeners.set(listener, subscription);
}

async function unlisten(channel: string, listener: Listener): Promise<void> {
    const channelListeners = subscriptions.get(channel);
    const subscription = channelListeners?.get(listener);
    if (!subscription) return;

    channelListeners.delete(listener);
    if (channelListeners.size === 0) subscriptions.delete(channel);
    await (await subscription)();
}

function basename(value: string): string {
    if (!value) return "";
    return value.replace(/[\\/]+$/, "").split(/[\\/]/).at(-1) ?? "";
}

function join(...parts: string[]): string {
    const input = parts.filter(Boolean).join(pathSeparator);
    const rooted = input.startsWith(pathSeparator);
    const drive = input.match(/^[A-Za-z]:/);
    const segments: string[] = [];

    for (const segment of input.split(/[\\/]+/)) {
        if (!segment || /^[A-Za-z]:$/.test(segment) || segment === ".") continue;
        if (segment === ".." && segments.length && segments.at(-1) !== "..") {
            segments.pop();
        } else if (segment !== ".." || !drive) {
            segments.push(segment);
        }
    }

    const prefix = drive?.[0] ?? (rooted ? pathSeparator : "");
    return `${prefix}${prefix && segments.length ? pathSeparator : ""}${segments.join(pathSeparator)}`;
}

window.desktop = {
    platform: "win32",
    ipcRenderer: {
        request(requestId, channel, args, onEvent) {
            const response = new Channel<IpcStreamMessage>();
            response.onmessage = onEvent;
            return invoke<void>("request", { requestId, channel, args: args ?? null, onEvent: response });
        },
        sendLazy(channel, args) {
            void invoke("send_lazy", { channel, args: args ?? null }).catch(console.error);
        },
        cancelRequest(requestId) {
            return invoke<void>("cancel_request", { requestId });
        },
        on(channel, listener) {
            listenTo(channel, listener);
            return () => void unlisten(channel, listener);
        },
        once(channel, listener) {
            listenTo(channel, listener, true);
        },
        removeAllListeners(channel) {
            const channelListeners = subscriptions.get(channel);
            if (!channelListeners) return;
            subscriptions.delete(channel);
            for (const subscription of channelListeners.values()) {
                void subscription.then(unlisten => unlisten());
            }
        },
    },
    path: { sep: pathSeparator, basename, join },
    webUtils: {
        getPathForFile(file) {
            return (file as File & { path?: string }).path ?? "";
        },
    },
};

void getCurrentWebview().onDragDropEvent(event => {
    window.dispatchEvent(new CustomEvent("desktop-files-dropped", { detail: event.payload }));
});
