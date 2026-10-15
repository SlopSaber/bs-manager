import type { IpcStreamMessage } from "shared/models/ipc/ipc-stream-message.interface";

declare global {
    interface Window {
        desktop: {
            platform: "win32";
            ipcRenderer: {
                request(requestId: string, channel: string, args: unknown, onEvent: (message: IpcStreamMessage) => void): Promise<void>;
                sendLazy(channel: string, args?: unknown): void;
                cancelRequest(requestId: string): Promise<void>;
                on(channel: string, func: (...args: unknown[]) => void): (() => void) | undefined;
                once(channel: string, func: (...args: unknown[]) => void): void;
                removeAllListeners(channel: string): void;
            };
            path: {
                sep: "\\";
                basename(path: string): string;
                join(...parts: string[]): string;
            };
            webUtils: {
                getPathForFile(file: File): string;
            };
        };
    }
}

export {};
